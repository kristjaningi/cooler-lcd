//! Sensor readings: CPU temperature from hwmon, GPU temperature from NVML.
//!
//! Sensors that are missing or fail (driver loaded late, NVML stale after
//! resume) are looked up again, at most once per `RETRY`.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

const RETRY: Duration = Duration::from_secs(30);
/// Temperatures change slowly; querying NVML is the costliest thing we do.
const POLL: Duration = Duration::from_secs(3);

pub struct Sensors {
    cached: (Option<f32>, Option<f32>),
    polled_at: Option<Instant>,
    cpu_path: Option<PathBuf>,
    nvml: Option<Nvml>,
    cpu_retry_at: Instant,
    gpu_retry_at: Instant,
}

impl Sensors {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            cached: (None, None),
            polled_at: None,
            cpu_path: None,
            nvml: None,
            cpu_retry_at: now,
            gpu_retry_at: now,
        }
    }

    /// CPU and GPU temperature in °C, refreshed at most every `POLL`.
    pub fn temps(&mut self) -> (Option<f32>, Option<f32>) {
        if self.polled_at.is_none_or(|t| t.elapsed() >= POLL) {
            self.cached = (self.cpu_temp(), self.gpu_temp());
            self.polled_at = Some(Instant::now());
        }
        self.cached
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
