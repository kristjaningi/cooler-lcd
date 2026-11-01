//! What's playing: the album art across the whole screen, shaded toward
//! the bottom where the title and artist go, and the track's progress in
//! the bottom bar instead of the rotation's. In the rotation only while
//! something plays (and for `mpris::GRACE` after it pauses).

mod art;
mod mpris;

use std::sync::Arc;

use chrono::{DateTime, Local};
use tiny_skia::Pixmap;

use super::Screen;
use crate::draw::{Align, arc, ellipsize, fill, mix, rounded_rect, text, text_aligned, wrap};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

const MARGIN: f32 = 28.0;
const TITLE_SIZE: f32 = 40.0;
const TITLE_LINES: usize = 2;
const ARTIST_SIZE: f32 = 30.0;
/// The artist's baseline, clear of the bar along the bottom edge.
const ARTIST_BASELINE: f32 = SIZE as f32 - 30.0;
const WHITE: Rgb = Rgb(255, 255, 255);
const BLACK: Rgb = Rgb(0, 0, 0);

/// Everything visible; the screen is redrawn only when this changes.
#[derive(PartialEq)]
struct Content {
    title: String,
    artist: String,
    playing: bool,
}

pub struct Music {
    shared: mpris::Shared,
    playback: Option<mpris::Playback>,
    content: Option<Content>,
    art: Option<Arc<Pixmap>>,
}

impl Music {
    pub fn new() -> Self {
        Self {
            shared: mpris::spawn(),
            playback: None,
            content: None,
            art: None,
        }
    }
}

impl Screen for Music {
    fn activity(&mut self, active: bool, visible: bool) {
        self.shared.lock().unwrap().activity(active, visible);
    }

    fn update(&mut self, _now: DateTime<Local>) -> bool {
        let s = self.shared.lock().unwrap();
        self.playback = s.playback.clone();
        let art = s.art.clone();
        drop(s);
        let content = self.playback.as_ref().map(|p| Content {
            title: p.track.title.clone(),
            artist: p.track.artist.clone(),
            playing: p.playing,
        });
        let art_changed = match (&self.art, &art) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        let changed = self.content != content || art_changed;
        self.content = content;
        self.art = art;
        changed
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        match &self.art {
            Some(art) => px.data_mut().copy_from_slice(art.data()),
            None => no_art(px, theme),
        }
        let Some(content) = &self.content else {
            let c = SIZE as f32 / 2.0;
            text(
                px,
                &theme.font,
                "Nothing playing",
                TITLE_SIZE,
                c,
                ARTIST_BASELINE,
                WHITE,
            );
            return;
        };
        let width = SIZE as f32 - 2.0 * MARGIN;
        let mut baseline = ARTIST_BASELINE;
        if !content.artist.is_empty() {
            let artist = ellipsize(&theme.font, &content.artist, ARTIST_SIZE, width);
            let color = mix(BLACK, WHITE, 0.8);
            left(px, theme, &artist, ARTIST_SIZE, baseline, color);
            baseline -= ARTIST_SIZE + 16.0;
        }
        for line in title_lines(theme, &content.title, width).iter().rev() {
            left(px, theme, line, TITLE_SIZE, baseline, WHITE);
            baseline -= TITLE_SIZE * 1.2;
        }
        if !content.playing {
            paused(px);
        }
    }

    /// The track's progress, which moves the bar between polls.
    fn bar(&self) -> Option<f32> {
        self.playback.as_ref()?.progress()
    }

    fn present(&self) -> bool {
        self.shared.lock().unwrap().present()
    }

    fn loading(&self) -> bool {
        !self.shared.lock().unwrap().ready
    }
}

fn left(px: &mut Pixmap, theme: &Theme, s: &str, size: f32, baseline: f32, color: Rgb) {
    text_aligned(
        px,
        &theme.font,
        s,
        size,
        MARGIN,
        baseline,
        color,
        Align::Left,
    );
}

/// The title wrapped to at most `TITLE_LINES`, the last cut short with an
/// ellipsis if there's more.
fn title_lines(theme: &Theme, title: &str, width: f32) -> Vec<String> {
    let mut lines = wrap(&theme.font, title, TITLE_SIZE, width);
    if lines.len() > TITLE_LINES {
        let rest = lines.split_off(TITLE_LINES - 1).join(" ");
        lines.push(ellipsize(&theme.font, &rest, TITLE_SIZE, width));
    }
    lines
}

/// In place of missing art: a record in the theme's colors, shaded like
/// art would be.
fn no_art(px: &mut Pixmap, theme: &Theme) {
    fill(px, theme.background);
    let (cx, cy) = (SIZE as f32 / 2.0, 190.0);
    let vinyl = mix(theme.background, BLACK, 0.5);
    arc(px, cx, cy, 80.0, 0.0, 360.0, 160.0, vinyl, false);
    let groove = mix(vinyl, theme.muted, 0.35);
    for r in (60..=150).step_by(15) {
        arc(px, cx, cy, r as f32, 0.0, 360.0, 1.5, groove, false);
    }
    arc(px, cx, cy, 20.0, 0.0, 360.0, 40.0, theme.accent, false);
    arc(px, cx, cy, 3.0, 0.0, 360.0, 6.0, vinyl, false);
    art::shade(px);
}

/// A pause sign in the top right corner, on a dark disc so it shows on
/// any art.
fn paused(px: &mut Pixmap) {
    let (cx, cy, r) = (SIZE as f32 - 52.0, 52.0, 30.0);
    arc(
        px,
        cx,
        cy,
        r / 2.0,
        0.0,
        360.0,
        r,
        mix(BLACK, WHITE, 0.12),
        false,
    );
    for dx in [-9.0, 3.0] {
        rounded_rect(px, cx + dx, cy - 12.0, 6.0, 24.0, 2.0, WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_new_image_at_the_same_url_redraws_the_screen() {
        let shared = mpris::Shared::default();
        let mut music = Music {
            shared: shared.clone(),
            playback: None,
            content: None,
            art: None,
        };
        assert!(!music.update(Local::now()));
        shared.lock().unwrap().art = Some(Arc::new(Pixmap::new(SIZE, SIZE).unwrap()));
        assert!(music.update(Local::now()));
        assert!(!music.update(Local::now()));
        shared.lock().unwrap().art = Some(Arc::new(Pixmap::new(SIZE, SIZE).unwrap()));
        assert!(
            music.update(Local::now()),
            "image identity matters even when content stays the same"
        );
    }
}
