//! Plan usage for AI coding agents (Claude Code, Codex, ...) as horizontal
//! bars, with a pace marker and the time until each limit resets.
//!
//! The numbers come from the records Omarchy's Agents bar widget keeps in
//! `~/.local/state/omarchy/agents/usage/`, one JSON file per agent. Omarchy
//! refreshes them while its bar runs (every 15 minutes by default); this
//! screen only reads them, so it makes no network requests of its own.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, Utc};
use serde::Deserialize;
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{Align, bar, fill, mix, rounded_rect, text, text_aligned};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

/// Records older than this are drawn dimmed with their age: three missed
/// refreshes at Omarchy's default interval.
const STALE: Duration = Duration::from_secs(45 * 60);

/// Agents listed first, in this order, and shown even without limits (to
/// say why). Any other agent appears after them once it reports a limit.
const PREFERRED: &[&str] = &["claude", "codex"];

const MARGIN: f32 = 28.0;
const WIDTH: f32 = SIZE as f32 - 2.0 * MARGIN;
const HEADER_H: f32 = 50.0;
const ROW_H: f32 = 78.0;
const NOTE_H: f32 = 34.0;
const SECTION_GAP: f32 = 18.0;
const BAR_H: f32 = 16.0;
/// Pace marker positions are rounded to this many steps across the bar, so
/// the screen is redrawn when the marker visibly moves rather than every tick.
const PACE_STEPS: f32 = 400.0;

/// One agent's record as Omarchy writes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tier_label: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
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
struct Agent {
    id: String,
    name: String,
    plan: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    limits: Vec<Limit>,
    status: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Limit {
    label: String,
    /// Fraction of the limit used, 0..=1.
    used: f32,
    resets_at: Option<DateTime<Utc>>,
    window: Option<Duration>,
}

/// One agent's block on screen; the screen is redrawn only when these change.
#[derive(Debug, PartialEq)]
struct Section {
    title: String,
    plan: String,
    rows: Vec<Row>,
    note: Option<String>,
    stale: bool,
}

#[derive(Debug, PartialEq)]
struct Row {
    label: String,
    percent: u32,
    /// Where an even spend across the window would be now, in `PACE_STEPS`.
    pace: Option<u32>,
    caption: String,
    /// Percentage points ahead of (+) or behind (-) pace.
    vs_pace: Option<i32>,
}

pub struct Usage {
    dir: PathBuf,
    /// Each record file and its mtime, to re-read only when one changes.
    seen: Vec<(PathBuf, Option<SystemTime>)>,
    agents: Vec<Agent>,
    content: Option<Vec<Section>>,
}

impl Usage {
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
            content: None,
        }
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

impl Screen for Usage {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        self.reload();
        let now = now.with_timezone(&Utc);
        let content = Some(fit(self.agents.iter().map(|a| section(a, now)).collect()));
        let changed = self.content != content;
        self.content = content;
        changed
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let Some(sections) = &self.content else {
            return;
        };
        fill(px, theme.background);
        if sections.is_empty() {
            let c = SIZE as f32 / 2.0;
            text(
                px,
                &theme.font,
                "No usage data",
                30.0,
                c,
                c - 6.0,
                theme.foreground,
            );
            text(
                px,
                &theme.font,
                "Enable Omarchy's Agents widget",
                19.0,
                c,
                c + 30.0,
                theme.muted,
            );
            return;
        }
        let mut y = (SIZE as f32 - total_height(sections)) / 2.0;
        for section in sections {
            y = draw_section(px, theme, section, y) + SECTION_GAP;
        }
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
    let plan = r
        .tier_label
        .filter(|s| !s.is_empty())
        .map(|s| capitalize(&s));
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
                used: l.percent?.max(0.0),
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
        plan,
        updated_at: time(r.updated_at.as_deref()),
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
fn ellipsize(s: &str, max: usize) -> String {
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

fn section(agent: &Agent, now: DateTime<Utc>) -> Section {
    let age = agent
        .updated_at
        .and_then(|t| (now - t).to_std().ok())
        .filter(|age| *age > STALE);
    let note = match (age, &agent.status) {
        _ if agent.limits.is_empty() => Some(
            agent
                .status
                .clone()
                .unwrap_or_else(|| "No limits reported".into()),
        ),
        (Some(age), _) => Some(format!("Updated {} ago", short_duration(age))),
        _ => None,
    };
    Section {
        title: agent.name.clone(),
        plan: agent.plan.clone().unwrap_or_default(),
        rows: agent.limits.iter().map(|l| row(l, now)).collect(),
        note,
        stale: age.is_some(),
    }
}

fn row(limit: &Limit, now: DateTime<Utc>) -> Row {
    let left = limit.resets_at.and_then(|t| (t - now).to_std().ok());
    // Past its reset time the window has started over, whatever the last
    // reading said.
    let reset_passed = limit.resets_at.is_some() && left.is_none();
    let used = if reset_passed { 0.0 } else { limit.used };
    let percent = (used * 100.0).round() as u32;
    let elapsed = match (left, limit.window) {
        (Some(left), Some(window)) if left <= window => {
            Some(1.0 - left.as_secs_f32() / window.as_secs_f32())
        }
        _ => None,
    };
    Row {
        label: limit.label.clone(),
        percent,
        pace: elapsed.map(|e| (e * PACE_STEPS).round() as u32),
        caption: match left {
            Some(left) => format!("Resets in {}", short_duration(left)),
            None if reset_passed => "Reset".into(),
            None => String::new(),
        },
        vs_pace: elapsed.map(|e| percent as i32 - (e * 100.0).round() as i32),
    }
}

fn section_height(s: &Section) -> f32 {
    HEADER_H + s.rows.len() as f32 * ROW_H + if s.note.is_some() { NOTE_H } else { 0.0 }
}

fn total_height(sections: &[Section]) -> f32 {
    let gaps = sections.len().saturating_sub(1) as f32 * SECTION_GAP;
    sections.iter().map(section_height).sum::<f32>() + gaps
}

/// Drops rows from the bottom (then whole sections) until everything fits.
fn fit(mut sections: Vec<Section>) -> Vec<Section> {
    let room = SIZE as f32 - 2.0 * MARGIN + 12.0;
    while total_height(&sections) > room {
        let Some(last) = sections.last_mut() else {
            break;
        };
        if last.rows.len() > 1 {
            last.rows.pop();
        } else {
            sections.pop();
        }
    }
    sections
}

/// Draws one section from `y` down and returns where it ends.
fn draw_section(px: &mut Pixmap, theme: &Theme, s: &Section, mut y: f32) -> f32 {
    let (left, right) = (MARGIN, MARGIN + WIDTH);
    let title_color = if s.stale { theme.muted } else { theme.accent };
    text_aligned(
        px,
        &theme.font,
        &s.title,
        28.0,
        left,
        y + 28.0,
        title_color,
        Align::Left,
    );
    if !s.plan.is_empty() {
        text_aligned(
            px,
            &theme.font,
            &s.plan,
            19.0,
            right,
            y + 27.0,
            theme.muted,
            Align::Right,
        );
    }
    rounded_rect(px, left, y + 40.0, WIDTH, 2.0, 1.0, theme.surface);
    y += HEADER_H;

    for row in &s.rows {
        draw_row(px, theme, row, s.stale, y);
        y += ROW_H;
    }
    if let Some(note) = &s.note {
        let color = if s.rows.is_empty() {
            theme.yellow
        } else {
            theme.muted
        };
        text_aligned(
            px,
            &theme.font,
            note,
            18.0,
            left,
            y + 22.0,
            color,
            Align::Left,
        );
        y += NOTE_H;
    }
    y
}

fn draw_row(px: &mut Pixmap, theme: &Theme, row: &Row, stale: bool, y: f32) {
    let (left, right) = (MARGIN, MARGIN + WIDTH);
    let level = if stale {
        theme.muted
    } else {
        level_color(theme, row.percent)
    };
    let label_color = if stale { theme.muted } else { theme.foreground };
    text_aligned(
        px,
        &theme.font,
        &row.label,
        21.0,
        left,
        y + 22.0,
        label_color,
        Align::Left,
    );
    let value = format!("{}%", row.percent);
    text_aligned(
        px,
        &theme.font,
        &value,
        28.0,
        right,
        y + 24.0,
        level,
        Align::Right,
    );

    let bar_y = y + 34.0;
    bar(
        px,
        left,
        bar_y,
        WIDTH,
        BAR_H,
        row.percent as f32 / 100.0,
        mix(theme.background, level, 0.45),
        level,
        theme.surface,
    );
    // Notches at each quarter make the bar read like a gauge.
    for q in 1..4 {
        let x = left + WIDTH * q as f32 / 4.0;
        rounded_rect(px, x - 1.0, bar_y, 2.0, BAR_H, 0.0, theme.background);
    }
    if let Some(pace) = row.pace {
        // A notch where an even spend would be by now: a fill short of it is
        // under pace, past it is burning faster than the window allows.
        let x = left + pace.min(PACE_STEPS as u32) as f32 / PACE_STEPS * WIDTH;
        rounded_rect(
            px,
            x - 3.5,
            bar_y - 6.0,
            7.0,
            BAR_H + 12.0,
            3.5,
            theme.background,
        );
        rounded_rect(
            px,
            x - 1.5,
            bar_y - 4.0,
            3.0,
            BAR_H + 8.0,
            1.5,
            theme.foreground,
        );
    }

    let caption_y = bar_y + BAR_H + 22.0;
    text_aligned(
        px,
        &theme.font,
        &row.caption,
        17.0,
        left,
        caption_y,
        theme.muted,
        Align::Left,
    );
    if let Some(d) = row.vs_pace.filter(|_| !stale) {
        let (label, color) = match d {
            ..=-3 => (format!("{} under pace", -d), theme.muted),
            3.. => (format!("{d} over pace"), theme.yellow),
            _ => ("On pace".into(), theme.muted),
        };
        text_aligned(
            px,
            &theme.font,
            &label,
            17.0,
            right,
            caption_y,
            color,
            Align::Right,
        );
    }
}

fn level_color(theme: &Theme, percent: u32) -> Rgb {
    match percent {
        80.. => theme.red,
        50.. => theme.yellow,
        _ => theme.green,
    }
}

/// "6d 5h", "1h 23m" or "23m".
fn short_duration(d: Duration) -> String {
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
        assert_eq!(a.plan.as_deref(), Some("Max 5x"));
        assert_eq!(a.limits.len(), 2);
        assert_eq!(a.limits[0].label, "Session");
        assert!(a.limits[0].resets_at.is_some());
        assert!(a.limits[1].resets_at.is_none());
    }

    #[test]
    fn pace_and_reset() {
        let now = Utc::now();
        let limit = Limit {
            label: "Session".into(),
            used: 0.30,
            resets_at: Some(now + chrono::Duration::minutes(150)),
            window: Some(Duration::from_secs(5 * 3600)),
        };
        let r = row(&limit, now);
        assert_eq!((r.percent, r.vs_pace), (30, Some(-20)));

        let passed = Limit {
            resets_at: Some(now - chrono::Duration::minutes(1)),
            ..limit
        };
        let r = row(&passed, now);
        assert_eq!((r.percent, r.caption.as_str(), r.pace), (0, "Reset", None));
    }
}
