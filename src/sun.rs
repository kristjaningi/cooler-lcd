//! Where the sun is in the sky, for night mode's `"sunset"` schedule.
//!
//! NOAA's low-precision formulas (good to well under a degree for
//! centuries either side of 2000), no tables and no crate.

use chrono::{DateTime, Utc};

/// The sun's elevation when it's rising or setting: its center this far
/// below the horizon, which refraction lifts its upper edge into view at.
pub const HORIZON: f64 = -0.833;

/// The sun's elevation above the horizon, in degrees, seen from `lat`,
/// `lon` (degrees, east positive) at `at`. Negative when it's down;
/// refraction isn't taken into account.
pub fn elevation(lat: f64, lon: f64, at: DateTime<Utc>) -> f64 {
    let jd = at.timestamp() as f64 / 86400.0 + 2440587.5;
    // Julian centuries since J2000.
    let t = (jd - 2451545.0) / 36525.0;

    let mean_long = (280.46646 + t * (36000.76983 + t * 0.0003032)).rem_euclid(360.0);
    let mean_anom = 357.52911 + t * (35999.05029 - t * 0.0001537);
    let ecc = 0.016708634 - t * (0.000042037 + t * 0.0000001267);
    let m = mean_anom.to_radians();
    let center = m.sin() * (1.914602 - t * (0.004817 + t * 0.000014))
        + (2.0 * m).sin() * (0.019993 - t * 0.000101)
        + (3.0 * m).sin() * 0.000289;
    let omega = (125.04 - 1934.136 * t).to_radians();
    let app_long = (mean_long + center - 0.00569 - 0.00478 * omega.sin()).to_radians();
    let obliq =
        23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.00059 - t * 0.001813))) / 60.0) / 60.0;
    let obliq = (obliq + 0.00256 * omega.cos()).to_radians();
    let decl = (obliq.sin() * app_long.sin()).asin();

    // Equation of time, in minutes: how far the sun runs ahead of the clock.
    let y = (obliq / 2.0).tan().powi(2);
    let l0 = mean_long.to_radians();
    let eq_time = 4.0
        * (y * (2.0 * l0).sin() - 2.0 * ecc * m.sin() + 4.0 * ecc * y * m.sin() * (2.0 * l0).cos()
            - 0.5 * y * y * (4.0 * l0).sin()
            - 1.25 * ecc * ecc * (2.0 * m).sin())
        .to_degrees();

    let minutes = (at.timestamp().rem_euclid(86400)) as f64 / 60.0;
    let solar = (minutes + eq_time + 4.0 * lon).rem_euclid(1440.0);
    let hour_angle = (solar / 4.0 - 180.0).to_radians();
    let lat = lat.to_radians();
    let cos_zenith = lat.sin() * decl.sin() + lat.cos() * decl.cos() * hour_angle.cos();
    90.0 - cos_zenith.clamp(-1.0, 1.0).acos().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{NaiveDate, TimeDelta, Timelike};

    const REYKJAVIK: (f64, f64) = (64.13, -21.94);
    const TROMSO: (f64, f64) = (69.65, 18.96);

    /// The highest and lowest the sun gets over a UTC day, and when it's
    /// highest.
    fn day(place: (f64, f64), y: i32, m: u32, d: u32) -> (f64, f64, DateTime<Utc>) {
        let start = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let samples = (0..24 * 60).map(|i| start + TimeDelta::minutes(i));
        let (mut high, mut low, mut noon) = (f64::MIN, f64::MAX, start);
        for at in samples {
            let e = elevation(place.0, place.1, at);
            if e > high {
                (high, noon) = (e, at);
            }
            low = low.min(e);
        }
        (high, low, noon)
    }

    #[test]
    fn reykjavik_at_the_solstices() {
        // 90 - 64.13 ± 23.44: high in June, barely up in December.
        let (high, low, noon) = day(REYKJAVIK, 2026, 6, 21);
        assert!((high - 49.3).abs() < 0.3, "{high}");
        // It does set, briefly, around 01:30 UTC.
        assert!(low < HORIZON && low > -3.0, "{low}");
        // Solar noon is an hour and a half after noon UTC, that far west
        // (88 minutes for the longitude, 2 back for the equation of time).
        let minutes = (noon.hour() * 60 + noon.minute()) as i32;
        assert!((minutes - (13 * 60 + 29)).abs() <= 2, "{noon}");
        let (high, ..) = day(REYKJAVIK, 2026, 12, 21);
        assert!((high - 2.4).abs() < 0.3, "{high}");
    }

    #[test]
    fn tromso_has_midnight_sun_and_polar_night() {
        let (_, low, _) = day(TROMSO, 2026, 6, 21);
        assert!(low > 2.5, "{low}");
        let (high, ..) = day(TROMSO, 2026, 12, 21);
        assert!(high < HORIZON, "{high}");
    }
}
