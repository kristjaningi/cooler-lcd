//! Codex plan usage from its local session logs. Every reply Codex receives
//! records the account's rate limits in the session's `rollout-*.jsonl`, so
//! this needs no network: it re-reads the tail of the newest log when it
//! changes.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::Limit;

const CHECK: Duration = Duration::from_secs(30);
/// Rate-limit events are frequent, so the last one is near the end.
const TAIL_BYTES: u64 = 512 * 1024;

#[derive(Default)]
pub struct Codex {
    checked_at: Option<Instant>,
    seen: Option<(PathBuf, SystemTime)>,
    limits: Vec<Limit>,
}

impl Codex {
    /// The latest known limits; empty until Codex has been used.
    pub fn limits(&mut self) -> &[Limit] {
        if self.checked_at.is_none_or(|t| t.elapsed() >= CHECK) {
            self.checked_at = Some(Instant::now());
            if let Some(newest) = newest_session()
                && self.seen.as_ref() != Some(&newest)
            {
                // A brand-new session has no limits yet; keep the old ones.
                if let Some(limits) = read_limits(&newest.0) {
                    self.limits = limits;
                }
                self.seen = Some(newest);
            }
        }
        &self.limits
    }
}

fn sessions_dir() -> Option<PathBuf> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".codex"),
    };
    Some(home.join("sessions"))
}

/// Sessions live in `sessions/YYYY/MM/DD/rollout-*.jsonl`; returns the most
/// recently modified log in the latest day directory.
fn newest_session() -> Option<(PathBuf, SystemTime)> {
    let mut dir = sessions_dir()?;
    for _ in 0..3 {
        dir = fs::read_dir(&dir)
            .ok()?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .max()?;
    }
    fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .max_by_key(|(_, mtime)| *mtime)
}

fn read_limits(path: &Path) -> Option<Vec<Limit>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .filter(|line| line.contains("\"rate_limits\""))
        .find_map(parse_line)
}

fn parse_line(line: &str) -> Option<Vec<Limit>> {
    let json: Value = serde_json::from_str(line).ok()?;
    let limits = json.get("payload")?.get("rate_limits")?;
    let parsed: Vec<Limit> = ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let window = limits.get(key)?;
            Some(Limit {
                label: window_label(window.get("window_minutes")?.as_u64()?),
                used: window.get("used_percent")?.as_f64()? as f32,
                resets_at: window
                    .get("resets_at")
                    .and_then(Value::as_i64)
                    .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0)),
            })
        })
        .collect();
    (!parsed.is_empty()).then_some(parsed)
}

fn window_label(minutes: u64) -> String {
    match minutes {
        10080 => "week".into(),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rate_limit_event() {
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":6.0,"window_minutes":10080,"resets_at":1791975177},"secondary":{"used_percent":40.0,"window_minutes":300,"resets_at":1791975177}}}}"#;
        let limits = parse_line(line).unwrap();
        assert_eq!((limits[0].label.as_str(), limits[0].used), ("week", 6.0));
        assert_eq!((limits[1].label.as_str(), limits[1].used), ("5h", 40.0));
        assert!(limits[0].resets_at.is_some());
    }

    #[test]
    fn skips_events_without_limits() {
        let line = r#"{"payload":{"rate_limits":{"primary":null,"secondary":null}}}"#;
        assert!(parse_line(line).is_none());
    }
}
