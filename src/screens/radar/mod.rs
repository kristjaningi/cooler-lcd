//! A surveillance radar scope over Reykjavik and Keflavik with live air
//! traffic: a rotating sweep over range rings, the coastline and runways,
//! and each aircraft as a blip with its history trail, a one-minute velocity
//! vector and a data block (callsign, altitude, speed).
//!
//! Positions come from ADS-B (see `adsb`) every few seconds and are dead
//! reckoned in between, so blips move smoothly at the animation rate.
//!
//! The map can be turned so a chosen bearing is at the top (`View`), to
//! face the way the panel does; the bezel's N marks north.

mod adsb;
mod geo;
mod runways;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, Mask, Paint, PathBuilder, Pixmap, Point,
    RadialGradient, SpreadMode, Stroke, StrokeDash, SweepGradient, Transform,
};

use super::Screen;
use crate::draw::{Align, mix, text_aligned};
use crate::render::SIZE;
use crate::theme::{Rgb, Theme};

/// Scope center unless the config sets one: between the two airports.
pub const CENTER: (f32, f32) = (64.05, -22.25);
/// Range at the scope's edge, nautical miles.
const RANGE_NM: f32 = 25.0;
/// Aircraft are fetched a little beyond the edge so they enter smoothly,
/// and so the fetch can ask about a coarser center than the scope's.
const FETCH_NM: u32 = 35;
/// The center sent to the feed is rounded to this many degrees (about
/// 3 nm north-south at most off, well inside the extra fetched range), so
/// a precise center, like someone's home, stays on this machine.
const FETCH_GRID: f32 = 0.1;
const RING_NM: f32 = 5.0;
const C: f32 = SIZE as f32 / 2.0;
const SCOPE_R: f32 = 222.0;
const PX_PER_NM: f32 = SCOPE_R / RANGE_NM;

/// One full turn of the sweep.
const SWEEP: Duration = Duration::from_secs(4);
/// The fading glow behind the beam, as a fraction of a turn.
const TRAIL: f32 = 0.22;
const FRAME: Duration = Duration::from_millis(66);

/// History dots kept per aircraft, one per fetch.
const HISTORY: usize = 8;
/// Dead reckoning stops this long after the last position report.
const MAX_EXTRAPOLATION: f32 = 30.0;
/// Velocity vector length.
const VECTOR_SECS: f32 = 60.0;

/// Icelandair's ICAO callsign prefix.
const ICELANDAIR: &str = "ICE";
/// Icelandair gold, fixed rather than themed so the flag carrier always
/// stands out from the phosphor.
const GOLD: Rgb = Rgb(255, 184, 28);
/// Arrivals and departures listed in the HUD, at most.
const MOVEMENTS: usize = 2;
/// One beacon pulse around each Icelandair aircraft.
const PULSE: Duration = Duration::from_millis(1800);

const AIRPORTS: &[(&str, (f32, f32))] = &[("KEF", (63.985, -22.6056)), ("RVK", (64.13, -21.9406))];

struct Track {
    ac: adsb::Aircraft,
    /// Earlier reported positions, oldest first.
    history: VecDeque<(f32, f32)>,
}

pub struct Radar {
    view: View,
    feed: adsb::Shared,
    seq: u64,
    tracks: HashMap<String, Track>,
    started: Instant,
    /// Times of the frame being drawn, set by `update`.
    now: Instant,
    utc: DateTime<Utc>,
    status: Status,
    /// Clips map features to the scope.
    scope: Mask,
    /// The parts that never move (scope, rings, bezel, map), drawn once per
    /// theme and copied under each frame.
    backdrop: RefCell<Option<(ThemeKey, Pixmap)>>,
}

/// What the backdrop depends on: theme colors and the loaded font (a
/// reloaded theme has its font at a new address).
type ThemeKey = ([Rgb; 3], usize);

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Acquiring,
    Live,
    NoLink,
}

impl Radar {
    /// `center` is the latitude and longitude at the middle of the scope,
    /// `heading` the compass bearing at its top.
    pub fn new(center: (f32, f32), heading: f32) -> Self {
        let mut scope = Mask::new(SIZE, SIZE).unwrap();
        if let Some(circle) = PathBuilder::from_circle(C, C, SCOPE_R) {
            scope.fill_path(&circle, FillRule::Winding, true, Transform::identity());
        }
        let now = Instant::now();
        Self {
            view: View::new(center, heading),
            feed: adsb::spawn(coarse(center.0), coarse(center.1), FETCH_NM),
            seq: 0,
            tracks: HashMap::new(),
            started: now,
            now,
            utc: Utc::now(),
            status: Status::Acquiring,
            scope,
            backdrop: RefCell::new(None),
        }
    }

    /// Takes in a newer aircraft list from the feed, if there is one. `want` marks the radar as
    /// on screen, which keeps the feed polling.
    fn sync(&mut self, want: bool) {
        let fresh = {
            let mut feed = self.feed.lock().unwrap();
            if want {
                feed.want();
            }
            // A failed poll (usually a 429) leaves the last aircraft list in
            // place and dead reckoning carries on, so the link only counts
            // as lost once that data has gone stale.
            let recent = feed.fetched_at.is_some_and(|t| t.elapsed() < adsb::MAX_AGE);
            self.status = match (&feed.error, feed.seq) {
                (Some(_), _) if !recent => Status::NoLink,
                (None, 0) => Status::Acquiring,
                _ => Status::Live,
            };
            (feed.seq != self.seq).then(|| (feed.seq, feed.aircraft.clone()))
        };
        if let Some((seq, aircraft)) = fresh {
            self.seq = seq;
            self.merge(aircraft);
        }
    }

    fn merge(&mut self, aircraft: Vec<adsb::Aircraft>) {
        let mut tracks = HashMap::with_capacity(aircraft.len());
        for ac in aircraft {
            let mut history = VecDeque::new();
            if let Some(mut old) = self.tracks.remove(&ac.hex) {
                history = std::mem::take(&mut old.history);
                if (old.ac.lat, old.ac.lon) != (ac.lat, ac.lon) {
                    history.push_back((old.ac.lat, old.ac.lon));
                    if history.len() > HISTORY {
                        history.pop_front();
                    }
                }
            }
            tracks.insert(ac.hex.clone(), Track { ac, history });
        }
        self.tracks = tracks;
    }

    /// The sweep's angle now, degrees clockwise from the top.
    fn beam(&self) -> f32 {
        // Wrap in integer time first: uptime as an f32 loses the precision
        // a smooth sweep needs after a few days.
        let sweep = SWEEP.as_millis();
        let phase = (self.now - self.started).as_millis() % sweep;
        phase as f32 / sweep as f32 * 360.0
    }

    /// How far through its current beacon pulse an Icelandair blip is, 0..1.
    fn pulse(&self) -> f32 {
        let pulse = PULSE.as_millis();
        let phase = (self.now - self.started).as_millis() % pulse;
        phase as f32 / pulse as f32
    }
}

/// Whether a callsign is an Icelandair flight ("ICE614"), not just one that
/// happens to start with the letters.
fn is_icelandair(ident: &str) -> bool {
    ident
        .strip_prefix(ICELANDAIR)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

impl Screen for Radar {
    fn update(&mut self, now: DateTime<Local>) -> bool {
        self.now = Instant::now();
        self.utc = now.with_timezone(&Utc);
        self.sync(true);
        // The sweep never stops, so every frame is new.
        true
    }

    fn draw(&self, px: &mut Pixmap, theme: &Theme) {
        let p = Palette::new(theme);
        let key = (
            [theme.background, theme.green, theme.red],
            theme.font.as_slice().as_ptr() as usize,
        );
        let mut cache = self.backdrop.borrow_mut();
        if cache.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut layer = Pixmap::new(SIZE, SIZE).unwrap();
            layer.fill(color(p.backdrop, 255));
            draw_scope(&mut layer, &p, &self.view);
            draw_map(&mut layer, &p, &self.view, &self.scope);
            *cache = Some((key, layer));
        }
        if let Some((_, layer)) = cache.as_ref() {
            px.data_mut().copy_from_slice(layer.data());
        }

        let beam = self.beam();
        draw_sweep(px, &p, beam);
        let mut targets: Vec<(&Track, (f32, f32))> = self
            .tracks
            .values()
            .filter(|t| self.now.saturating_duration_since(t.ac.fixed_at) < adsb::MAX_AGE)
            .map(|t| (t, dead_reckon(&t.ac, self.now)))
            .filter(|(_, pos)| in_scope(self.view.project(*pos)))
            .collect();
        // Ground traffic first, so airborne blips and their labels sit on
        // top, with Icelandair above the rest and emergencies above all.
        targets.sort_by_key(|(t, _)| {
            (
                t.ac.alt.is_some(),
                t.ac.emergency,
                is_icelandair(&t.ac.ident),
            )
        });
        let pulse = self.pulse();
        for (track, pos) in &targets {
            draw_track(px, &p, &self.view, &self.scope, track, *pos, beam, pulse);
        }

        let airborne = targets.iter().filter(|(t, _)| t.ac.alt.is_some()).count();
        let icelandair = targets
            .iter()
            .filter(|(t, _)| t.ac.alt.is_some() && is_icelandair(&t.ac.ident))
            .count();
        let mut movements: Vec<(runways::Movement, &str)> = targets
            .iter()
            .filter_map(|(t, pos)| {
                let m = runways::classify(&t.ac, self.view.project(*pos), &self.view)?;
                Some((m, &*t.ac.ident))
            })
            .collect();
        movements.sort_by_key(|(m, ident)| (m.kind == runways::Kind::Departure, *ident));
        movements.truncate(MOVEMENTS);
        draw_hud(
            px,
            &p,
            self.status,
            airborne,
            icelandair,
            &movements,
            self.utc,
            self.now - self.started,
        );
        scanlines(px);
    }

    /// Emergency squawks and Icelandair flights on approach. Uses the data
    /// already fetched without asking for more, so it only sees traffic
    /// while the radar has been on screen recently.
    fn alerts(&mut self, _now: DateTime<Local>) -> Vec<String> {
        self.sync(false);
        let now = Instant::now();
        let mut alerts: Vec<String> = self
            .tracks
            .values()
            .map(|t| &t.ac)
            .filter(|ac| now.saturating_duration_since(ac.fixed_at) < adsb::MAX_AGE)
            .filter_map(|ac| {
                let pos = self.view.project(dead_reckon(ac, now));
                if !in_scope(pos) {
                    return None;
                }
                if ac.emergency {
                    return Some(format!("{} squawking emergency", ac.ident));
                }
                let m = runways::classify(ac, pos, &self.view)?;
                (is_icelandair(&ac.ident) && m.kind == runways::Kind::Arrival)
                    .then(|| format!("{} landing {} {}", ac.ident, m.airport, m.runway))
            })
            .collect();
        alerts.sort();
        alerts
    }

    fn interval(&self) -> Duration {
        FRAME
    }

    /// Encoding is most of a frame's cost at 15 fps; at 80 the encoder
    /// halves its chroma resolution and takes under half the time, which
    /// the dim phosphor picture doesn't show.
    fn jpeg_quality(&self) -> u8 {
        80
    }

    fn loading(&self) -> bool {
        self.status == Status::Acquiring && self.started.elapsed() < Duration::from_secs(5)
    }
}

/// The theme's colors, recast as a phosphor display.
struct Palette<'a> {
    theme: &'a Theme,
    backdrop: Rgb,
    phosphor: Rgb,
    /// `phosphor` at `level` (0..=1) of full brightness over the backdrop.
    glow: [Rgb; 11],
    hot: Rgb,
    alert: Rgb,
}

impl<'a> Palette<'a> {
    fn new(theme: &'a Theme) -> Self {
        let backdrop = mix(theme.background, Rgb(0, 0, 0), 0.35);
        let phosphor = theme.green;
        Self {
            theme,
            backdrop,
            phosphor,
            glow: std::array::from_fn(|i| mix(backdrop, phosphor, i as f32 / 10.0)),
            hot: mix(phosphor, Rgb(255, 255, 255), 0.55),
            alert: theme.red,
        }
    }

    fn level(&self, level: f32) -> Rgb {
        mix(self.backdrop, self.phosphor, level.clamp(0.0, 1.0))
    }
}

/// Rounds a coordinate to the `FETCH_GRID`.
fn coarse(deg: f32) -> f32 {
    (deg / FETCH_GRID).round() * FETCH_GRID
}

/// How the map lies on the scope: where its center is and which compass
/// bearing is at the top.
pub struct View {
    center: (f32, f32),
    up: f32,
    sin: f32,
    cos: f32,
}

impl View {
    pub fn new(center: (f32, f32), up: f32) -> Self {
        let up = up.rem_euclid(360.0);
        let (sin, cos) = up.to_radians().sin_cos();
        Self {
            center,
            up,
            sin,
            cos,
        }
    }

    /// Screen position of a latitude/longitude (local flat-earth
    /// projection, turned so `up` points to the top).
    pub fn project(&self, (lat, lon): (f32, f32)) -> (f32, f32) {
        let (clat, clon) = self.center;
        let north = (lat - clat) * 60.0;
        let east = (lon - clon) * 60.0 * clat.to_radians().cos();
        let right = east * self.cos - north * self.sin;
        let ahead = east * self.sin + north * self.cos;
        (C + right * PX_PER_NM, C - ahead * PX_PER_NM)
    }

    /// The screen angle (clockwise from the top) a compass bearing points.
    pub fn turn(&self, bearing: f32) -> f32 {
        bearing - self.up
    }
}

fn in_scope((x, y): (f32, f32)) -> bool {
    (x - C).hypot(y - C) <= SCOPE_R - 4.0
}

/// Where the aircraft is now, carried forward from its last report along
/// its track at its ground speed.
fn dead_reckon(ac: &adsb::Aircraft, now: Instant) -> (f32, f32) {
    let (Some(gs), Some(track)) = (ac.gs, ac.track) else {
        return (ac.lat, ac.lon);
    };
    let secs = now
        .saturating_duration_since(ac.fixed_at)
        .as_secs_f32()
        .min(MAX_EXTRAPOLATION);
    ahead(ac.lat, ac.lon, gs, track, secs)
}

/// The position `secs` ahead at `gs` knots on `track` degrees.
fn ahead(lat: f32, lon: f32, gs: f32, track: f32, secs: f32) -> (f32, f32) {
    let nm = gs * secs / 3600.0;
    let (sin, cos) = track.to_radians().sin_cos();
    (
        lat + nm * cos / 60.0,
        lon + nm * sin / (60.0 * lat.to_radians().cos()),
    )
}

/// Point at `bearing` degrees (clockwise from the top) and `r` pixels out.
fn polar(bearing: f32, r: f32) -> (f32, f32) {
    let (sin, cos) = bearing.to_radians().sin_cos();
    (C + r * sin, C - r * cos)
}

fn draw_scope(px: &mut Pixmap, p: &Palette, view: &View) {
    // A faint phosphor bloom, brightest at the center.
    let mut fill = paint(p.backdrop, 255);
    if let Some(shader) = RadialGradient::new(
        Point::from_xy(C, C),
        0.0,
        Point::from_xy(C, C),
        SCOPE_R,
        vec![
            GradientStop::new(0.0, color(p.level(0.13), 255)),
            GradientStop::new(1.0, color(p.level(0.04), 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        fill.shader = shader;
    }
    if let Some(disk) = PathBuilder::from_circle(C, C, SCOPE_R) {
        px.fill_path(&disk, &fill, FillRule::Winding, Transform::identity(), None);
    }

    // Range rings, with the 10 nm ones a step brighter.
    let mut nm = RING_NM;
    while nm < RANGE_NM {
        let major = nm % 10.0 == 0.0;
        circle(px, nm * PX_PER_NM, 1.0, p.glow[if major { 3 } else { 2 }]);
        if major {
            let (x, y) = polar(45.0, nm * PX_PER_NM);
            let label = format!("{nm:.0}");
            text_aligned(
                px,
                &p.theme.font,
                &label,
                12.0,
                x + 3.0,
                y - 3.0,
                p.glow[5],
                Align::Left,
            );
        }
        nm += RING_NM;
    }
    // Crosshair.
    for b in [0.0, 90.0] {
        let (a, z) = (polar(b, SCOPE_R), polar(b + 180.0, SCOPE_R));
        line(px, a, z, 1.0, p.glow[2], 255, None);
    }
    // Bezel: the edge ring with compass bearing ticks, every 30 degrees
    // labeled, and north marked outside the ring.
    circle(px, SCOPE_R, 2.0, p.glow[7]);
    for deg in (0..360).step_by(5) {
        let len = match deg {
            d if d % 30 == 0 => 12.0,
            d if d % 10 == 0 => 7.0,
            _ => 4.0,
        };
        let b = view.turn(deg as f32);
        line(
            px,
            polar(b, SCOPE_R),
            polar(b, SCOPE_R - len),
            1.5,
            p.glow[6],
            255,
            None,
        );
        if deg % 30 == 0 {
            let (x, y) = polar(b, SCOPE_R - 24.0);
            let (label, size, c) = match deg {
                0 => ("N".to_string(), 14.0, p.hot),
                _ => (format!("{deg:03}"), 11.0, p.glow[5]),
            };
            text_aligned(
                px,
                &p.theme.font,
                &label,
                size,
                x,
                y + size / 2.8,
                c,
                Align::Center,
            );
        }
    }
    let north = view.turn(0.0);
    let mut pb = PathBuilder::new();
    let tip = polar(north, SCOPE_R + 2.0);
    pb.move_to(tip.0, tip.1);
    for (side, r) in [(-3.0, SCOPE_R + 11.0), (3.0, SCOPE_R + 11.0)] {
        let (x, y) = polar(north + side, r);
        pb.line_to(x, y);
    }
    pb.close();
    if let Some(path) = pb.finish() {
        px.fill_path(
            &path,
            &paint(p.hot, 255),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn draw_map(px: &mut Pixmap, p: &Palette, view: &View, scope: &Mask) {
    let stroke = Stroke {
        width: 1.6,
        line_cap: LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Stroke::default()
    };
    for coast in geo::COAST {
        let mut pb = PathBuilder::new();
        for (i, &pt) in coast.iter().enumerate() {
            let (x, y) = view.project(pt);
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        if let Some(path) = pb.finish() {
            px.stroke_path(
                &path,
                &paint(p.glow[5], 255),
                &stroke,
                Transform::identity(),
                Some(scope),
            );
        }
    }
    // Extended centerlines out along each approach, dashed like a
    // controller's scope.
    let mut dashed = Stroke {
        width: 1.0,
        ..Stroke::default()
    };
    dashed.dash = StrokeDash::new(vec![5.0, 5.0], 0.0);
    for d in runways::directions(view) {
        let reach = runways::APPROACH_NM * PX_PER_NM;
        let (x, y) = d.threshold;
        let mut pb = PathBuilder::new();
        pb.move_to(x, y);
        pb.line_to(x - d.dir.0 * reach, y - d.dir.1 * reach);
        if let Some(path) = pb.finish() {
            px.stroke_path(
                &path,
                &paint(p.glow[4], 255),
                &dashed,
                Transform::identity(),
                Some(scope),
            );
        }
    }
    for rw in geo::RUNWAYS {
        let [a, b] = rw.ends.map(|(_, pos)| view.project(pos));
        line(px, a, b, 3.0, p.glow[9], 255, Some(scope));
    }
    for (name, pos) in AIRPORTS {
        let (x, y) = view.project(*pos);
        text_aligned(
            px,
            &p.theme.font,
            name,
            12.0,
            x - 10.0,
            y + 18.0,
            p.glow[6],
            Align::Right,
        );
    }
}

fn draw_sweep(px: &mut Pixmap, p: &Palette, beam: f32) {
    // The gradient runs clockwise from the +x axis, so rotate it to put its
    // bright end on the beam and let it fade over the trail behind.
    let ph = p.phosphor;
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(ph.0, ph.1, ph.2, 0)),
        GradientStop::new(1.0 - TRAIL, Color::from_rgba8(ph.0, ph.1, ph.2, 0)),
        GradientStop::new(0.97, Color::from_rgba8(ph.0, ph.1, ph.2, 55)),
        GradientStop::new(1.0, Color::from_rgba8(ph.0, ph.1, ph.2, 110)),
    ];
    let mut fill = paint(ph, 255);
    if let Some(shader) = SweepGradient::new(
        Point::from_xy(C, C),
        0.0,
        360.0,
        stops,
        SpreadMode::Pad,
        Transform::from_rotate_at(beam - 90.0, C, C),
    ) {
        fill.shader = shader;
    }
    // Fill only the wedge the trail covers; the rest would be transparent.
    let mut pb = PathBuilder::new();
    pb.move_to(C, C);
    let span = TRAIL * 360.0;
    for i in 0..=24 {
        let (x, y) = polar(beam - span + span * i as f32 / 24.0, SCOPE_R - 1.0);
        pb.line_to(x, y);
    }
    pb.close();
    if let Some(wedge) = pb.finish() {
        px.fill_path(
            &wedge,
            &fill,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    // The beam itself, with a soft halo.
    let tip = polar(beam, SCOPE_R - 1.0);
    line(px, (C, C), tip, 7.0, ph, 40, None);
    line(px, (C, C), tip, 2.0, p.hot, 255, None);
    dot(px, (C, C), 4.0, p.hot, 255);
}

#[allow(clippy::too_many_arguments)]
fn draw_track(
    px: &mut Pixmap,
    p: &Palette,
    view: &View,
    scope: &Mask,
    track: &Track,
    pos: (f32, f32),
    beam: f32,
    pulse: f32,
) {
    let ac = &track.ac;
    let (x, y) = view.project(pos);
    let ice = is_icelandair(&ac.ident) && !ac.emergency;
    // Brightest just after the beam passes, fading until the next pass.
    // Icelandair stays near full brightness all the way round.
    let bearing = (x - C).atan2(C - y).to_degrees().rem_euclid(360.0);
    let since = (beam - bearing).rem_euclid(360.0) / 360.0;
    let fade = 1.0 - if ice { 0.25 } else { 0.6 } * since;
    let base = match (ac.emergency, ice) {
        (true, _) => p.alert,
        (_, true) => GOLD,
        _ => p.phosphor,
    };
    let lit = mix(p.backdrop, base, fade);

    if ac.alt.is_none() && !ac.emergency {
        // On the ground: a dim dot, no label; Icelandair's a little bigger,
        // so its fleet at the gates shows up in gold.
        let (r, level) = if ice { (3.5, 0.8) } else { (2.5, 0.5) };
        dot(px, (x, y), r, mix(p.backdrop, base, level * fade), 255);
        return;
    }

    for (i, &old) in track.history.iter().enumerate() {
        let age = (track.history.len() - i) as f32 / (HISTORY as f32 + 1.0);
        let c = mix(p.backdrop, base, 0.65 * (1.0 - age));
        let (hx, hy) = view.project(old);
        if let Some(rect) = tiny_skia::Rect::from_xywh(hx - 1.5, hy - 1.5, 3.0, 3.0) {
            px.fill_rect(rect, &paint(c, 255), Transform::identity(), Some(scope));
        }
    }
    if let (Some(gs), Some(trk)) = (ac.gs, ac.track) {
        let tip = view.project(ahead(pos.0, pos.1, gs, trk, VECTOR_SECS));
        line(
            px,
            (x, y),
            tip,
            1.5,
            mix(p.backdrop, base, 0.6 * fade + 0.2),
            255,
            Some(scope),
        );
    }

    if ice {
        // A beacon ring expanding and fading out, then the halo and an
        // airliner pointing along its track.
        let eased = 1.0 - (1.0 - pulse).powi(2);
        ring(
            px,
            (x, y),
            8.0 + 18.0 * eased,
            1.6,
            GOLD,
            (200.0 * (1.0 - pulse)) as u8,
        );
        dot(px, (x, y), 14.0, GOLD, (60.0 * fade) as u8);
        dot(px, (x, y), 8.0, GOLD, (90.0 * fade) as u8);
        airliner(
            px,
            (x, y),
            view.turn(ac.track.unwrap_or(0.0)),
            mix(GOLD, Rgb(255, 255, 255), 0.25),
            p.backdrop,
        );
    } else {
        // Halo, then a square target symbol.
        dot(px, (x, y), 11.0, base, (70.0 * fade) as u8);
        dot(px, (x, y), 6.0, base, (110.0 * fade) as u8);
        target(px, (x, y), mix(lit, Rgb(255, 255, 255), 0.4 * fade));
    }

    // Data block behind the aircraft, clear of its velocity vector, unless
    // that side runs off the scope.
    let heading_right = ac
        .track
        .is_some_and(|t| view.turn(t).to_radians().sin() > 0.0);
    let right = match (x < C - 120.0, x > C + 120.0) {
        (true, _) => true,
        (_, true) => false,
        _ => !heading_right,
    };
    let (lx, align) = if right {
        (x + 16.0, Align::Left)
    } else {
        (x - 16.0, Align::Right)
    };
    let ly = y - 12.0;
    line(
        px,
        (x + if right { 5.0 } else { -5.0 }, y - 5.0),
        (lx, ly + 3.0),
        1.0,
        p.glow[6],
        255,
        None,
    );
    let ident_color = match (ac.emergency, ice) {
        (true, _) => p.alert,
        (_, true) => GOLD,
        _ => p.hot,
    };
    text_aligned(
        px,
        &p.theme.font,
        &ac.ident,
        14.0,
        lx,
        ly,
        ident_color,
        align,
    );
    text_aligned(
        px,
        &p.theme.font,
        &data_line(ac, &p.theme.font),
        12.0,
        lx,
        ly + 14.0,
        lit,
        align,
    );
}

/// A square target symbol, the mark for ordinary traffic.
fn target(px: &mut Pixmap, (x, y): (f32, f32), c: Rgb) {
    let s = 4.0;
    let Some(rect) = tiny_skia::Rect::from_xywh(x - s, y - s, 2.0 * s, 2.0 * s) else {
        return;
    };
    let path = PathBuilder::from_rect(rect);
    let stroke = Stroke {
        width: 1.8,
        ..Stroke::default()
    };
    px.stroke_path(&path, &paint(c, 255), &stroke, Transform::identity(), None);
}

/// A top-down airliner centered on the point, nose along `track` degrees,
/// outlined in `edge` so it reads over the halo.
fn airliner(px: &mut Pixmap, (x, y): (f32, f32), track: f32, fill: Rgb, edge: Rgb) {
    // The right half, nose up, from nose to tail; mirrored for the left.
    const HALF: &[(f32, f32)] = &[
        (0.0, -11.0),
        (1.3, -9.5),
        (1.5, -3.0),
        (10.0, 2.5),
        (10.0, 4.2),
        (1.5, 1.8),
        (1.2, 6.8),
        (4.5, 9.2),
        (4.5, 10.6),
        (0.0, 9.6),
    ];
    let mut pb = PathBuilder::new();
    pb.move_to(HALF[0].0, HALF[0].1);
    for &(hx, hy) in &HALF[1..] {
        pb.line_to(hx, hy);
    }
    for &(hx, hy) in HALF[1..HALF.len() - 1].iter().rev() {
        pb.line_to(-hx, hy);
    }
    pb.close();
    let Some(path) = pb.finish() else { return };
    let at = Transform::from_rotate(track).post_translate(x, y);
    let stroke = Stroke {
        width: 2.0,
        line_join: tiny_skia::LineJoin::Round,
        ..Stroke::default()
    };
    px.stroke_path(&path, &paint(edge, 255), &stroke, at, None);
    px.fill_path(&path, &paint(fill, 255), FillRule::Winding, at, None);
}

/// "035↓ 102": altitude in hundreds of feet, climb or descent, ground speed.
fn data_line(ac: &adsb::Aircraft, font: &ab_glyph::FontVec) -> String {
    use ab_glyph::Font;
    let alt = ac
        .alt
        .map_or("GND".into(), |a| format!("{:03}", (a.max(0) + 50) / 100));
    let arrow = |fancy: char, plain: char| {
        if font.glyph_id(fancy).0 != 0 {
            fancy
        } else {
            plain
        }
    };
    let trend = match ac.vrate {
        Some(v) if v > 300 => arrow('↑', '+'),
        Some(v) if v < -300 => arrow('↓', '-'),
        _ => ' ',
    };
    let gs = ac.gs.map_or(String::new(), |g| format!(" {:.0}", g));
    format!("{alt}{trend}{gs}")
}

#[allow(clippy::too_many_arguments)]
fn draw_hud(
    px: &mut Pixmap,
    p: &Palette,
    status: Status,
    airborne: usize,
    icelandair: usize,
    movements: &[(runways::Movement, &str)],
    utc: DateTime<Utc>,
    up: Duration,
) {
    let font = &p.theme.font;
    let (l, r, t, b) = (12.0, SIZE as f32 - 12.0, 22.0, SIZE as f32 - 12.0);
    text_aligned(px, font, "SURV RADAR", 14.0, l, t, p.glow[9], Align::Left);
    text_aligned(
        px,
        font,
        "BIKF  BIRK",
        12.0,
        l,
        t + 16.0,
        p.glow[5],
        Align::Left,
    );
    text_aligned(
        px,
        font,
        &utc.format("%H:%M:%SZ").to_string(),
        14.0,
        r,
        t,
        p.glow[9],
        Align::Right,
    );
    text_aligned(
        px,
        font,
        &utc.format("%d %b").to_string().to_uppercase(),
        12.0,
        r,
        t + 16.0,
        p.glow[5],
        Align::Right,
    );

    let blink = (up.as_millis() / 500).is_multiple_of(2);
    let (label, c) = match status {
        Status::Live => (format!("TGT {airborne:02}"), p.glow[9]),
        Status::Acquiring => (
            "ACQUIRING".into(),
            if blink { p.glow[9] } else { p.glow[4] },
        ),
        Status::NoLink => ("NO LINK".into(), if blink { p.alert } else { p.backdrop }),
    };
    text_aligned(px, font, &label, 14.0, l, b - 16.0, c, Align::Left);
    text_aligned(px, font, "ADSB.FI", 12.0, l, b, p.glow[5], Align::Left);
    if status == Status::Live && icelandair > 0 {
        text_aligned(
            px,
            font,
            &format!("ICE {icelandair:02}"),
            14.0,
            l,
            b - 32.0,
            GOLD,
            Align::Left,
        );
    }
    // Arrivals and departures stack up from above the range readout.
    for (i, (m, ident)) in movements.iter().enumerate() {
        let kind = match m.kind {
            runways::Kind::Arrival => "ARR",
            runways::Kind::Departure => "DEP",
        };
        let c = if is_icelandair(ident) {
            GOLD
        } else {
            p.glow[8]
        };
        text_aligned(
            px,
            font,
            &format!("{kind} {ident} {} {}", m.airport, m.runway),
            12.0,
            r,
            b - 36.0 - i as f32 * 15.0,
            c,
            Align::Right,
        );
    }
    text_aligned(
        px,
        font,
        &format!("RNG {RANGE_NM:.0} NM"),
        14.0,
        r,
        b - 16.0,
        p.glow[9],
        Align::Right,
    );
    text_aligned(
        px,
        font,
        &format!("{:.0} NM RINGS", RING_NM),
        12.0,
        r,
        b,
        p.glow[5],
        Align::Right,
    );
}

/// Darkens every third row a little, like the raster of an old CRT.
fn scanlines(px: &mut Pixmap) {
    let w = px.width() as usize * 4;
    for row in px.data_mut().chunks_exact_mut(w).step_by(3) {
        for c in row.as_chunks_mut::<4>().0 {
            for v in &mut c[..3] {
                *v = (*v as u16 * 7 / 8) as u8;
            }
        }
    }
}

fn color(c: Rgb, alpha: u8) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, alpha)
}

fn paint(c: Rgb, alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(c.0, c.1, c.2, alpha);
    paint.anti_alias = true;
    paint
}

#[allow(clippy::too_many_arguments)]
fn line(
    px: &mut Pixmap,
    a: (f32, f32),
    b: (f32, f32),
    width: f32,
    c: Rgb,
    alpha: u8,
    mask: Option<&Mask>,
) {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    let Some(path) = pb.finish() else { return };
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    px.stroke_path(
        &path,
        &paint(c, alpha),
        &stroke,
        Transform::identity(),
        mask,
    );
}

fn circle(px: &mut Pixmap, r: f32, width: f32, c: Rgb) {
    let Some(path) = PathBuilder::from_circle(C, C, r) else {
        return;
    };
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    px.stroke_path(&path, &paint(c, 255), &stroke, Transform::identity(), None);
}

fn ring(px: &mut Pixmap, (x, y): (f32, f32), r: f32, width: f32, c: Rgb, alpha: u8) {
    let Some(path) = PathBuilder::from_circle(x, y, r) else {
        return;
    };
    let stroke = Stroke {
        width,
        ..Stroke::default()
    };
    px.stroke_path(
        &path,
        &paint(c, alpha),
        &stroke,
        Transform::identity(),
        None,
    );
}

fn dot(px: &mut Pixmap, (x, y): (f32, f32), r: f32, c: Rgb, alpha: u8) {
    if let Some(path) = PathBuilder::from_circle(x, y, r) {
        px.fill_path(
            &path,
            &paint(c, alpha),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_around_center() {
        let view = View::new(CENTER, 0.0);
        let (x, y) = view.project(CENTER);
        assert!((x - C).abs() < 0.01 && (y - C).abs() < 0.01);
        // 10 nm north is 10 nm worth of pixels up.
        let (_, y) = view.project((CENTER.0 + 10.0 / 60.0, CENTER.1));
        assert!((C - y - 10.0 * PX_PER_NM).abs() < 0.01);
        // Both airports are on the scope.
        for (_, pos) in AIRPORTS {
            assert!(in_scope(view.project(*pos)));
        }
    }

    #[test]
    fn turns_the_map() {
        let view = View::new(CENTER, 111.0);
        let near = |(x, y): (f32, f32), (ex, ey): (f32, f32)| {
            assert!((x - ex).abs() < 0.05 && (y - ey).abs() < 0.05, "{x},{y}");
        };
        let ten_nm = 10.0 * PX_PER_NM;
        // 10 nm out on bearing 111 is straight up, on 201 to the right.
        near(
            view.project(ahead(CENTER.0, CENTER.1, 600.0, 111.0, 60.0)),
            (C, C - ten_nm),
        );
        near(
            view.project(ahead(CENTER.0, CENTER.1, 600.0, 201.0, 60.0)),
            (C + ten_nm, C),
        );
        assert_eq!(view.turn(111.0), 0.0);
        assert_eq!(View::new(CENTER, -90.0).up, 270.0);
    }

    #[test]
    fn dead_reckons_along_track() {
        // 60 knots due east for a minute is one nautical mile east.
        let (lat, lon) = ahead(64.0, -22.0, 60.0, 90.0, 60.0);
        assert!((lat - 64.0).abs() < 1e-4);
        let east_nm = (lon + 22.0) * 60.0 * 64f32.to_radians().cos();
        assert!((east_nm - 1.0).abs() < 1e-3);
    }

    #[test]
    fn spots_arrivals_and_departures() {
        for heading in [0.0, 111.0] {
            spots_movements(&View::new(CENTER, heading));
        }
    }

    fn spots_movements(view: &View) {
        let now = Instant::now();
        let at =
            |ac: &adsb::Aircraft| runways::classify(ac, view.project(dead_reckon(ac, now)), view);
        let finals = adsb::Aircraft {
            hex: "4cc2a1".into(),
            ident: "ICE614".into(),
            // About 2.7 nm short of Keflavik's runway 01, heading north.
            lat: 63.92,
            lon: -22.6,
            alt: Some(1400),
            gs: None,
            track: Some(2.0),
            vrate: Some(-700),
            emergency: false,
            fixed_at: now,
        };
        let arrival = at(&finals).unwrap();
        assert_eq!(arrival.kind, runways::Kind::Arrival);
        assert_eq!((arrival.airport, arrival.runway), ("BIKF", "01"));

        let climbing = adsb::Aircraft {
            vrate: Some(1500),
            ..finals.clone()
        };
        assert_eq!(at(&climbing), None, "climbing short of the runway");
        let cruising = adsb::Aircraft {
            alt: Some(34000),
            ..finals.clone()
        };
        assert_eq!(at(&cruising), None);
        let crossing = adsb::Aircraft {
            track: Some(90.0),
            ..finals.clone()
        };
        assert_eq!(at(&crossing), None, "not aligned with the runway");

        // Past the far end of 01, climbing out northbound.
        let departure = adsb::Aircraft {
            lat: 64.02,
            lon: -22.6054,
            alt: Some(1800),
            vrate: Some(2200),
            ..finals
        };
        let m = at(&departure).unwrap();
        assert_eq!(m.kind, runways::Kind::Departure);
        assert_eq!((m.airport, m.runway), ("BIKF", "01"));
    }

    #[test]
    fn fetches_around_a_coarser_center() {
        assert!((coarse(64.04) - 64.0).abs() < 1e-4);
        assert!((coarse(-21.93) + 21.9).abs() < 1e-4);
        // The most the fetch center can be off, plus the scope's reach,
        // stays inside what's fetched.
        let off_nm = FETCH_GRID / 2.0 * 60.0;
        assert!(RANGE_NM + off_nm < FETCH_NM as f32);
    }

    #[test]
    fn spots_icelandair() {
        assert!(is_icelandair("ICE614"));
        assert!(is_icelandair("ICE5TP"));
        assert!(!is_icelandair("ICELAND"));
        assert!(!is_icelandair("PLAY101"));
        assert!(!is_icelandair("TF-ISB"));
    }

    #[test]
    fn polar_bearings() {
        let (x, y) = polar(90.0, 10.0);
        assert!((x - (C + 10.0)).abs() < 1e-3 && (y - C).abs() < 1e-3);
    }
}
