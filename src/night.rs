//! Night mode: when the panel dims, so it doesn't glow across a dark room.
//! The config's schedule decides, unless `cooler-lcd night on` or `off`
//! says otherwise; that lasts until the schedule next changes its mind
//! (or `night auto`), so a panel kept bright one evening dims again the
//! next.

use chrono::{DateTime, Local, Utc};

use crate::config::{Config, Schedule};
use crate::sun;

/// What `cooler-lcd night` asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    On,
    Off,
    Auto,
}

pub struct Night {
    schedule: Schedule,
    /// Where the sun is worked out from.
    at: (f64, f64),
    /// An override from `night on` or `off`, and what the schedule said
    /// when it was given: once that changes, the override lapses.
    forced: Option<(bool, bool)>,
}

impl Night {
    pub fn new(config: &Config) -> Self {
        let (lat, lon) = config.radar.center();
        Self {
            schedule: config.night.schedule,
            at: (lat as f64, lon as f64),
            forced: None,
        }
    }

    /// Whether the panel should be dim at `now`. Drops an override the
    /// schedule has moved past, so call it often (every frame is fine).
    pub fn dark(&mut self, now: DateTime<Local>) -> bool {
        let scheduled = self.scheduled(now);
        match self.forced {
            Some((dark, given_at)) if given_at == scheduled => dark,
            _ => {
                self.forced = None;
                scheduled
            }
        }
    }

    /// Carries out `cooler-lcd night [on|off|auto]`, and says how things
    /// stand.
    pub fn command(&mut self, mode: Option<Mode>, now: DateTime<Local>) -> String {
        let scheduled = self.scheduled(now);
        match mode {
            Some(Mode::On) => self.forced = Some((true, scheduled)),
            Some(Mode::Off) => self.forced = Some((false, scheduled)),
            Some(Mode::Auto) => self.forced = None,
            None => {}
        }
        let dark = self.dark(now);
        let state = if dark { "dimmed" } else { "not dimmed" };
        // It stays this way until the schedule next turns the other way.
        let until = match (self.schedule, dark) {
            (Schedule::Hours(_, to), true) => format!(" until {}", to.format("%H:%M")),
            (Schedule::Hours(from, _), false) => format!(" until {}", from.format("%H:%M")),
            (Schedule::Sunset, true) => " until sunrise".into(),
            (Schedule::Sunset, false) => " until sunset".into(),
            (Schedule::Never, _) if self.forced.is_some() => " until `night auto`".into(),
            (Schedule::Never, _) => " (the schedule is \"never\")".into(),
        };
        let how = match self.forced {
            Some(_) if mode.is_none() => {
                format!(" (by `night {}`)", if dark { "on" } else { "off" })
            }
            _ => String::new(),
        };
        format!("{state}{until}{how}")
    }

    fn scheduled(&self, now: DateTime<Local>) -> bool {
        match self.schedule {
            Schedule::Never => false,
            Schedule::Sunset => {
                sun::elevation(self.at.0, self.at.1, now.with_timezone(&Utc)) < sun::HORIZON
            }
            // The local clock's time of day, so it follows DST.
            Schedule::Hours(from, to) => {
                let t = now.time();
                if from < to {
                    from <= t && t < to
                } else {
                    t >= from || t < to
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{NaiveTime, TimeZone};

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 1, 15, h, m, 0).unwrap()
    }

    fn night(schedule: Schedule) -> Night {
        let mut config = Config::default();
        config.night.schedule = schedule;
        Night::new(&config)
    }

    fn hours(from: u32, to: u32) -> Schedule {
        let t = |h| NaiveTime::from_hms_opt(h, 0, 0).unwrap();
        Schedule::Hours(t(from), t(to))
    }

    #[test]
    fn hours_run_across_midnight() {
        let mut n = night(hours(22, 7));
        assert!(!n.dark(at(21, 59)));
        assert!(n.dark(at(22, 0)));
        assert!(n.dark(at(3, 0)));
        assert!(n.dark(at(6, 59)));
        assert!(!n.dark(at(7, 0)));
        let mut n = night(hours(1, 5));
        assert!(!n.dark(at(0, 30)));
        assert!(n.dark(at(1, 0)));
        assert!(!n.dark(at(5, 0)));
    }

    #[test]
    fn an_override_lasts_until_the_schedule_changes() {
        let mut n = night(hours(22, 7));
        // Dimmed in the afternoon: still dim through the night, then the
        // schedule has it again.
        assert_eq!(n.command(Some(Mode::On), at(15, 0)), "dimmed until 07:00");
        assert!(n.dark(at(21, 0)));
        assert!(n.dark(at(23, 0)));
        assert!(n.forced.is_none(), "lapsed at 22:00");
        assert!(!n.dark(at(7, 0)));

        // Kept bright one evening, dim again the next.
        let mut n = night(hours(22, 7));
        assert!(n.dark(at(23, 0)));
        assert_eq!(
            n.command(Some(Mode::Off), at(23, 0)),
            "not dimmed until 22:00"
        );
        assert!(!n.dark(at(23, 30)));
        assert_eq!(
            n.command(None, at(23, 30)),
            "not dimmed until 22:00 (by `night off`)"
        );
        assert!(!n.dark(at(7, 0)));
        assert!(n.dark(at(22, 0)));

        // `auto` hands it back at once.
        let mut n = night(hours(22, 7));
        n.command(Some(Mode::On), at(12, 0));
        assert_eq!(
            n.command(Some(Mode::Auto), at(12, 0)),
            "not dimmed until 22:00"
        );
        assert!(!n.dark(at(12, 1)));
    }

    #[test]
    fn with_no_schedule_an_override_sticks() {
        let mut n = night(Schedule::Never);
        assert!(!n.dark(at(23, 0)));
        assert_eq!(
            n.command(None, at(23, 0)),
            "not dimmed (the schedule is \"never\")"
        );
        assert_eq!(
            n.command(Some(Mode::On), at(23, 0)),
            "dimmed until `night auto`"
        );
        assert!(n.dark(at(12, 0)));
        assert!(n.dark(at(23, 0)));
    }

    #[test]
    fn sunset_follows_the_sun() {
        let mut n = night(Schedule::Sunset);
        n.at = (64.13, -21.94);
        let utc = |mo, d, h| {
            Utc.with_ymd_and_hms(2026, mo, d, h, 0, 0)
                .unwrap()
                .with_timezone(&Local)
        };
        assert!(!n.dark(utc(6, 21, 13)));
        assert!(n.dark(utc(6, 21, 1)), "the short June night");
        assert!(n.dark(utc(12, 21, 9)), "December morning");
        assert!(!n.dark(utc(12, 21, 13)));
        assert_eq!(n.command(None, utc(12, 21, 13)), "not dimmed until sunset");
    }
}
