//! Plan usage for AI coding agents (Claude Code, Codex, ...), in type big
//! enough to read from across the desk. The agents are ranked by how much
//! of their headline limit is used, and the most used is the hero: its
//! headline limit as a huge percentage over a thick bar, with the time
//! until it resets, and its plan beside its name. Its other limits and any
//! free resets, then the other agents' headline limits, follow as one line
//! each: name (with a count of free resets when it has some), percentage, a
//! short bar and the time left.
//!
//! The numbers come from Omarchy's records (see `records`); a limit that's
//! nearly spent takes over the panel.

use chrono::{DateTime, Local, Utc};
use tiny_skia::Pixmap;

mod records;

use super::Screen;
use crate::draw::{
    Align, bar, fill, mix, reset_icon, rounded_rect, text, text_aligned, text_width,
};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};
use records::{Agent, Limit, Records, Resets, ellipsize, nearly_spent, short_duration};

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
/// The plan's type beside the hero's name.
const PLAN_SIZE: f32 = 28.0;
/// The reset badge after a line's label: the sign's center this far past
/// the label, the count this far past the sign's center, in this size.
const BADGE_GAP: f32 = 20.0;
const BADGE_COUNT_X: f32 = 13.0;
const BADGE_SIZE: f32 = 26.0;
/// Points of usage another agent needs over the hero to take its place, so
/// two agents at nearly the same level don't swap on every refresh.
const OVERTAKE: u32 = 10;

#[derive(Debug, PartialEq)]
struct Hero {
    name: String,
    /// The plan the account is on ("Max 5x").
    plan: Option<String>,
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
    /// Has to stay in view when lines are dropped to fit: it's nearly
    /// spent, or it's the hero's free resets, which have nowhere else to go.
    keep: bool,
    /// Free resets the agent has left, shown beside its name.
    resets: Option<u32>,
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
    /// The hero, which keeps the top spot until it's overtaken.
    leader: Option<Leader>,
}

/// The hero's agent and the percentage it was shown at.
struct Leader {
    id: String,
    percent: u32,
}

impl Usage {
    pub fn new() -> Self {
        Self {
            records: Records::new(),
            content: None,
            leader: None,
        }
    }
}

impl Screen for Usage {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let now = now.with_timezone(&Utc);
        let agents = ranked(self.records.agents(), self.leader.as_ref(), now);
        self.leader = agents
            .iter()
            .find(|a| !a.limits.is_empty())
            .map(|a| Leader {
                id: a.id.clone(),
                percent: standing(a, now).percent,
            });
        let content = Some(fit(layout(&agents, now)));
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

/// Where an agent stands in the ranking.
struct Standing {
    /// Fresh with limits, stale with limits, or without limits.
    group: u8,
    /// Its headline limit's percentage, as shown.
    percent: u32,
    /// Its headline limit is nearly spent.
    alert: bool,
}

fn standing(a: &Agent, now: DateTime<Utc>) -> Standing {
    let Some(head) = (!a.limits.is_empty()).then(|| &a.limits[headline(&a.limits, now)]) else {
        return Standing {
            group: 2,
            percent: 0,
            alert: false,
        };
    };
    Standing {
        group: a.stale_age(now).is_some() as u8,
        percent: Reading::of(head, now).percent,
        alert: nearly_spent(head, now),
    }
}

/// Orders the agents most used first: fresh ones with limits by their
/// headline limit's percentage, then stale ones the same way, then those
/// without limits. Ties keep the records' order (Claude Code, Codex, then
/// the rest by id).
///
/// The current `leader` stays first while it's in the leading group and the
/// runner-up isn't `OVERTAKE` points ahead of it, unless the runner-up is
/// nearly spent and the leader isn't (that's what took over the panel), or
/// the leader just dropped by `OVERTAKE` or more (its window reset).
fn ranked<'a>(agents: &'a [Agent], leader: Option<&Leader>, now: DateTime<Utc>) -> Vec<&'a Agent> {
    let mut agents: Vec<&Agent> = agents.iter().collect();
    agents.sort_by_key(|a| {
        let s = standing(a, now);
        (s.group, std::cmp::Reverse(s.percent))
    });
    let Some(leader) = leader else { return agents };
    if let Some(i) = agents.iter().position(|a| a.id == leader.id)
        && i > 0
    {
        let held = standing(agents[i], now);
        let top = standing(agents[0], now);
        if held.group == top.group
            && held.group < 2
            && top.percent < held.percent + OVERTAKE
            && (held.alert || !top.alert)
            && leader.percent < held.percent + OVERTAKE
        {
            let held = agents.remove(i);
            agents.insert(0, held);
        }
    }
    agents
}

/// Sorts the agents into the hero and the lines below it.
fn layout(agents: &[&Agent], now: DateTime<Utc>) -> Content {
    let hero_at = agents.iter().position(|a| !a.limits.is_empty());
    let mut content = Content {
        hero: None,
        details: Vec::new(),
        others: Vec::new(),
    };
    for (i, agent) in agents.iter().enumerate() {
        let stale = agent.stale_age(now);
        if Some(i) == hero_at {
            let head = headline(&agent.limits, now);
            let r = Reading::of(&agent.limits[head], now);
            content.hero = Some(Hero {
                name: agent.name.clone(),
                plan: agent.plan.clone(),
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
            content.details.extend(agent.resets.as_ref().map(|r| Line {
                label: "Resets".into(),
                percent: None,
                left: resets_text(r, now),
                dim: stale.is_some(),
                keep: true,
                resets: None,
            }));
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
                keep: false,
                resets: None,
            },
            false => {
                let l = &agent.limits[headline(&agent.limits, now)];
                Line {
                    resets: agent.resets.as_ref().map(|r| r.available),
                    ..line(compact_label(&agent.name), l, stale.is_some(), now)
                }
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
        keep: nearly_spent(limit, now),
        resets: None,
    }
}

/// "2 · next expires in 1d 3h", after the "Resets" label: Omarchy's
/// wording, short enough to fit beside it.
fn resets_text(r: &Resets, now: DateTime<Utc>) -> String {
    match r.next_expires.and_then(|t| (t - now).to_std().ok()) {
        Some(left) => format!("{} · next expires in {}", r.available, short_duration(left)),
        None => r.available.to_string(),
    }
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
/// keeping nearly spent ones (they're why the screen took over) and the
/// hero's free resets: first the hero's details, then the other agents.
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
        let quiet = |lines: &Vec<Line>| lines.iter().rposition(|l| !l.keep);
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
    // The plan follows the name, cut short before it runs into the label.
    if let Some(plan) = &h.plan {
        let x = MARGIN + text_width(&theme.font, &h.name, 32.0) + 14.0;
        let room = RIGHT - text_width(&theme.font, &h.label, 28.0) - 20.0 - x;
        let plan = crate::draw::ellipsize(&theme.font, plan, PLAN_SIZE, room);
        if room > 0.0 && plan != "…" {
            text_aligned(
                px,
                &theme.font,
                &plan,
                PLAN_SIZE,
                x,
                52.0,
                theme.foreground,
                Align::Left,
            );
        }
    }

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
    // A reset sign and the count of free resets follow the name, which is
    // cut short if that's what it takes to end them before the percentage.
    let badge = l.resets.map(|n| {
        let count = n.to_string();
        let width = BADGE_GAP + BADGE_COUNT_X + text_width(&theme.font, &count, BADGE_SIZE);
        (count, width)
    });
    let label_end = match l.percent {
        Some(p) => PCT_X - text_width(&theme.font, &format!("{p}%"), LINE_SIZE) - 10.0,
        None => RIGHT,
    } - badge.as_ref().map_or(0.0, |(_, w)| *w);
    let label = crate::draw::ellipsize(&theme.font, &l.label, LINE_SIZE, label_end - MARGIN);
    text_aligned(
        px,
        &theme.font,
        &label,
        LINE_SIZE,
        MARGIN,
        baseline,
        label_color,
        Align::Left,
    );
    if let Some((count, _)) = badge {
        let color = if l.dim { theme.muted } else { theme.accent };
        let x = MARGIN + text_width(&theme.font, &label, LINE_SIZE) + BADGE_GAP;
        reset_icon(px, x, baseline - 10.0, 8.0, 3.0, color);
        text_aligned(
            px,
            &theme.font,
            &count,
            BADGE_SIZE,
            x + BADGE_COUNT_X,
            baseline,
            color,
            Align::Left,
        );
    }
    let Some(percent) = l.percent else {
        // A note in the room the label leaves.
        let room = RIGHT - (MARGIN + text_width(&theme.font, &label, LINE_SIZE) + 16.0);
        text_aligned(
            px,
            &theme.font,
            &crate::draw::ellipsize(&theme.font, &l.left, 24.0, room),
            24.0,
            RIGHT,
            baseline,
            label_color,
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
    use std::time::Duration;

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
            fetched_at: None,
            limits_stale: false,
            limits,
            status: None,
            plan: None,
            resets: None,
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
        let c = layout(&agents.iter().collect::<Vec<_>>(), now);
        let hero = c.hero.unwrap();
        assert_eq!((hero.name.as_str(), hero.percent), ("codex", 40));
        assert_eq!(c.details.len(), 1);
        let others: Vec<_> = c.others.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(others, ["claude", "grok"]);
        assert_eq!(c.others[0].percent, None);
    }

    #[test]
    fn ranks_the_most_used_first() {
        let now = Utc::now();
        let mut stale = agent("amp", vec![limit("Session", 0.9, now)]);
        stale.limits_stale = true;
        let agents = [
            agent("claude", vec![limit("Session", 0.2, now)]),
            agent("codex", vec![limit("Session", 0.6, now)]),
            agent("gemini", vec![]),
            agent("grok", vec![limit("Session", 0.2, now)]),
            stale,
        ];
        let ids = |leader| -> Vec<String> {
            ranked(&agents, leader, now)
                .iter()
                .map(|a| a.id.clone())
                .collect()
        };
        assert_eq!(
            ids(None),
            ["codex", "claude", "grok", "amp", "gemini"],
            "ties keep their order; stale, then no limits, go last"
        );
    }

    #[test]
    fn ranks_by_the_headline_limit_as_shown() {
        let now = Utc::now();
        let mut reset = limit("Session", 0.8, now);
        reset.resets_at = Some(now - chrono::Duration::minutes(1));
        let agents = [
            agent("claude", vec![reset, limit("Weekly", 0.3, now)]),
            agent(
                "codex",
                vec![limit("Session", 0.4, now), limit("Weekly", 0.95, now)],
            ),
        ];
        let ids: Vec<_> = ranked(&agents, None, now)
            .iter()
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["codex", "claude"],
            "a reset window counts as 0%; a nearly spent one leads"
        );
    }

    #[test]
    fn the_hero_holds_until_clearly_overtaken() {
        let now = Utc::now();
        let pair = |claude, codex| {
            [
                agent("claude", vec![limit("Session", claude, now)]),
                agent("codex", vec![limit("Session", codex, now)]),
            ]
        };
        let claude = |percent| Leader {
            id: "claude".into(),
            percent,
        };
        let first =
            |agents: &[Agent], leader: Option<&Leader>| ranked(agents, leader, now)[0].id.clone();
        assert_eq!(first(&pair(0.40, 0.49), Some(&claude(40))), "claude");
        assert_eq!(first(&pair(0.40, 0.50), Some(&claude(40))), "codex");
        assert_eq!(first(&pair(0.40, 0.49), None), "codex");

        // A stale leader can't hold against fresh numbers.
        let mut agents = pair(0.40, 0.45);
        agents[0].limits_stale = true;
        assert_eq!(first(&agents, Some(&claude(40))), "codex");

        // Nor can a calm leader against the limit that took over the panel,
        // though two nearly spent ones still hold.
        assert_eq!(first(&pair(0.85, 0.92), Some(&claude(85))), "codex");
        assert_eq!(first(&pair(0.91, 0.99), Some(&claude(91))), "claude");
    }

    #[test]
    fn a_reset_leader_gives_up_the_top_spot() {
        let now = Utc::now();
        let mut reset = limit("Session", 0.8, now);
        reset.resets_at = Some(now - chrono::Duration::minutes(1));
        let agents = [
            agent("claude", vec![reset]),
            agent("codex", vec![limit("Session", 0.09, now)]),
        ];
        let leader = |percent| Leader {
            id: "claude".into(),
            percent,
        };
        let first = |percent| ranked(&agents, Some(&leader(percent)), now)[0].id.clone();
        assert_eq!(first(80), "codex", "dropped from 80% to 0%");
        assert_eq!(first(5), "claude", "a small drop still holds");
    }

    #[test]
    fn update_keeps_the_hero_between_refreshes() {
        let now = Utc::now();
        let mut usage = Usage::new();
        let mut hero = |claude, codex| {
            usage.records = Records::with_agents(vec![
                agent("claude", vec![limit("Session", claude, now)]),
                agent("codex", vec![limit("Session", codex, now)]),
            ]);
            usage.update(now.with_timezone(&Local));
            let c = usage.content.as_ref().unwrap();
            c.hero.as_ref().unwrap().name.clone()
        };
        assert_eq!(hero(0.40, 0.30), "claude");
        assert_eq!(hero(0.40, 0.45), "claude");
        assert_eq!(hero(0.40, 0.55), "codex");
        assert_eq!(hero(0.50, 0.55), "codex");
        assert_eq!(hero(0.92, 0.55), "claude", "nearly spent takes over");
    }

    #[test]
    fn shows_the_plan_and_free_resets() {
        let now = Utc::now();
        let resets = Resets {
            available: 2,
            next_expires: Some(now + chrono::Duration::minutes(90)),
        };
        let mut codex = agent("codex", vec![limit("Weekly", 0.6, now)]);
        codex.plan = Some("Pro 100".into());
        codex.resets = Some(resets.clone());
        let mut claude = agent("claude", vec![limit("Session", 0.3, now)]);
        claude.plan = Some("Max 5x".into());

        // As the hero: the plan by its name, the resets as a line of their own.
        let c = layout(&[&codex, &claude], now);
        assert_eq!(c.hero.unwrap().plan.as_deref(), Some("Pro 100"));
        assert_eq!(c.details.len(), 1);
        assert!(c.details[0].left.starts_with("2 · next expires in 1h"));
        assert!(c.details[0].keep, "it has nowhere else to go");
        assert_eq!(c.others[0].resets, None);

        // Below it: the count beside its name.
        let c = layout(&[&claude, &codex], now);
        assert!(c.details.is_empty());
        assert_eq!(c.others[0].resets, Some(2));

        // Past the first expiry, only the count is known.
        let lapsed = Resets {
            next_expires: Some(now - chrono::Duration::minutes(1)),
            ..resets
        };
        assert_eq!(resets_text(&lapsed, now), "2");
    }

    #[test]
    fn fit_drops_quiet_lines_first() {
        let line = |label: &str, alert| Line {
            label: label.into(),
            percent: Some(if alert { 95 } else { 10 }),
            left: String::new(),
            dim: false,
            keep: alert,
            resets: None,
        };
        let c = fit(Content {
            hero: Some(Hero {
                name: "Claude".into(),
                plan: None,
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
