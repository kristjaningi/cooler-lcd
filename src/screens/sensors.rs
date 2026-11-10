//! Sensor readings: CPU temperature from hwmon, GPU temperature from NVML,
//! CPU load and memory from `/proc`, GPU load and power from NVML, and a
//! short temperature history.
//!
//! Sensors that are missing or fail (driver loaded late, NVML stale after
//! resume) are looked up again, at most once per `RETRY`.
//!
//! The reading happens on a thread of its own, so a driver call that
//! stalls (NVML after a resume, say) can't freeze the frame loop: asking
//! for readings returns the latest at once, and starts a fresh poll when
//! they're due. Nothing is polled while nobody asks.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

use crate::clock::Stamp;

const RETRY: Duration = Duration::from_secs(30);
/// Temperatures change slowly; querying NVML is the costliest thing we do.
const POLL: Duration = Duration::from_secs(3);
/// Five missed polls: old readings must not look live after a driver stalls.
const STALE: Duration = Duration::from_secs(15);
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

/// One history sample: when, and the CPU and GPU temperature then. Timed
/// by a clock that counts through suspend, or last night's temperatures
/// would chart as the last ten minutes on waking.
pub type Sample = (Stamp, Option<f32>, Option<f32>);

/// The readings as the polling thread left them.
#[derive(Default)]
struct Latest {
    readings: Readings,
    history: VecDeque<Sample>,
    /// When the last completed poll began; none before the first completes.
    polled_at: Option<Instant>,
    /// When the poll that hasn't finished was asked for.
    asked: Option<Instant>,
}

impl Latest {
    /// The readings to show. Nothing polls while nothing asks, so age alone
    /// means nothing: after time off screen these are the last ones until
    /// the new poll is in. They're unavailable only once a poll has gone
    /// unanswered for `STALE`.
    fn shown(&self) -> Readings {
        if self.asked.is_some_and(|t| t.elapsed() >= STALE) {
            Readings::default()
        } else {
            self.readings
        }
    }
}

type Shared = Arc<(Mutex<Latest>, Condvar)>;

pub struct Sensors {
    shared: Shared,
}

impl Sensors {
    /// Starts the polling thread, and the first poll so the first frame
    /// has something to show.
    pub fn new() -> Self {
        let shared = Shared::default();
        shared.0.lock().unwrap().asked = Some(Instant::now());
        let s = shared.clone();
        thread::spawn(move || run(&s));
        Self { shared }
    }

    /// CPU and GPU temperature in °C, refreshed at most every `POLL`, for
    /// alerts: stricter than `readings`, since what the sensors said before
    /// polling went idle (the desktop away, say) mustn't take over the panel.
    pub fn temps(&mut self) -> (Option<f32>, Option<f32>) {
        // One lock throughout, so a poll finishing now can't vouch for the
        // sample before it.
        let l = self.latest();
        if l.polled_at.is_some_and(|t| t.elapsed() < STALE) {
            let r = l.shown();
            (r.cpu_temp, r.gpu_temp)
        } else {
            (None, None)
        }
    }

    /// The latest readings, starting a fresh poll if they're `POLL` old.
    pub fn readings(&mut self) -> Readings {
        self.latest().shown()
    }

    /// What the polling thread left, having asked for a fresh poll if due.
    fn latest(&self) -> MutexGuard<'_, Latest> {
        let (latest, wake) = &*self.shared;
        let mut l = latest.lock().unwrap();
        if l.asked.is_none() && l.polled_at.is_none_or(|t| t.elapsed() >= POLL) {
            l.asked = Some(Instant::now());
            wake.notify_one();
        }
        l
    }

    /// Calls `f` with the temperatures over the last `HISTORY`, oldest
    /// first, for the times something asked for readings.
    pub fn with_history<T>(&self, f: impl FnOnce(&VecDeque<Sample>) -> T) -> T {
        f(&self.shared.0.lock().unwrap().history)
    }

    /// Whether the first poll has finished.
    pub fn ready(&self) -> bool {
        self.shared.0.lock().unwrap().polled_at.is_some()
    }
}

/// The polling thread: one poll each time one is asked for.
fn run(shared: &Shared) {
    let (latest, wake) = &**shared;
    let mut probe = Probe::new();
    loop {
        drop(
            wake.wait_while(latest.lock().unwrap(), |l| l.asked.is_none())
                .unwrap(),
        );
        poll(shared, || probe.read());
    }
}

fn poll(shared: &Shared, read: impl FnOnce() -> Readings) {
    // A slow driver mustn't make readings collected earlier look fresh.
    let (started, sampled) = (Instant::now(), Stamp::now());
    let r = read();
    let (now, stamp) = (Instant::now(), Stamp::now());
    let mut l = shared.0.lock().unwrap();
    // It's still history, but too old by now to show as the latest.
    l.readings = if now.duration_since(started) < STALE {
        r
    } else {
        Readings::default()
    };
    l.history.push_back((sampled, r.cpu_temp, r.gpu_temp));
    while l
        .history
        .front()
        .is_some_and(|(t, ..)| stamp - *t > HISTORY)
    {
        l.history.pop_front();
    }
    l.polled_at = Some(started);
    l.asked = None;
}

/// The sensors themselves, read on the polling thread.
struct Probe {
    cpu_path: Option<PathBuf>,
    nvml: Option<Nvml>,
    cpu_retry_at: Instant,
    gpu_retry_at: Instant,
    /// Busy and total CPU time at the last poll, for the load since.
    cpu_times: Option<(u64, u64)>,
}

impl Probe {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            cpu_path: None,
            nvml: None,
            cpu_retry_at: now,
            gpu_retry_at: now,
            cpu_times: None,
        }
    }

    fn read(&mut self) -> Readings {
        let mut r = Readings {
            cpu_temp: self.cpu_temp(),
            gpu_temp: self.gpu_temp(),
            cpu_load: self.cpu_load(),
            mem: read_meminfo(),
            ..Readings::default()
        };
        self.gpu_details(&mut r);
        r
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
    fn a_slow_completed_poll_does_not_make_its_old_sample_fresh() {
        let shared = Shared::default();
        let mut sensors = Sensors {
            shared: shared.clone(),
        };
        poll(&shared, || {
            let readings = Readings {
                cpu_temp: Some(90.0),
                ..Readings::default()
            };
            // CPU temperature has been read; a later driver call stalls.
            thread::sleep(STALE);
            readings
        });
        assert_eq!(sensors.readings(), Readings::default());
        assert_eq!(sensors.temps(), (None, None));
        assert!(
            shared.0.lock().unwrap().asked.is_some(),
            "request a new sample"
        );
        sensors.with_history(|h| {
            let (at, cpu, _) = h.back().unwrap();
            assert!(at.elapsed() >= STALE, "history keeps acquisition time");
            assert_eq!(*cpu, Some(90.0));
        });

        poll(&shared, || Readings {
            cpu_temp: Some(50.0),
            ..Readings::default()
        });
        assert_eq!(sensors.temps(), (Some(50.0), None));
    }

    #[test]
    fn an_idle_gap_keeps_the_last_readings_on_screen_but_not_in_alerts() {
        let shared = Shared::default();
        let hot = Readings {
            cpu_temp: Some(90.0),
            gpu_temp: Some(50.0),
            ..Readings::default()
        };
        let mut sensors = Sensors {
            shared: shared.clone(),
        };
        {
            // Nothing has asked for a while: the desktop was away, say.
            let mut l = shared.0.lock().unwrap();
            l.readings = hot;
            l.polled_at = Some(Instant::now() - STALE * 10);
        }
        assert_eq!(sensors.temps(), (None, None), "too old to alert on");
        assert!(shared.0.lock().unwrap().asked.is_some(), "asks for a poll");
        assert_eq!(sensors.readings(), hot, "not blank while it's answered");

        poll(&shared, || Readings {
            cpu_temp: Some(50.0),
            ..Readings::default()
        });
        assert_eq!(sensors.temps(), (Some(50.0), None));
    }

    #[test]
    fn stalled_polls_hide_old_readings_and_recovery_restores_them() {
        let shared = Shared::default();
        let readings = Readings {
            cpu_temp: Some(90.0),
            gpu_temp: Some(50.0),
            cpu_load: Some(0.5),
            gpu_load: Some(0.3),
            mem: Some((10, 20)),
            gpu_power: Some(80.0),
        };
        let mut sensors = Sensors {
            shared: shared.clone(),
        };
        assert_eq!(
            sensors.readings(),
            Readings::default(),
            "no first sample yet"
        );
        {
            let mut l = shared.0.lock().unwrap();
            l.readings = readings;
            l.polled_at = Some(Instant::now());
            l.asked = None;
        }
        assert_eq!(sensors.readings(), readings);
        {
            let mut l = shared.0.lock().unwrap();
            l.polled_at = Some(Instant::now() - STALE);
            l.asked = Some(Instant::now() - STALE); // A driver call hasn't returned.
        }
        assert_eq!(sensors.readings(), Readings::default());
        assert_eq!(
            sensors.temps(),
            (None, None),
            "old temperatures cannot alert"
        );
        assert!(
            shared.0.lock().unwrap().asked.is_some(),
            "no second worker is started"
        );
        {
            let mut l = shared.0.lock().unwrap();
            l.polled_at = Some(Instant::now());
            l.asked = None;
        }
        assert_eq!(
            sensors.readings(),
            readings,
            "a completed poll restores readings"
        );
    }

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
