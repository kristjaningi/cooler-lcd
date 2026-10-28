//! A clock for the radar's data ages. `Instant` is CLOCK_MONOTONIC on
//! Linux, which stops while the machine is suspended, so aircraft fetched
//! before a night's sleep would read as seconds old on waking and be drawn,
//! and alerted on, as live traffic. CLOCK_BOOTTIME keeps counting through
//! suspend.

use std::ops::{Add, Sub};
use std::time::Duration;

/// A point in time since boot, suspend included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp(Duration);

impl Stamp {
    pub fn now() -> Self {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // Can only fail for an unknown clock id or a bad pointer; Linux has
        // had CLOCK_BOOTTIME since 2.6.39.
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
        Self(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    }

    pub fn elapsed(self) -> Duration {
        Self::now() - self
    }

    pub fn saturating_duration_since(self, earlier: Stamp) -> Duration {
        self - earlier
    }

    pub fn checked_sub(self, d: Duration) -> Option<Stamp> {
        self.0.checked_sub(d).map(Stamp)
    }
}

impl Add<Duration> for Stamp {
    type Output = Stamp;

    fn add(self, d: Duration) -> Stamp {
        Stamp(self.0 + d)
    }
}

/// The time between two stamps, zero if `earlier` is the later one.
impl Sub for Stamp {
    type Output = Duration;

    fn sub(self, earlier: Stamp) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_pace_with_instant_while_awake() {
        let (stamp, instant) = (Stamp::now(), std::time::Instant::now());
        std::thread::sleep(Duration::from_millis(20));
        let (a, b) = (stamp.elapsed(), instant.elapsed());
        assert!(a >= Duration::from_millis(20));
        assert!(a.abs_diff(b) < Duration::from_millis(5));
    }
}
