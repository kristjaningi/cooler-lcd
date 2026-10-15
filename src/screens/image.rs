//! A pre-scaled image submitted by a script; no file I/O in the frame loop.

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::theme::Theme;

pub struct Image {
    picture: Pixmap,
    drawn: bool,
}

impl Image {
    pub fn new(picture: Pixmap) -> Self {
        Self {
            picture,
            drawn: false,
        }
    }
}

impl Screen for Image {
    fn update(&mut self, _: DateTime<Local>) -> bool {
        !std::mem::replace(&mut self.drawn, true)
    }
    fn draw(&self, px: &mut Pixmap, _: &Theme) {
        px.data_mut().copy_from_slice(self.picture.data());
    }
}
