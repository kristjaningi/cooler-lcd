//! A countdown from `cooler-lcd timer 25m`: a huge ring that runs down
//! with the time left in the middle, and once it's up, a takeover that
//! counts the time since until it's dismissed.
//!
//! It runs on the wall clock, so time spent suspended counts.

use std::time::Duration;

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{arc, fill, mix, text, text_width};
use crate::render::SIZE;
use crate::theme::Theme;

const RING_R: f32 = 204.0;
const RING_W: f32 = 26.0;
/// The ring starts and ends at twelve o'clock.
const RING_START: f32 = 270.0;
/// The widest the time in the middle can be, well inside the ring.
const TIME_W: f32 = 300.0;
const TIME_SIZE: f32 = 132.0;
/// "TIME'S UP", and the time over under it.
const DONE_SIZE: f32 = 56.0;
const OVER_SIZE: f32 = 52.0;
const LABEL_SIZE: f32 = 30.0;

/// Everything visible; the screen is redrawn only when this changes.
#[derive(PartialEq)]
enum Content {
    /// Whole seconds left, rounded up so it reads 0:00 only once it's up.
    Running(u64),
    /// Whole seconds since it was up.
    Done(u64),
}

pub struct Timer {
    length: Duration,
    ends: DateTime<Local>,
    content: Option<Content>,
}

impl Timer {
    pub fn new(length: Duration, ends: DateTime<Local>) -> Self {
        Self {
            length,
            ends,
            content: None,
        }
    }
}

impl Screen for Timer {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let content = Some(content(self.ends, now));
        let changed = self.content != content;
        self.content = content;
        changed
    }

    /// Just past the next whole second to or from `ends`, so the digits
    /// change on time instead of drifting against it.
    fn interval(&self) -> Duration {
        let ms = (self.ends - Local::now())
            .num_milliseconds()
            .rem_euclid(1000);
        Duration::from_millis(ms as u64 + 20)
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let Some(content) = &self.content else { return };
        fill(px, theme.background);
        let c = SIZE as f32 / 2.0;
        let track = mix(theme.background, theme.surface, 0.9);
        arc(px, c, c, RING_R, RING_START, 360.0, RING_W, track, false);
        match *content {
            Content::Running(left) => {
                let total = self.length.as_secs().max(1);
                // More than the whole length left only if the clock was set back.
                let left = left.min(total);
                let sweep = 360.0 * left as f32 / total as f32;
                arc(
                    px,
                    c,
                    c,
                    RING_R,
                    RING_START,
                    sweep,
                    RING_W,
                    theme.accent,
                    true,
                );
                // Sized for the timer's full length, so the digits don't
                // grow as it counts down past an hour or ten minutes.
                let size = time_size(theme, &clock(total));
                let time = clock(left);
                text(
                    px,
                    &theme.font,
                    &time,
                    size,
                    c,
                    c + size * 0.36,
                    theme.foreground,
                );
                let until = self.ends.format("until %H:%M").to_string();
                text(
                    px,
                    &theme.font,
                    &until,
                    LABEL_SIZE,
                    c,
                    c + 104.0,
                    theme.muted,
                );
            }
            Content::Done(over) => {
                arc(
                    px,
                    c,
                    c,
                    RING_R,
                    RING_START,
                    360.0,
                    RING_W,
                    theme.green,
                    true,
                );
                text(
                    px,
                    &theme.font,
                    "TIME'S UP",
                    DONE_SIZE,
                    c,
                    c - 6.0,
                    theme.foreground,
                );
                let over = format!("+{}", clock(over));
                text(px, &theme.font, &over, OVER_SIZE, c, c + 64.0, theme.green);
            }
        }
    }
}

fn content(ends: DateTime<Local>, now: DateTime<Local>) -> Content {
    let ms = (ends - now).num_milliseconds();
    if ms > 0 {
        Content::Running((ms as u64).div_ceil(1000))
    } else {
        Content::Done(ms.unsigned_abs() / 1000)
    }
}

/// `secs` as 4:05, or 1:04:05 from an hour up.
fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The size at which a time shaped like `time` fits `TIME_W`, at most
/// `TIME_SIZE`. Measured with zeros, so it holds whatever the digits.
fn time_size(theme: &Theme, time: &str) -> f32 {
    let zeros: String = time
        .chars()
        .map(|ch| if ch.is_ascii_digit() { '0' } else { ch })
        .collect();
    let width = text_width(&theme.font, &zeros, TIME_SIZE);
    if width <= TIME_W {
        TIME_SIZE
    } else {
        TIME_SIZE * TIME_W / width
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn counts_down_then_up() {
        let now = Local::now();
        let at = |ms| content(now + TimeDelta::milliseconds(ms), now);
        assert!(at(25 * 60_000) == Content::Running(1500));
        assert!(at(1_500_001) == Content::Running(1501), "rounds up");
        assert!(at(1) == Content::Running(1), "not 0:00 until it's up");
        assert!(at(0) == Content::Done(0));
        assert!(at(-61_500) == Content::Done(61));
    }

    #[test]
    fn formats_times() {
        assert_eq!(clock(1500), "25:00");
        assert_eq!(clock(65), "1:05");
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(3600 + 4 * 60 + 5), "1:04:05");
    }
}
