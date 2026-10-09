//! Claude and Codex plan usage as ring gauges, with time until each limit
//! resets.

mod claude;
mod codex;

use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, Utc};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{fill, ring, text};
use crate::theme::{Rgb, Theme};

/// Claude numbers older than this are drawn dimmed with their age.
const STALE: Duration = Duration::from_secs(20 * 60);

const RADIUS: f32 = 62.0;
const STROKE: f32 = 12.0;
const SECTIONS: [(f32, f32); 2] = [(40.0, 132.0), (276.0, 368.0)]; // (title baseline, ring center y)

#[derive(Clone, Debug, PartialEq)]
pub struct Limit {
    pub label: String,
    /// Percent of the limit used.
    pub used: f32,
    pub resets_at: Option<DateTime<Utc>>,
}

/// One drawn ring; the screen is redrawn only when these change.
#[derive(PartialEq)]
struct Gauge {
    label: String,
    percent: Option<u32>,
    caption: String,
    stale: bool,
}

pub struct Usage {
    claude: claude::Shared,
    codex: codex::Codex,
    content: Option<[Vec<Gauge>; 2]>,
}

impl Usage {
    pub fn new() -> Self {
        Self {
            claude: claude::spawn(),
            codex: codex::Codex::default(),
            content: None,
        }
    }
}

impl Screen for Usage {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let now = now.with_timezone(&Utc);
        let claude = match &*self.claude.lock().unwrap() {
            Some(usage) => {
                let age = SystemTime::now()
                    .duration_since(usage.fetched_at)
                    .unwrap_or_default();
                gauges(&usage.limits, now, (age > STALE).then_some(age))
            }
            None => placeholders(&["5h", "week"]),
        };
        let codex = match self.codex.limits() {
            [] => placeholders(&["week"]),
            limits => gauges(limits, now, None),
        };
        let content = Some([claude, codex]);
        let changed = self.content != content;
        self.content = content;
        changed
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let Some(sections) = &self.content else {
            return;
        };
        fill(px, theme.background);
        for ((title, gauges), (title_y, cy)) in
            ["Claude", "Codex"].iter().zip(sections).zip(SECTIONS)
        {
            text(px, &theme.font, title, 26.0, 240.0, title_y, theme.accent);
            let xs: &[f32] = if gauges.len() == 1 {
                &[240.0]
            } else {
                &[130.0, 350.0]
            };
            for (gauge, &cx) in gauges.iter().zip(xs) {
                draw_gauge(px, theme, gauge, cx, cy);
            }
        }
    }
}

fn draw_gauge(px: &mut Pixmap, theme: &Theme, gauge: &Gauge, cx: f32, cy: f32) {
    let color = match gauge.percent {
        _ if gauge.stale => theme.muted,
        Some(p) => level_color(theme, p),
        None => theme.muted,
    };
    let fraction = gauge.percent.map_or(0.0, |p| p.min(100) as f32 / 100.0);
    ring(px, cx, cy, RADIUS, STROKE, fraction, color, theme.surface);

    let value = gauge.percent.map_or("--".into(), |p| format!("{p}%"));
    let value_color = if gauge.stale {
        theme.muted
    } else {
        theme.foreground
    };
    text(px, &theme.font, &value, 36.0, cx, cy + 8.0, value_color);
    text(
        px,
        &theme.font,
        &gauge.label,
        18.0,
        cx,
        cy + 34.0,
        theme.muted,
    );
    text(
        px,
        &theme.font,
        &gauge.caption,
        18.0,
        cx,
        cy + RADIUS + 34.0,
        theme.muted,
    );
}

fn level_color(theme: &Theme, percent: u32) -> Rgb {
    match percent {
        80.. => theme.red,
        50.. => theme.yellow,
        _ => theme.green,
    }
}

/// `stale` is the data's age when it is old enough to flag.
fn gauges(limits: &[Limit], now: DateTime<Utc>, stale: Option<Duration>) -> Vec<Gauge> {
    limits
        .iter()
        .take(2)
        .map(|limit| {
            // Past its reset time the window has started over, whatever the
            // last reading said.
            let reset_passed = limit.resets_at.is_some_and(|t| t <= now);
            let used = if reset_passed { 0.0 } else { limit.used };
            let caption = match (stale, limit.resets_at) {
                (Some(age), _) => format!("{} ago", short_duration(age)),
                (None, Some(t)) if !reset_passed => {
                    format!(
                        "resets {}",
                        short_duration((t - now).to_std().unwrap_or_default())
                    )
                }
                _ => String::new(),
            };
            Gauge {
                label: limit.label.clone(),
                percent: Some(used.round().max(0.0) as u32),
                caption,
                stale: stale.is_some(),
            }
        })
        .collect()
}

fn placeholders(labels: &[&str]) -> Vec<Gauge> {
    labels
        .iter()
        .map(|label| Gauge {
            label: (*label).into(),
            percent: None,
            caption: "no data".into(),
            stale: false,
        })
        .collect()
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
    fn passed_reset_means_zero_used() {
        let now = Utc::now();
        let limit = Limit {
            label: "week".into(),
            used: 80.0,
            resets_at: Some(now - chrono::Duration::minutes(1)),
        };
        let g = &gauges(&[limit], now, None)[0];
        assert_eq!((g.percent, g.caption.as_str()), (Some(0), ""));
    }
}
