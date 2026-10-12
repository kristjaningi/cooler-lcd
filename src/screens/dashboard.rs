//! The dashboard, built for reading from across the desk: the clock and
//! date, a ring gauge each for the CPU and GPU (temperature outside, load
//! inside), and the last ten minutes of temperatures as a chart. The CPU is
//! drawn in the accent color and the GPU in cyan throughout, so the chart's
//! lines match the rings.

use std::time::Instant;

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use super::sensors::{HISTORY, Readings, Sample, Sensors};
use crate::draw::{Align, arc, fill, mix, rounded_rect, sparkline, text, text_aligned};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

/// A temperature at or above `HOT` takes over the panel; it has to drop
/// below `COOL` before it can again, so hovering at the line doesn't flap.
const HOT: f32 = 85.0;
const COOL: f32 = 75.0;

const MARGIN: f32 = 24.0;
const RING_Y: f32 = 236.0;
const RING_XS: [f32; 2] = [128.0, 352.0];
const RING_R: f32 = 84.0;
const RING_W: f32 = 16.0;
const LOAD_R: f32 = 62.0;
const LOAD_W: f32 = 7.0;
/// The rings open at the bottom: they start at 135° and run 270° round.
const RING_START: f32 = 135.0;
const RING_SWEEP: f32 = 270.0;
/// A full temperature ring.
const MAX_TEMP: f32 = 100.0;
const CHART_Y: f32 = 352.0;
const CHART_H: f32 = 106.0;
/// Chart points per history, so it's redrawn when a line visibly moves.
const CHART_STEPS: usize = 60;

/// Everything visible; the screen is redrawn only when this changes.
#[derive(PartialEq)]
struct Content {
    time: String,
    weekday: String,
    date: String,
    /// CPU, then GPU: temperature in °C and load in percent.
    temps: [Option<i32>; 2],
    loads: [Option<u32>; 2],
    mem: Option<u32>,
    power: Option<u32>,
    /// CPU and GPU temperature history, `CHART_STEPS` buckets old to new.
    chart: [Vec<Option<i32>>; 2],
}

pub struct Dashboard {
    sensors: Sensors,
    content: Option<Content>,
    /// CPU and GPU currently over `HOT` (until they drop below `COOL`).
    hot: [bool; 2],
}

impl Dashboard {
    pub fn new() -> Self {
        Self {
            sensors: Sensors::new(),
            content: None,
            hot: [false; 2],
        }
    }
}

impl Screen for Dashboard {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let r: Readings = self.sensors.readings();
        let percent = |f: Option<f32>| f.map(|f| (f * 100.0).round() as u32);
        let content = Content {
            time: now.format("%H:%M").to_string(),
            weekday: now.format("%a").to_string().to_uppercase(),
            date: now.format("%-d %b").to_string().to_uppercase(),
            temps: [r.cpu_temp, r.gpu_temp].map(|t| t.map(|t| t.round() as i32)),
            loads: [percent(r.cpu_load), percent(r.gpu_load)],
            mem: percent(r.mem.map(|(used, total)| used as f32 / total.max(1) as f32)),
            power: r.gpu_power.map(|w| w.round() as u32),
            chart: self.sensors.with_history(|h| chart(h, Instant::now())),
        };
        let changed = self.content.as_ref() != Some(&content);
        self.content = Some(content);
        changed
    }

    fn alerts(&mut self, _now: DateTime<Local>) -> Vec<String> {
        let (cpu, gpu) = self.sensors.temps();
        for (hot, temp) in self.hot.iter_mut().zip([cpu, gpu]) {
            *hot = hot_now(*hot, temp);
        }
        ["CPU", "GPU"]
            .into_iter()
            .zip(self.hot)
            .filter(|(_, hot)| *hot)
            .map(|(label, _)| format!("{label} over {HOT:.0}°"))
            .collect()
    }

    fn loading(&self) -> bool {
        !self.sensors.ready()
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let Some(c) = &self.content else { return };
        fill(px, theme.background);
        let right = SIZE as f32 - MARGIN;

        // Clock on the left, the date stacked on the right.
        text_aligned(
            px,
            &theme.font,
            &c.time,
            100.0,
            MARGIN - 4.0,
            104.0,
            theme.foreground,
            Align::Left,
        );
        text_aligned(
            px,
            &theme.font,
            &c.weekday,
            34.0,
            right,
            60.0,
            theme.accent,
            Align::Right,
        );
        text_aligned(
            px,
            &theme.font,
            &c.date,
            34.0,
            right,
            102.0,
            theme.foreground,
            Align::Right,
        );

        let colors = [theme.accent, theme.cyan];
        for (i, label) in ["CPU", "GPU"].into_iter().enumerate() {
            ring(
                px, theme, RING_XS[i], label, c.temps[i], c.loads[i], colors[i],
            );
        }

        draw_chart(px, theme, c, colors);
    }
}

/// One gauge: temperature as the outer ring and number, load as the thin
/// inner ring and the number in the opening at the bottom.
fn ring(
    px: &mut Pixmap,
    theme: &Theme,
    cx: f32,
    label: &str,
    temp: Option<i32>,
    load: Option<u32>,
    color: Rgb,
) {
    let cy = RING_Y;
    let track = mix(theme.background, theme.surface, 0.9);
    arc(
        px, cx, cy, RING_R, RING_START, RING_SWEEP, RING_W, track, false,
    );
    arc(
        px, cx, cy, LOAD_R, RING_START, RING_SWEEP, LOAD_W, track, false,
    );
    if let Some(t) = temp {
        let f = (t as f32 / MAX_TEMP).clamp(0.02, 1.0);
        arc(
            px,
            cx,
            cy,
            RING_R,
            RING_START,
            RING_SWEEP * f,
            RING_W,
            color,
            true,
        );
    }
    let load_color = mix(theme.foreground, color, 0.35);
    if let Some(l) = load {
        let f = (l as f32 / 100.0).clamp(0.01, 1.0);
        arc(
            px,
            cx,
            cy,
            LOAD_R,
            RING_START,
            RING_SWEEP * f,
            LOAD_W,
            load_color,
            false,
        );
    }

    text(px, &theme.font, label, 22.0, cx, cy - 22.0, theme.muted);
    let (value, value_color) = match temp {
        Some(t) => (format!("{t}°"), temp_color(theme, t)),
        None => ("--".into(), theme.muted),
    };
    text(
        px,
        &theme.font,
        &value,
        50.0,
        cx + 8.0,
        cy + 30.0,
        value_color,
    );
    let load = load.map_or("--".into(), |l| format!("{l}%"));
    text(
        px,
        &theme.font,
        &load,
        28.0,
        cx,
        cy + RING_R + 6.0,
        load_color,
    );
}

/// Hot readings stand out from the rest of the screen's calm foreground.
fn temp_color(theme: &Theme, t: i32) -> Rgb {
    match t {
        80.. => theme.red,
        70.. => theme.yellow,
        _ => theme.foreground,
    }
}

/// Whether a sensor counts as hot, given whether it did a moment ago. A
/// missing reading clears it.
fn hot_now(was_hot: bool, temp: Option<f32>) -> bool {
    match temp {
        Some(t) if t >= HOT => true,
        Some(t) if was_hot => t >= COOL,
        _ => false,
    }
}

/// The temperature history card: memory and GPU power along the top, the
/// CPU and GPU lines below.
fn draw_chart(px: &mut Pixmap, theme: &Theme, c: &Content, colors: [Rgb; 2]) {
    let (x, w) = (MARGIN, SIZE as f32 - 2.0 * MARGIN);
    rounded_rect(px, x, CHART_Y, w, CHART_H, 18.0, theme.surface);

    let caption_y = CHART_Y + 32.0;
    let pad = 18.0;
    let mem = c.mem.map_or("--".into(), |m| format!("{m}%"));
    text_aligned(
        px,
        &theme.font,
        "RAM",
        22.0,
        x + pad,
        caption_y,
        theme.muted,
        Align::Left,
    );
    text_aligned(
        px,
        &theme.font,
        &mem,
        22.0,
        x + pad + 52.0,
        caption_y,
        theme.foreground,
        Align::Left,
    );
    let power = c.power.map_or("--".into(), |p| format!("{p}W"));
    text_aligned(
        px,
        &theme.font,
        &power,
        22.0,
        x + w - pad,
        caption_y,
        theme.foreground,
        Align::Right,
    );
    text_aligned(
        px,
        &theme.font,
        "GPU",
        22.0,
        x + w - pad - 14.0 - 13.2 * power.chars().count() as f32,
        caption_y,
        theme.muted,
        Align::Right,
    );
    text(
        px,
        &theme.font,
        "10 MIN",
        18.0,
        x + w / 2.0,
        caption_y - 2.0,
        mix(theme.surface, theme.muted, 0.7),
    );

    // Both lines share one scale, at least 10° tall so noise stays flat.
    let all = c.chart.iter().flatten().flatten().copied();
    let (lo, hi) = all.fold((i32::MAX, i32::MIN), |(lo, hi), t| (lo.min(t), hi.max(t)));
    if lo > hi {
        return;
    }
    let mid = (lo + hi) as f32 / 2.0;
    let span = ((hi - lo) as f32).max(10.0) + 4.0;
    let (top, floor) = (caption_y + 12.0, CHART_Y + CHART_H - 10.0);
    let (left, right) = (x + pad, x + w - pad);
    for (series, color) in c.chart.iter().zip(colors) {
        let points: Vec<(f32, f32)> = series
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let t = (*t)? as f32;
                let px_x = left + (right - left) * i as f32 / (CHART_STEPS - 1) as f32;
                let f = ((t - mid) / span + 0.5).clamp(0.0, 1.0);
                Some((px_x, floor - f * (floor - top)))
            })
            .collect();
        sparkline(px, &points, floor, 3.0, color);
    }
}

/// The history in `CHART_STEPS` buckets across the last `HISTORY`, each the
/// latest reading in it, rounded to whole degrees.
fn chart(history: &std::collections::VecDeque<Sample>, now: Instant) -> [Vec<Option<i32>>; 2] {
    let mut out = [vec![None; CHART_STEPS], vec![None; CHART_STEPS]];
    let span = HISTORY.as_secs_f32();
    for &(at, cpu, gpu) in history {
        let age = now.saturating_duration_since(at).as_secs_f32();
        let i = ((1.0 - age / span) * (CHART_STEPS - 1) as f32).round();
        if i < 0.0 {
            continue;
        }
        let i = i as usize;
        out[0][i] = cpu.map(|t| t.round() as i32).or(out[0][i]);
        out[1][i] = gpu.map(|t| t.round() as i32).or(out[1][i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn hot_has_hysteresis() {
        assert!(!hot_now(false, Some(84.0)));
        assert!(hot_now(false, Some(85.0)));
        assert!(hot_now(true, Some(80.0)), "stays hot until it cools");
        assert!(!hot_now(true, Some(74.0)));
        assert!(!hot_now(true, None));
    }

    #[test]
    fn charts_history_by_age() {
        let now = Instant::now();
        let history = [
            (now - HISTORY, Some(40.0), None),
            (now - HISTORY / 2, Some(50.4), Some(60.0)),
            (now, Some(70.0), Some(65.0)),
        ]
        .into_iter()
        .collect();
        let [cpu, gpu] = chart(&history, now);
        assert_eq!(cpu[0], Some(40));
        assert_eq!(cpu[CHART_STEPS / 2], Some(50));
        assert_eq!(cpu[CHART_STEPS - 1], Some(70));
        assert_eq!(gpu[0], None);
        assert_eq!(cpu.iter().flatten().count(), 3);
        // Too old to show.
        let old = [(now - HISTORY - Duration::from_secs(30), Some(1.0), None)]
            .into_iter()
            .collect();
        assert!(chart(&old, now)[0].iter().all(Option::is_none));
    }
}
