//! Clock, date, and CPU/GPU temperature cards.

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use super::sensors::Sensors;
use crate::draw::{fill, rounded_rect, text};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

const CARD_Y: f32 = 284.0;
const CARD_W: f32 = 208.0;
const CARD_H: f32 = 172.0;
const CARD_XS: [f32; 2] = [24.0, 248.0];

/// Everything visible; the screen is redrawn only when this changes.
#[derive(PartialEq)]
struct Content {
    time: String,
    date: String,
    temps: [Option<i32>; 2],
}

pub struct Dashboard {
    sensors: Sensors,
    content: Option<Content>,
}

impl Dashboard {
    pub fn new() -> Self {
        Self {
            sensors: Sensors::new(),
            content: None,
        }
    }
}

impl Screen for Dashboard {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let (cpu, gpu) = self.sensors.temps();
        let content = Content {
            time: now.format("%H:%M").to_string(),
            date: now.format("%a %d %b").to_string(),
            temps: [cpu, gpu].map(|t| t.map(|t| t.round() as i32)),
        };
        let changed = self.content.as_ref() != Some(&content);
        self.content = Some(content);
        changed
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let Some(content) = &self.content else { return };
        fill(px, theme.background);

        let center = SIZE as f32 / 2.0;
        text(
            px,
            &theme.font,
            &content.time,
            130.0,
            center,
            180.0,
            theme.foreground,
        );
        text(
            px,
            &theme.font,
            &content.date,
            36.0,
            center,
            240.0,
            theme.accent,
        );

        for ((x, label), temp) in CARD_XS.into_iter().zip(["CPU", "GPU"]).zip(content.temps) {
            let cx = x + CARD_W / 2.0;
            rounded_rect(px, x, CARD_Y, CARD_W, CARD_H, 20.0, theme.surface);
            text(px, &theme.font, label, 28.0, cx, CARD_Y + 46.0, theme.muted);
            let (value, color) = match temp {
                Some(t) => (format!("{t}°"), temp_color(theme, t)),
                None => ("--".into(), theme.muted),
            };
            text(px, &theme.font, &value, 84.0, cx, CARD_Y + 140.0, color);
        }
    }
}

fn temp_color(theme: &Theme, t: i32) -> Rgb {
    match t {
        80.. => theme.red,
        65.. => theme.yellow,
        _ => theme.green,
    }
}
