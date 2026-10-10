//! Turns a screen into a ready-to-send frame (64-byte header + JPEG).
//!
//! The main loop only calls `render` when the screen, its content or the
//! theme changed, so most ticks just resend the last frame.

use anyhow::Result;
use jpeg_encoder::{ColorType, Encoder};
use tiny_skia::Pixmap;

use crate::device::{HEADER_LEN, write_header};
use crate::screens::Screen;
use crate::theme::Theme;

pub const SIZE: u32 = 480;

pub struct Renderer {
    canvas: Pixmap,
    frame: Vec<u8>,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            canvas: Pixmap::new(SIZE, SIZE).unwrap(),
            frame: Vec::with_capacity(128 * 1024),
        }
    }

    pub fn render(&mut self, screen: &dyn Screen, theme: &Theme) -> Result<()> {
        screen.draw(&mut self.canvas, theme);
        self.frame.clear();
        self.frame.resize(HEADER_LEN, 0);
        Encoder::new(&mut self.frame, screen.jpeg_quality()).encode(
            self.canvas.data(),
            SIZE as u16,
            SIZE as u16,
            ColorType::Rgba,
        )?;
        write_header(&mut self.frame, SIZE, SIZE);
        Ok(())
    }

    /// The last encoded frame (header + JPEG); empty before the first render.
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }
}
