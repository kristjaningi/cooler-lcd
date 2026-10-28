//! The agents' plan usage, from the records Omarchy's Agents bar widget
//! keeps in `~/.local/state/omarchy/agents/usage/`, one JSON file per agent.
//! Omarchy refreshes them while its bar runs (every 15 minutes by default);
//! this only reads them, so it makes no network requests of its own.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A limit at least this used takes over the panel.
const ALERT_USED: f32 = 0.9;
/// Limits older than this are shown dimmed with their age: three missed
/// refreshes at Omarchy's default interval.
const STALE: Duration = Duration::from_secs(45 * 60);

/// Agents listed first, in this order, and shown even without limits (to
/// say why). Any other agent appears after them once it reports a limit.
/// The screen ranks agents by use; this order only breaks ties.
const PREFERRED: &[&str] = &["claude", "codex"];

/// One agent's record as Omarchy writes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    updated_at: Option<String>,
    /// When the limits were last fetched, in milliseconds since the epoch.
    /// A record is written on every refresh, but when fetching fails it
    /// carries the last limits along, flagged stale, so `updated_at` says
    /// nothing about them. Not every collector writes these.
    #[serde(default)]
    limits_fetched_at: Option<f64>,
    #[serde(default)]
    limits_stale: bool,
    #[serde(default)]
    limits: Vec<RecordLimit>,
    #[serde(default)]
    usage_status_text: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordLimit {
    #[serde(default)]
    label: String,
    /// Fraction of the limit used, 0..=1.
    percent: Option<f32>,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    /// When the limits were fetched (or, failing that, the record written).
    pub fetched_at: Option<DateTime<Utc>>,
    /// The collector couldn't refresh the limits.
    pub limits_stale: bool,
    pub limits: Vec<Limit>,
    pub status: Option<String>,
}

impl Agent {
    /// How old the limits are when they can't be trusted: the collector
    /// says it couldn't refresh them, or they're older than `STALE`.
    pub fn stale_age(&self, now: DateTime<Utc>) -> Option<Duration> {
        let age = self.fetched_at.and_then(|t| (now - t).to_std().ok());
        if self.limits_stale {
            return Some(age.unwrap_or_default());
        }
        age.filter(|age| *age > STALE)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    pub label: String,
    /// Fraction of the limit used, 0..=1.
    pub used: f32,
    pub resets_at: Option<DateTime<Utc>>,
    pub window: Option<Duration>,
}

/// The agents' records, read again whenever one of the files changes.
pub struct Records {
    dir: PathBuf,
    /// Each record file and its mtime, to re-read only when one changes.
    seen: Vec<(PathBuf, Option<SystemTime>)>,
    agents: Vec<Agent>,
}

impl Records {
    pub fn new() -> Self {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            });
        Self {
            dir: state.join("omarchy/agents/usage"),
            seen: Vec::new(),
            agents: Vec::new(),
        }
    }

    /// Records already read, from a directory that doesn't exist.
    #[cfg(test)]
    pub fn with_agents(agents: Vec<Agent>) -> Self {
        Self {
            dir: PathBuf::from("/nonexistent"),
            seen: Vec::new(),
            agents,
        }
    }

    /// The agents to show, preferred ones first, from the latest records.
    /// The screen reorders them by use.
    pub fn agents(&mut self) -> &[Agent] {
        self.reload();
        &self.agents
    }

    /// A takeover condition for each limit that's nearly spent.
    pub fn alerts(&mut self, now: DateTime<Utc>) -> Vec<String> {
        self.agents()
            .iter()
            // Old numbers are shown, dimmed, but don't take over.
            .filter(|a| a.stale_age(now).is_none())
            .flat_map(|a| {
                a.limits
                    .iter()
                    .filter(move |l| nearly_spent(l, now))
                    .map(move |l| {
                        format!(
                            "{} {} over {:.0}%",
                            a.name,
                            short_label(&l.label),
                            ALERT_USED * 100.0
                        )
                    })
            })
            .collect()
    }

    fn reload(&mut self) {
        let mut files: Vec<(PathBuf, Option<SystemTime>)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| {
                let mtime = fs::metadata(&p).and_then(|m| m.modified()).ok();
                (p, mtime)
            })
            .collect();
        files.sort();
        if files == self.seen {
            return;
        }
        // A record caught mid-write fails to parse; it keeps its last good
        // reading and is read again on the next change.
        let mut agents: Vec<Agent> = files
            .iter()
            .filter_map(|(path, _)| {
                let parsed = fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                match parsed {
                    Some(record) => Some(agent(record)),
                    None => {
                        let id = path.file_stem()?.to_str()?;
                        self.agents.iter().find(|a| a.id == id).cloned()
                    }
                }
            })
            .collect();
        agents.sort_by_key(|a| (rank(&a.id), a.id.clone()));
        agents.retain(|a| !a.limits.is_empty() || PREFERRED.contains(&a.id.as_str()));
        self.agents = agents;
        self.seen = files;
    }
}

fn rank(id: &str) -> usize {
    PREFERRED
        .iter()
        .position(|p| *p == id)
        .unwrap_or(PREFERRED.len())
}

fn agent(r: Record) -> Agent {
    let time = |s: Option<&str>| {
        DateTime::parse_from_rfc3339(s?)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    };
    let status = r
        .usage_status_text
        .filter(|s| !s.is_empty())
        .map(|s| ellipsize(&s, 40));
    let limits = r
        .limits
        .into_iter()
        .filter_map(|l| {
            Some(Limit {
                window: window(&l.label),
                label: short_label(&l.label),
                // Past 1 is past the limit, which the bars can't show.
                used: l.percent?.clamp(0.0, 1.0),
                resets_at: time(l.resets_at.as_deref()),
            })
        })
        .collect();
    Agent {
        name: if r.name.is_empty() {
            capitalize(&r.id)
        } else {
            r.name
        },
        id: r.id,
        fetched_at: r
            .limits_fetched_at
            .filter(|&ms| ms > 0.0)
            .and_then(|ms| DateTime::from_timestamp_millis(ms as i64))
            .or_else(|| time(r.updated_at.as_deref())),
        limits_stale: r.limits_stale,
        limits,
        status,
    }
}

/// The window a limit's label names, as Omarchy's collectors spell them:
/// "Session (5-hour)", "Weekly (7-day)", "Fable Weekly", "Monthly",
/// "5h window", "90m window".
fn window(label: &str) -> Option<Duration> {
    let l = label.to_lowercase();
    let hours = |h: u64| Some(Duration::from_secs(h * 3600));
    if l.contains("session") || l.contains("5-hour") {
        hours(5)
    } else if l.contains("week") || l.contains("7-day") {
        hours(7 * 24)
    } else if l.contains("month") {
        hours(30 * 24)
    } else if let Some(n) = l.strip_suffix("h window") {
        hours(n.trim().parse().ok()?)
    } else if let Some(n) = l.strip_suffix("m window") {
        Some(Duration::from_secs(n.trim().parse::<u64>().ok()? * 60))
    } else {
        None
    }
}

/// "Session (5-hour)" -> "Session".
fn short_label(label: &str) -> String {
    label.split(" (").next().unwrap_or(label).trim().to_string()
}

/// Cuts `s` to at most `max` characters, so a long message stays on screen.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// At least `ALERT_USED` spent, and not already past its reset.
pub fn nearly_spent(limit: &Limit, now: DateTime<Utc>) -> bool {
    limit.used >= ALERT_USED && limit.resets_at.is_none_or(|t| t > now)
}

/// "6d 5h", "1h 23m" or "23m".
pub fn short_duration(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    let (days, hours, mins) = (mins / 1440, mins / 60 % 24, mins % 60);
    match (days, hours) {
        (0, 0) => format!("{mins}m"),
        (0, _) => format!("{hours}h {mins}m"),
        _ => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(short_duration(Duration::from_secs(23 * 60)), "23m");
        assert_eq!(short_duration(Duration::from_secs(83 * 60)), "1h 23m");
        assert_eq!(
            short_duration(Duration::from_secs((6 * 24 + 5) * 3600)),
            "6d 5h"
        );
    }

    #[test]
    fn reads_windows_from_labels() {
        let h = |h: u64| Some(Duration::from_secs(h * 3600));
        assert_eq!(window("Session (5-hour)"), h(5));
        assert_eq!(window("Weekly (7-day)"), h(168));
        assert_eq!(window("Fable Weekly"), h(168));
        assert_eq!(window("3h window"), h(3));
        assert_eq!(window("90m window"), Some(Duration::from_secs(5400)));
        assert_eq!(window("Limit"), None);
        assert_eq!(short_label("Session (5-hour)"), "Session");
    }

    #[test]
    fn parses_omarchy_record() {
        let json = r#"{"id":"claude","name":"Claude Code","tierLabel":"max 5x",
            "updatedAt":"2026-10-09T19:03:02.187726+00:00",
            "limits":[{"label":"Session (5-hour)","percent":0.14,
                       "resetsAt":"2026-10-09T19:20:00.087780+00:00"},
                      {"label":"Weekly (7-day)","percent":0.02,"resetsAt":""}]}"#;
        let a = agent(serde_json::from_str(json).unwrap());
        assert_eq!(a.limits.len(), 2);
        assert_eq!(a.limits[0].label, "Session");
        assert!(a.limits[0].resets_at.is_some());
        assert!(a.limits[1].resets_at.is_none());
    }

    #[test]
    fn judges_freshness_by_the_limits_not_the_record() {
        let now = Utc::now();
        let record = |extra: &str| {
            let json = format!(
                r#"{{"id":"claude","updatedAt":"{}",{extra}
                    "limits":[{{"label":"Session","percent":0.95}}]}}"#,
                now.to_rfc3339()
            );
            agent(serde_json::from_str(&json).unwrap())
        };
        // Written just now, but carrying limits fetched an hour ago.
        let hour_ago = (now - chrono::Duration::hours(1)).timestamp_millis();
        let a = record(&format!(
            r#""limitsFetchedAt":{hour_ago},"limitsStale":true,"#
        ));
        let age = a.stale_age(now).unwrap();
        assert!((3590..=3610).contains(&age.as_secs()), "{age:?}");

        // Flagged stale while still recent: stale all the same.
        let recent = (now - chrono::Duration::minutes(20)).timestamp_millis();
        let a = record(&format!(
            r#""limitsFetchedAt":{recent},"limitsStale":true,"#
        ));
        assert!(a.stale_age(now).is_some());
        let a = record(&format!(r#""limitsFetchedAt":{recent},"#));
        assert!(a.stale_age(now).is_none());

        // Without the fields (Codex), the record's own time counts.
        assert!(record("").stale_age(now).is_none());
    }

    #[test]
    fn stale_limits_dont_take_over() {
        let now = Utc::now();
        let mut records = Records {
            dir: PathBuf::from("/nonexistent"),
            seen: Vec::new(),
            agents: vec![Agent {
                id: "claude".into(),
                name: "Claude".into(),
                fetched_at: Some(now),
                limits_stale: false,
                limits: vec![Limit {
                    label: "Session".into(),
                    used: 0.95,
                    resets_at: None,
                    window: None,
                }],
                status: None,
            }],
        };
        assert_eq!(records.alerts(now).len(), 1);
        records.agents[0].limits_stale = true;
        assert!(records.alerts(now).is_empty());
    }

    #[test]
    fn caps_usage_at_the_limit() {
        let json = r#"{"id":"codex","limits":[{"label":"Weekly","percent":1.5},
            {"label":"Session","percent":-0.1}]}"#;
        let a = agent(serde_json::from_str(json).unwrap());
        assert_eq!(a.limits[0].used, 1.0);
        assert_eq!(a.limits[1].used, 0.0);
    }

    #[test]
    fn alerts_on_nearly_spent_limits() {
        let now = Utc::now();
        let limit = Limit {
            label: "Session".into(),
            used: 0.92,
            resets_at: Some(now + chrono::Duration::minutes(30)),
            window: None,
        };
        assert!(nearly_spent(&limit, now));
        let reset = Limit {
            resets_at: Some(now - chrono::Duration::minutes(1)),
            ..limit.clone()
        };
        assert!(!nearly_spent(&reset, now), "already started over");
        assert!(!nearly_spent(
            &Limit {
                used: 0.89,
                ..limit
            },
            now
        ));
    }
}
