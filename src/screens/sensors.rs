//! Sensor readings: CPU temperature from hwmon, GPU temperature from NVML,
//! CPU load and memory from `/proc`, GPU load and power from NVML, and a
//! short temperature history.
//!
//! Sensors that are missing or fail (driver loaded late, NVML stale after
//! resume) are looked up again, at most once per `RETRY`.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

const RETRY: Duration = Duration::from_secs(30);
/// Temperatures change slowly; querying NVML is the costliest thing we do.
const POLL: Duration = Duration::from_secs(3);
/// How far back the temperature history goes.
pub const HISTORY: Duration = Duration::from_secs(10 * 60);

/// Everything read in one poll.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Readings {
    /// °C.
    pub cpu_temp: Option<f32>,
    pub gpu_temp: Option<f32>,
    /// Fraction of the time busy since the last poll, 0..=1.
    pub cpu_load: Option<f32>,
    pub gpu_load: Option<f32>,
    /// Used and total bytes.
    pub mem: Option<(u64, u64)>,
    /// Watts.
    pub gpu_power: Option<f32>,
}

/// One history sample: when, and the CPU and GPU temperature then.
pub type Sample = (Instant, Option<f32>, Option<f32>);

pub struct Sensors {
    cached: Readings,
    polled_at: Option<Instant>,
    cpu_path: Option<PathBuf>,
    nvml: Option<Nvml>,
    cpu_retry_at: Instant,
    gpu_retry_at: Instant,
    /// Busy and total CPU time at the last poll, for the load since.
    cpu_times: Option<(u64, u64)>,
    history: VecDeque<Sample>,
}

impl Sensors {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            cached: Readings::default(),
            polled_at: None,
            cpu_path: None,
            nvml: None,
            cpu_retry_at: now,
            gpu_retry_at: now,
            cpu_times: None,
            history: VecDeque::new(),
        }
    }

    /// CPU and GPU temperature in °C, refreshed at most every `POLL`.
    pub fn temps(&mut self) -> (Option<f32>, Option<f32>) {
        let r = self.readings();
        (r.cpu_temp, r.gpu_temp)
    }

    /// The latest readings, refreshed at most every `POLL`.
    pub fn readings(&mut self) -> Readings {
        if self.polled_at.is_none_or(|t| t.elapsed() >= POLL) {
            self.poll();
        }
        self.cached
    }

    /// Temperatures over the last `HISTORY`, oldest first, for the times
    /// something asked for readings.
    pub fn history(&self) -> &VecDeque<Sample> {
        &self.history
    }

    fn poll(&mut self) {
        let now = Instant::now();
        let mut r = Readings {
            cpu_temp: self.cpu_temp(),
            gpu_temp: self.gpu_temp(),
            cpu_load: self.cpu_load(),
            mem: read_meminfo(),
            ..Readings::default()
        };
        self.gpu_details(&mut r);
        self.history.push_back((now, r.cpu_temp, r.gpu_temp));
        while self
            .history
            .front()
            .is_some_and(|(t, ..)| now.duration_since(*t) > HISTORY)
        {
            self.history.pop_front();
        }
        self.cached = r;
        self.polled_at = Some(now);
    }

    /// CPU package temperature in °C.
    fn cpu_temp(&mut self) -> Option<f32> {
        if self.cpu_path.is_none() && due(&mut self.cpu_retry_at) {
            self.cpu_path = find_cpu_temp();
        }
        let value = read_millidegrees(self.cpu_path.as_ref()?);
        if value.is_none() {
            self.cpu_path = None;
        }
        value
    }

    /// Temperature of the first NVIDIA GPU in °C.
    fn gpu_temp(&mut self) -> Option<f32> {
        if self.nvml.is_none() && due(&mut self.gpu_retry_at) {
            self.nvml = Nvml::init().ok();
        }
        let value = self
            .nvml
            .as_ref()?
            .device_by_index(0)
            .and_then(|gpu| gpu.temperature(TemperatureSensor::Gpu))
            .ok();
        if value.is_none() {
            self.nvml = None;
        }
        value.map(|t| t as f32)
    }

    /// GPU load and power.
    fn gpu_details(&self, r: &mut Readings) {
        let Some(gpu) = self.nvml.as_ref().and_then(|n| n.device_by_index(0).ok()) else {
            return;
        };
        r.gpu_load = gpu.utilization_rates().ok().map(|u| u.gpu as f32 / 100.0);
        r.gpu_power = gpu.power_usage().ok().map(|mw| mw as f32 / 1000.0);
    }

    /// Fraction of CPU time busy since the last poll; none on the first.
    fn cpu_load(&mut self) -> Option<f32> {
        let now = read_cpu_times();
        let prev = std::mem::replace(&mut self.cpu_times, now);
        let ((busy, total), (busy0, total0)) = (now?, prev?);
        let dt = total.checked_sub(total0).filter(|&d| d > 0)?;
        Some((busy.saturating_sub(busy0) as f32 / dt as f32).clamp(0.0, 1.0))
    }
}

/// True (and schedules the next retry) when `retry_at` has passed.
fn due(retry_at: &mut Instant) -> bool {
    let now = Instant::now();
    if now < *retry_at {
        return false;
    }
    *retry_at = now + RETRY;
    true
}

fn read_millidegrees(path: &PathBuf) -> Option<f32> {
    let raw = fs::read_to_string(path).ok()?;
    Some(raw.trim().parse::<f32>().ok()? / 1000.0)
}

/// Busy and total jiffies across all CPUs, from `/proc/stat`.
fn read_cpu_times() -> Option<(u64, u64)> {
    parse_cpu_times(&fs::read_to_string("/proc/stat").ok()?)
}

fn parse_cpu_times(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    // user nice system idle iowait irq softirq steal; guest time is
    // already counted in user and nice.
    let total: u64 = fields.iter().take(8).sum();
    let idle = fields.get(3)? + fields.get(4).unwrap_or(&0);
    Some((total - idle, total))
}

/// Used and total memory in bytes, from `/proc/meminfo`.
fn read_meminfo() -> Option<(u64, u64)> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
}

fn parse_meminfo(info: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = info.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line[name.len()..].split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    };
    let (total, available) = (field("MemTotal:")?, field("MemAvailable:")?);
    Some((total.saturating_sub(available), total))
}

/// AMD: k10temp's Tdie when present (older Ryzens offset Tctl), else Tctl.
/// Intel: coretemp's package temperature (temp1).
fn find_cpu_temp() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let dir = entry.path();
        let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
        if !matches!(name.trim(), "k10temp" | "coretemp" | "zenpower") {
            continue;
        }
        let tdie = (1..=10)
            .map(|i| dir.join(format!("temp{i}_label")))
            .find(|label| fs::read_to_string(label).is_ok_and(|l| l.trim() == "Tdie"));
        return Some(match tdie {
            Some(label) => PathBuf::from(label.to_string_lossy().replace("_label", "_input")),
            None => dir.join("temp1_input"),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat() {
        let stat = "cpu  100 5 50 800 20 3 2 0 40 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
        // Busy: user+nice+system+irq+softirq+steal; idle+iowait aren't.
        assert_eq!(parse_cpu_times(stat), Some((160, 980)));
        assert_eq!(parse_cpu_times("intr 1 2 3"), None);
    }

    #[test]
    fn parses_meminfo() {
        let info = "MemTotal:       32000000 kB\nMemFree:  1000 kB\nMemAvailable:   20000000 kB\n";
        assert_eq!(
            parse_meminfo(info),
            Some((12_000_000 * 1024, 32_000_000 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 5 kB\n"), None);
    }
}
