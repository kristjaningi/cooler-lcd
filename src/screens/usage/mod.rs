//! Plan usage for AI coding agents (Claude Code, Codex, ...), in type big
//! enough to read from across the desk. The first
//! agent with limits (Claude Code, usually) is the hero: its headline limit
//! as a huge percentage over a thick bar, with the time until it resets.
//! Its other limits, then the other agents' headline limits, follow as one
//! line each: name, percentage, a short bar and the time left.
//!
//! The numbers come from Omarchy's records (see `records`); a limit that's
//! nearly spent takes over the panel.

use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use tiny_skia::Pixmap;

mod records;

use super::Screen;
use crate::draw::{Align, bar, fill, mix, rounded_rect, text, text_aligned};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};
use records::{Agent, Limit, Records, ellipsize, nearly_spent, short_duration};

/// Records older than this are dimmed with their age: three missed
/// refreshes at Omarchy's default interval.
const STALE: Duration = Duration::from_secs(45 * 60);
const MARGIN: f32 = 24.0;
const RIGHT: f32 = SIZE as f32 - MARGIN;
/// Where the lines below the hero start, and how much room they have
/// before the bottom edge (kept clear for the rotation bar).
const LINES_Y: f32 = 222.0;
const LINES_END: f32 = 462.0;
const LINE_H: f32 = 52.0;
const DIVIDER_H: f32 = 22.0;
const LINE_SIZE: f32 = 30.0;
/// Column edges of a line: label from the margin, percentage ending at
/// `PCT_X`, the bar from `BAR_X` to `BAR_END`, the time left ending at the
/// right margin.
const PCT_X: f32 = 220.0;
const BAR_X: f32 = 236.0;
const BAR_END: f32 = 334.0;
/// Pace marker positions are rounded to this many steps across the hero
/// bar, so it's redrawn when the marker visibly moves rather than every tick.
const PACE_STEPS: f32 = 400.0;

#[derive(Debug, PartialEq)]
struct Hero {
    name: String,
    label: String,
    percent: u32,
    pace: Option<u32>,
    /// "1h 2m", "Reset", or empty without a reset time.
    left: String,
    /// "Updated 1h ago" when the record is stale.
    stale: Option<String>,
}

#[derive(Debug, PartialEq)]
struct Line {
    label: String,
    /// None for an agent without limits, which shows `left` as its note.
    percent: Option<u32>,
    left: String,
    dim: bool,
    /// Nearly spent, so it has to stay in view.
    alert: bool,
}

#[derive(Debug, PartialEq)]
struct Content {
    hero: Option<Hero>,
    /// The hero's other limits.
    details: Vec<Line>,
    /// The other agents, one line each.
    others: Vec<Line>,
}

pub struct Usage {
    records: Records,
    content: Option<Content>,
}

impl Usage {
    pub fn new() -> Self {
        Self {
            records: Records::new(),
            content: None,
        }
    }
}

impl Screen for Usage {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let content = Some(fit(layout(self.records.agents(), now.with_timezone(&Utc))));
        let changed = self.content != content;
        self.content = content;
        changed
    }

    fn alerts(&mut self, now: DateTime<Local>) -> Vec<String> {
        self.records.alerts(now.with_timezone(&Utc))
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let Some(c) = &self.content else { return };
        fill(px, theme.background);
        let mid = SIZE as f32 / 2.0;
        if c.hero.is_none() && c.others.is_empty() {
            text(
                px,
                &theme.font,
                "No usage data",
                40.0,
                mid,
                mid - 10.0,
                theme.foreground,
            );
            text(
                px,
                &theme.font,
                "Turn on Omarchy's",
                28.0,
                mid,
                mid + 40.0,
                theme.muted,
            );
            text(
                px,
                &theme.font,
                "Agents widget",
                28.0,
                mid,
                mid + 76.0,
                theme.muted,
            );
            return;
        }

        let mut y = LINES_Y;
        if let Some(hero) = &c.hero {
            draw_hero(px, theme, hero);
        } else {
            y = MARGIN + 10.0;
        }
        for line in &c.details {
            draw_line(px, theme, line, y);
            y += LINE_H;
        }
        if !c.details.is_empty() && !c.others.is_empty() {
            rounded_rect(
                px,
                MARGIN,
                y + DIVIDER_H / 2.0 - 12.0,
                RIGHT - MARGIN,
                2.0,
                1.0,
                theme.surface,
            );
            y += DIVIDER_H;
        }
        for line in &c.others {
            draw_line(px, theme, line, y);
            y += LINE_H;
        }
    }
}

/// Sorts the agents into the hero and the lines below it.
fn layout(agents: &[Agent], now: DateTime<Utc>) -> Content {
    let hero_at = agents.iter().position(|a| !a.limits.is_empty());
    let mut content = Content {
        hero: None,
        details: Vec::new(),
        others: Vec::new(),
    };
    for (i, agent) in agents.iter().enumerate() {
        let stale = age(agent, now);
        if Some(i) == hero_at {
            let head = headline(&agent.limits, now);
            let r = Reading::of(&agent.limits[head], now);
            content.hero = Some(Hero {
                name: agent.name.clone(),
                label: compact_label(&agent.limits[head].label),
                percent: r.percent,
                pace: r.elapsed.map(|e| (e * PACE_STEPS).round() as u32),
                left: r.left,
                stale: stale.map(|age| format!("Updated {} ago", short_duration(age))),
            });
            content.details = agent
                .limits
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != head)
                .map(|(_, l)| line(compact_label(&l.label), l, stale.is_some(), now))
                .collect();
            continue;
        }
        content.others.push(match agent.limits.is_empty() {
            true => Line {
                label: compact_label(&agent.name),
                percent: None,
                left: agent
                    .status
                    .as_deref()
                    .map_or("No limits".into(), |s| ellipsize(s, 14)),
                dim: true,
                alert: false,
            },
            false => {
                let l = &agent.limits[headline(&agent.limits, now)];
                line(compact_label(&agent.name), l, stale.is_some(), now)
            }
        });
    }
    content
}

fn line(label: String, limit: &Limit, dim: bool, now: DateTime<Utc>) -> Line {
    let r = Reading::of(limit, now);
    Line {
        label,
        percent: Some(r.percent),
        left: r.left,
        dim,
        alert: nearly_spent(limit, now),
    }
}

/// How old the agent's record is, when it's older than `STALE`.
fn age(agent: &Agent, now: DateTime<Utc>) -> Option<Duration> {
    agent
        .updated_at
        .and_then(|t| (now - t).to_std().ok())
        .filter(|age| *age > STALE)
}

/// The limit to lead with: the first (the shortest window, as Omarchy lists
/// them), unless another is nearly spent; then the most spent of those.
fn headline(limits: &[Limit], now: DateTime<Utc>) -> usize {
    limits
        .iter()
        .enumerate()
        .filter(|(_, l)| nearly_spent(l, now))
        .max_by(|a, b| a.1.used.total_cmp(&b.1.used))
        .map_or(0, |(i, _)| i)
}

/// A limit as of `now`.
struct Reading {
    percent: u32,
    /// How far through its window it is, 0..=1, when that's known.
    elapsed: Option<f32>,
    left: String,
}

impl Reading {
    fn of(limit: &Limit, now: DateTime<Utc>) -> Self {
        let left = limit.resets_at.and_then(|t| (t - now).to_std().ok());
        // Past its reset time the window has started over.
        let reset_passed = limit.resets_at.is_some() && left.is_none();
        let used = if reset_passed { 0.0 } else { limit.used };
        Self {
            percent: (used * 100.0).round() as u32,
            elapsed: match (left, limit.window) {
                (Some(left), Some(window)) if left <= window => {
                    Some(1.0 - left.as_secs_f32() / window.as_secs_f32())
                }
                _ => None,
            },
            left: match left {
                Some(left) => short_duration(left),
                None if reset_passed => "Reset".into(),
                None => String::new(),
            },
        }
    }
}

/// A label short enough for a line's first column: "Fable Weekly" is just
/// "Fable" (the window is the same as the plain "Weekly" above it), and
/// anything else is cut to seven characters.
fn compact_label(label: &str) -> String {
    let label = label.split(" (").next().unwrap_or(label).trim();
    let label = match label.strip_suffix(" Weekly") {
        Some(model) if !model.is_empty() => model,
        _ => label,
    };
    let label = label.strip_suffix(" Code").unwrap_or(label);
    label.chars().take(7).collect()
}

/// Drops lines from the bottom until they fit above the bottom edge,
/// keeping nearly spent ones (they're why the screen took over): first the
/// hero's details, then the other agents.
fn fit(mut c: Content) -> Content {
    let height = |c: &Content| {
        let divider = !c.details.is_empty() && !c.others.is_empty();
        (c.details.len() + c.others.len()) as f32 * LINE_H + if divider { DIVIDER_H } else { 0.0 }
    };
    let start = if c.hero.is_some() {
        LINES_Y
    } else {
        MARGIN + 10.0
    };
    while start + height(&c) > LINES_END + 8.0 {
        let quiet = |lines: &Vec<Line>| lines.iter().rposition(|l| !l.alert);
        if let Some(i) = quiet(&c.details) {
            c.details.remove(i);
        } else if let Some(i) = quiet(&c.others) {
            c.others.remove(i);
        } else if c.others.pop().is_none() && c.details.pop().is_none() {
            break;
        }
    }
    c
}

fn level_color(theme: &Theme, percent: u32) -> Rgb {
    match percent {
        80.. => theme.red,
        50.. => theme.yellow,
        _ => theme.green,
    }
}

fn draw_hero(px: &mut Pixmap, theme: &Theme, h: &Hero) {
    let stale = h.stale.is_some();
    let level = if stale {
        theme.muted
    } else {
        level_color(theme, h.percent)
    };
    text_aligned(
        px,
        &theme.font,
        &h.name,
        32.0,
        MARGIN,
        52.0,
        if stale { theme.muted } else { theme.accent },
        Align::Left,
    );
    text_aligned(
        px,
        &theme.font,
        &h.label,
        28.0,
        RIGHT,
        52.0,
        theme.muted,
        Align::Right,
    );

    text_aligned(
        px,
        &theme.font,
        &format!("{}%", h.percent),
        104.0,
        MARGIN - 4.0,
        150.0,
        level,
        Align::Left,
    );
    // The time left, or the record's age when it can't be trusted.
    let (caption, value, color) = match &h.stale {
        Some(note) => {
            let age = note.trim_start_matches("Updated ").trim_end_matches(" ago");
            ("updated", format!("{age} ago"), theme.yellow)
        }
        None if h.left == "Reset" => ("window", "reset".into(), theme.foreground),
        None => ("resets in", h.left.clone(), theme.foreground),
    };
    if !value.is_empty() {
        text_aligned(
            px,
            &theme.font,
            caption,
            24.0,
            RIGHT,
            104.0,
            theme.muted,
            Align::Right,
        );
        text_aligned(
            px,
            &theme.font,
            &value,
            38.0,
            RIGHT,
            148.0,
            color,
            Align::Right,
        );
    }

    let (bar_y, bar_h, w) = (168.0, 26.0, RIGHT - MARGIN);
    bar(
        px,
        MARGIN,
        bar_y,
        w,
        bar_h,
        h.percent as f32 / 100.0,
        mix(theme.background, level, 0.45),
        level,
        theme.surface,
    );
    if let Some(pace) = h.pace.filter(|_| !stale) {
        // Where an even spend across the window would be by now.
        let x = MARGIN + pace.min(PACE_STEPS as u32) as f32 / PACE_STEPS * w;
        rounded_rect(
            px,
            x - 4.5,
            bar_y - 7.0,
            9.0,
            bar_h + 14.0,
            4.5,
            theme.background,
        );
        rounded_rect(
            px,
            x - 2.0,
            bar_y - 5.0,
            4.0,
            bar_h + 10.0,
            2.0,
            theme.foreground,
        );
    }
}

fn draw_line(px: &mut Pixmap, theme: &Theme, l: &Line, y: f32) {
    let baseline = y + 34.0;
    let label_color = if l.dim { theme.muted } else { theme.foreground };
    text_aligned(
        px,
        &theme.font,
        &l.label,
        LINE_SIZE,
        MARGIN,
        baseline,
        label_color,
        Align::Left,
    );
    let Some(percent) = l.percent else {
        text_aligned(
            px,
            &theme.font,
            &l.left,
            24.0,
            RIGHT,
            baseline,
            theme.muted,
            Align::Right,
        );
        return;
    };
    let level = if l.dim {
        theme.muted
    } else {
        level_color(theme, percent)
    };
    text_aligned(
        px,
        &theme.font,
        &format!("{percent}%"),
        LINE_SIZE,
        PCT_X,
        baseline,
        level,
        Align::Right,
    );
    let bar_h = 12.0;
    bar(
        px,
        BAR_X,
        baseline - 10.0 - bar_h / 2.0,
        BAR_END - BAR_X,
        bar_h,
        percent as f32 / 100.0,
        mix(theme.background, level, 0.45),
        level,
        theme.surface,
    );
    text_aligned(
        px,
        &theme.font,
        &l.left,
        LINE_SIZE,
        RIGHT,
        baseline,
        if l.dim { theme.muted } else { theme.foreground },
        Align::Right,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(label: &str, used: f32, now: DateTime<Utc>) -> Limit {
        Limit {
            label: label.into(),
            used,
            resets_at: Some(now + chrono::Duration::hours(2)),
            window: Some(Duration::from_secs(5 * 3600)),
        }
    }

    fn agent(id: &str, limits: Vec<Limit>) -> Agent {
        Agent {
            id: id.into(),
            name: id.into(),
            updated_at: None,
            limits,
            status: None,
        }
    }

    #[test]
    fn pace_and_reset() {
        let now = Utc::now();
        let mut l = limit("Session", 0.30, now);
        l.resets_at = Some(now + chrono::Duration::minutes(150));
        let r = Reading::of(&l, now);
        assert_eq!(
            (r.percent, r.elapsed, r.left.as_str()),
            (30, Some(0.5), "2h 30m")
        );

        l.resets_at = Some(now - chrono::Duration::minutes(1));
        let r = Reading::of(&l, now);
        assert_eq!((r.percent, r.elapsed, r.left.as_str()), (0, None, "Reset"));
    }

    #[test]
    fn shortens_labels() {
        assert_eq!(compact_label("Session (5-hour)"), "Session");
        assert_eq!(compact_label("Fable Weekly"), "Fable");
        assert_eq!(compact_label("Weekly"), "Weekly");
        assert_eq!(compact_label("Claude Code"), "Claude");
        assert_eq!(compact_label("Fireworks"), "Firewor");
    }

    #[test]
    fn leads_with_the_first_limit_unless_another_is_nearly_spent() {
        let now = Utc::now();
        let calm = [limit("Session", 0.5, now), limit("Weekly", 0.6, now)];
        assert_eq!(headline(&calm, now), 0);
        let spent = [limit("Session", 0.5, now), limit("Weekly", 0.95, now)];
        assert_eq!(headline(&spent, now), 1);
    }

    #[test]
    fn first_agent_with_limits_is_the_hero() {
        let now = Utc::now();
        let agents = [
            agent("claude", vec![]),
            agent(
                "codex",
                vec![limit("Session", 0.4, now), limit("Weekly", 0.1, now)],
            ),
            agent("grok", vec![limit("Weekly", 0.2, now)]),
        ];
        let c = layout(&agents, now);
        let hero = c.hero.unwrap();
        assert_eq!((hero.name.as_str(), hero.percent), ("codex", 40));
        assert_eq!(c.details.len(), 1);
        let others: Vec<_> = c.others.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(others, ["claude", "grok"]);
        assert_eq!(c.others[0].percent, None);
    }

    #[test]
    fn fit_drops_quiet_lines_first() {
        let line = |label: &str, alert| Line {
            label: label.into(),
            percent: Some(if alert { 95 } else { 10 }),
            left: String::new(),
            dim: false,
            alert,
        };
        let c = fit(Content {
            hero: Some(Hero {
                name: "Claude".into(),
                label: "Session".into(),
                percent: 10,
                pace: None,
                left: String::new(),
                stale: None,
            }),
            details: vec![line("Weekly", false), line("Fable", false)],
            others: (0..5).map(|i| line(&format!("a{i}"), i == 4)).collect(),
        });
        assert!(c.details.is_empty());
        let others: Vec<_> = c.others.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(others, ["a0", "a1", "a2", "a4"], "the alerted line stays");
    }
}
