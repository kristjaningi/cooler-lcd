//! A dim clock shown while nobody is at the desktop, in place of the
//! carousel: no usage numbers on display and no radar traffic fetched.

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{fill, mix, text};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

const BLACK: Rgb = Rgb(0, 0, 0);

pub struct Away {
    time: Option<String>,
}

impl Away {
    pub fn new() -> Self {
        Self { time: None }
    }
}

impl Screen for Away {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        let time = Some(now.format("%H:%M").to_string());
        let changed = self.time != time;
        self.time = time;
        changed
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        fill(px, BLACK);
        if let Some(time) = &self.time {
            let c = SIZE as f32 / 2.0;
            let dim = mix(BLACK, theme.foreground, 0.3);
            text(px, &theme.font, time, 110.0, c, c + 38.0, dim);
        }
    }
}
