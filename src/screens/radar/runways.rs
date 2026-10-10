//! Which runway an aircraft is landing on or taking off from, judged from
//! where it is and where it's going relative to each runway's extended
//! centerline. Worked out in screen space, which the projection keeps
//! true to shape this close to the center.

use super::{PX_PER_NM, View, adsb, geo};

/// Approaches and climb-outs are followed below this, feet.
const MAX_ALT: i32 = 4000;
/// How far out the approach is followed, and the centerline drawn, nm.
pub const APPROACH_NM: f32 = 10.0;
/// How far past the far end of the runway a climb-out is followed, nm.
const CLIMB_OUT_NM: f32 = 6.0;
/// Distance off the centerline still counted as on it, nm.
const LATERAL_NM: f32 = 1.5;
/// Difference between track and runway heading still counted as aligned.
const ALIGNED_DEG: f32 = 20.0;
/// Climb rate that makes it a departure, feet per minute.
const CLIMBING: i32 = 300;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Arrival,
    Departure,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Movement {
    pub kind: Kind,
    pub airport: &'static str,
    pub runway: &'static str,
}

/// A runway used in one direction: landing or taking off on `ident`
/// starts at `threshold` and runs along the unit vector `dir`.
pub struct Direction {
    airport: &'static str,
    ident: &'static str,
    pub threshold: (f32, f32),
    pub dir: (f32, f32),
    length_nm: f32,
}

/// Both directions of every runway, in screen space.
pub fn directions(view: &View) -> impl Iterator<Item = Direction> {
    geo::RUNWAYS.iter().flat_map(|rw| {
        let [(ident_a, a), (ident_b, b)] = rw.ends;
        let (a, b) = (view.project(a), view.project(b));
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = dx.hypot(dy);
        let dir = (dx / len, dy / len);
        let length_nm = len / PX_PER_NM;
        [
            Direction {
                airport: rw.airport,
                ident: ident_a,
                threshold: a,
                dir,
                length_nm,
            },
            Direction {
                airport: rw.airport,
                ident: ident_b,
                threshold: b,
                dir: (-dir.0, -dir.1),
                length_nm,
            },
        ]
    })
}

/// The approach or climb-out `ac` is on, given its screen position.
pub fn classify(ac: &adsb::Aircraft, (x, y): (f32, f32), view: &View) -> Option<Movement> {
    let alt = ac.alt?;
    let track = view.turn(ac.track?).to_radians();
    if alt >= MAX_ALT {
        return None;
    }
    let climbing = ac.vrate.unwrap_or(0) >= CLIMBING;
    let heading = (track.sin(), -track.cos());
    let min_alignment = ALIGNED_DEG.to_radians().cos();
    directions(view).find_map(|d| {
        if d.dir.0 * heading.0 + d.dir.1 * heading.1 < min_alignment {
            return None;
        }
        let rx = (x - d.threshold.0) / PX_PER_NM;
        let ry = (y - d.threshold.1) / PX_PER_NM;
        let along = rx * d.dir.0 + ry * d.dir.1;
        let lateral = (rx * d.dir.1 - ry * d.dir.0).abs();
        if lateral > LATERAL_NM {
            return None;
        }
        let kind = if !climbing && (-APPROACH_NM..=d.length_nm).contains(&along) {
            Kind::Arrival
        } else if climbing && (0.0..=d.length_nm + CLIMB_OUT_NM).contains(&along) {
            Kind::Departure
        } else {
            return None;
        };
        Some(Movement {
            kind,
            airport: d.airport,
            runway: d.ident,
        })
    })
}
