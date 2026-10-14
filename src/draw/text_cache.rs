//! Bounded layout and glyph-coverage caches for the animated radar.
//! Eight subpixel phases per axis keep moving labels within 1/16 pixel of
//! their requested position. Coverage stays independent of color, so the
//! radar can tint and fade text every frame without rasterizing it again.

use std::collections::{HashMap, VecDeque};

use ab_glyph::{Font, FontVec, GlyphId, point};
use tiny_skia::Pixmap;

use super::{Align, layout};
use crate::theme::Rgb;

const PHASES: f32 = 8.0;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_GLYPHS: usize = 8192;
const MAX_LAYOUTS: usize = 256;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    id: u16,
    size: u32,
    x: u8,
    y: u8,
}

struct Coverage {
    min: (i32, i32),
    width: usize,
    values: Vec<f32>,
}

impl Coverage {
    fn new(font: &FontVec, key: Key) -> Self {
        let glyph = GlyphId(key.id).with_scale_and_position(
            f32::from_bits(key.size),
            point(key.x as f32 / PHASES, key.y as f32 / PHASES),
        );
        let Some(outline) = font.outline_glyph(glyph) else {
            return Self {
                min: (0, 0),
                width: 0,
                values: Vec::new(),
            };
        };
        let bounds = outline.px_bounds();
        let width = bounds.width() as usize;
        let mut values = vec![0.0; width * bounds.height() as usize];
        outline.draw(|x, y, coverage| {
            values[y as usize * width + x as usize] = coverage.clamp(0.0, 1.0);
        });
        Self {
            min: (bounds.min.x as i32, bounds.min.y as i32),
            width,
            values,
        }
    }

    fn bytes(&self) -> usize {
        self.values.len() * std::mem::size_of::<f32>()
    }

    fn draw(&self, px: &mut Pixmap, origin: (i32, i32), color: Rgb) {
        if self.width == 0 {
            return;
        }
        let (w, h) = (px.width() as i32, px.height() as i32);
        let (left, top) = (origin.0 + self.min.0, origin.1 + self.min.1);
        let data = px.data_mut();
        for (row, values) in self.values.chunks_exact(self.width).enumerate() {
            let y = top + row as i32;
            if y < 0 || y >= h {
                continue;
            }
            let start = (-left).max(0) as usize;
            let end = (w - left).clamp(0, self.width as i32) as usize;
            for (x, &coverage) in values.iter().enumerate().take(end).skip(start) {
                if coverage == 0.0 {
                    continue;
                }
                let i = ((y * w + left + x as i32) * 4) as usize;
                for (c, target) in [color.0, color.1, color.2].into_iter().enumerate() {
                    let dst = data[i + c] as f32;
                    data[i + c] = (dst + (target as f32 - dst) * coverage).round() as u8;
                }
            }
        }
    }
}

type Layout = (u32, Vec<(GlyphId, f32)>, f32);

#[derive(Default)]
pub struct TextCache {
    #[cfg(test)]
    pub bypass: bool,
    revision: u64,
    layouts: HashMap<String, Layout>,
    glyphs: HashMap<Key, Coverage>,
    order: VecDeque<Key>,
    bytes: usize,
}

impl TextCache {
    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn prepare(&mut self, revision: u64) {
        if self.revision != revision {
            *self = Self {
                revision,
                ..Self::default()
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        px: &mut Pixmap,
        font: &FontVec,
        s: &str,
        size: f32,
        x: f32,
        baseline: f32,
        color: Rgb,
        align: Align,
    ) {
        #[cfg(test)]
        if self.bypass {
            return super::text_aligned(px, font, s, size, x, baseline, color, align);
        }
        // Bound retained strings too, including an unexpectedly long
        // callsign from the feed. Normal radar labels are a few bytes.
        if s.len() > 1024 {
            return super::text_aligned(px, font, s, size, x, baseline, color, align);
        }
        if self.layouts.get(s).is_none_or(|l| l.0 != size.to_bits()) {
            if self.layouts.len() >= MAX_LAYOUTS {
                self.layouts.clear();
            }
            let (glyphs, width) = layout(font, s, size);
            self.layouts
                .insert(s.to_string(), (size.to_bits(), glyphs, width));
        }
        let (_, glyphs, width) = &self.layouts[s];
        let left = match align {
            Align::Left => x,
            Align::Center => x - width / 2.0,
            Align::Right => x - width,
        };
        let phase = |v: f32| {
            let v = (v * PHASES).round() as i32;
            (
                v.div_euclid(PHASES as i32),
                v.rem_euclid(PHASES as i32) as u8,
            )
        };
        let (oy, py) = phase(baseline);
        for &(id, offset) in glyphs {
            let (ox, px_phase) = phase(left + offset);
            let key = Key {
                id: id.0,
                size: size.to_bits(),
                x: px_phase,
                y: py,
            };
            if !self.glyphs.contains_key(&key) {
                let coverage = Coverage::new(font, key);
                let bytes = coverage.bytes();
                // FIFO eviction bounds both coverage memory and metadata.
                // A hit never allocates or changes the eviction queue.
                while !self.order.is_empty()
                    && (self.bytes + bytes > MAX_BYTES || self.glyphs.len() >= MAX_GLYPHS)
                {
                    let old = self.order.pop_front().unwrap();
                    self.bytes -= self.glyphs.remove(&old).unwrap().bytes();
                }
                if bytes > MAX_BYTES {
                    coverage.draw(px, (ox, oy), color);
                    continue;
                }
                self.bytes += bytes;
                self.order.push_back(key);
                self.glyphs.insert(key, coverage);
            }
            self.glyphs[&key].draw(px, (ox, oy), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    #[test]
    fn cached_text_matches_the_original_with_small_subpixel_error_and_clipping() {
        let theme = Theme::fallback();
        let mut cache = TextCache::default();
        cache.prepare(theme.revision);
        for align in [Align::Left, Align::Center, Align::Right] {
            for (x, y) in [(7.31, 18.41), (-9.2, 6.7), (126.3, 48.03)] {
                let mut cached = Pixmap::new(160, 64).unwrap();
                cached.fill(tiny_skia::Color::BLACK);
                let mut direct = cached.clone();
                let text = "ICE614 035↓ 102";
                cache.draw(
                    &mut cached,
                    &theme.font,
                    text,
                    14.0,
                    x,
                    y,
                    Rgb(255, 255, 255),
                    align,
                );
                super::super::text_aligned(
                    &mut direct,
                    &theme.font,
                    text,
                    14.0,
                    x,
                    y,
                    Rgb(255, 255, 255),
                    align,
                );
                let error: u64 = cached
                    .data()
                    .iter()
                    .zip(direct.data())
                    .map(|(a, b)| a.abs_diff(*b) as u64)
                    .sum();
                let ink: u64 = direct.pixels().iter().map(|p| p.red() as u64 * 3).sum();
                assert!(
                    error as f64 / (ink.max(1) as f64) < 0.08,
                    "subpixel error {error}, ink {ink}"
                );
                assert!(cached.pixels().iter().all(|p| p.alpha() == 255));
                let before = cache.glyphs.len();
                let mut again = Pixmap::new(160, 64).unwrap();
                again.fill(tiny_skia::Color::BLACK);
                cache.draw(
                    &mut again,
                    &theme.font,
                    text,
                    14.0,
                    x,
                    y,
                    Rgb(255, 255, 255),
                    align,
                );
                assert_eq!(cached.data(), again.data());
                assert_eq!(cache.glyphs.len(), before, "warm draw reuses coverage");
            }
        }
    }

    #[test]
    fn cache_memory_is_bounded_and_theme_reload_clears_it() {
        let theme = Theme::fallback();
        let mut cache = TextCache::default();
        cache.prepare(theme.revision);
        let mut px = Pixmap::new(8, 8).unwrap();
        // Large glyphs exercise byte eviction, spaces exercise entry bounds.
        for i in 0..20 {
            cache.draw(
                &mut px,
                &theme.font,
                "W",
                400.0 + i as f32 * 20.0,
                0.0,
                0.0,
                Rgb(0, 0, 0),
                Align::Left,
            );
            assert!(cache.bytes <= MAX_BYTES);
        }
        for i in 0..MAX_GLYPHS + 10 {
            cache.draw(
                &mut px,
                &theme.font,
                " ",
                i as f32 + 1.0,
                0.0,
                0.0,
                Rgb(0, 0, 0),
                Align::Left,
            );
        }
        assert!(cache.glyphs.len() <= MAX_GLYPHS);
        for i in 0..MAX_LAYOUTS + 10 {
            cache.draw(
                &mut px,
                &theme.font,
                &format!("ICE{i}"),
                14.0,
                0.0,
                0.0,
                Rgb(255, 255, 255),
                Align::Left,
            );
        }
        assert!(cache.layouts.len() <= MAX_LAYOUTS);
        cache.prepare(theme.revision + 1);
        assert!(cache.glyphs.is_empty() && cache.layouts.is_empty() && cache.order.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}
