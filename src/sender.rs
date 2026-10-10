//! Keeps the panel fed from a thread of its own.
//!
//! The firmware falls back to its logo after ~2-3 s without a frame, so the
//! latest frame is resent every second whatever the frame loop is doing: a
//! slow sensor (NVML after resume), a theme reload running fontconfig, a big
//! usage reload. This thread also owns the connection: it reconnects when the
//! cooler is unplugged, a write fails, or the machine wakes from suspend.

use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::device::Panel;
use crate::log::Log;

/// The longest the panel goes without a frame.
const KEEPALIVE: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(3);
/// Wall clock running this far ahead of the monotonic clock means we slept;
/// the USB handle may be stale, so reconnect.
const WAKE_GAP: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Slot {
    /// The latest frame (header + JPEG).
    frame: Option<Arc<Vec<u8>>>,
    /// Published since the sender last picked it up.
    fresh: bool,
    connected: bool,
}

type Shared = Arc<(Mutex<Slot>, Condvar)>;

pub struct Sender {
    shared: Shared,
}

impl Sender {
    pub fn spawn() -> Self {
        let shared = Shared::default();
        let s = shared.clone();
        thread::spawn(move || run(&s));
        Self { shared }
    }

    /// Hands a new frame to the sender, which sends it right away.
    pub fn publish(&self, frame: &[u8]) {
        let (slot, wake) = &*self.shared;
        let mut slot = slot.lock().unwrap();
        slot.frame = Some(Arc::new(frame.to_vec()));
        slot.fresh = true;
        wake.notify_one();
    }

    /// Whether the panel is connected; there's no point drawing otherwise.
    pub fn connected(&self) -> bool {
        self.shared.0.lock().unwrap().connected
    }
}

fn run(shared: &Shared) {
    let mut log = Log::default();
    loop {
        let mut panel = match Panel::open() {
            Ok(p) => p,
            Err(e) => {
                log.error(format!("{e:#}"));
                thread::sleep(RETRY);
                continue;
            }
        };
        log.info(format!("connected to screen (PM={})", panel.pm));
        set_connected(shared, true);

        let mut clocks = (SystemTime::now(), Instant::now());
        loop {
            let frame = next_frame(shared);

            let now = (SystemTime::now(), Instant::now());
            let wall = now.0.duration_since(clocks.0).unwrap_or_default();
            if wall.saturating_sub(now.1 - clocks.1) > WAKE_GAP {
                log.info("woke from sleep; reconnecting".into());
                break;
            }
            clocks = now;

            if let Some(frame) = frame
                && let Err(e) = panel.send(&frame)
            {
                log.error(format!("{e:#}; reconnecting"));
                break;
            }
        }
        set_connected(shared, false);
    }
}

/// Waits for a newly published frame, or `KEEPALIVE` at most, and returns
/// the latest frame either way.
fn next_frame(shared: &Shared) -> Option<Arc<Vec<u8>>> {
    let (slot, wake) = &**shared;
    let mut slot = slot.lock().unwrap();
    if !slot.fresh {
        slot = wake.wait_timeout(slot, KEEPALIVE).unwrap().0;
    }
    slot.fresh = false;
    slot.frame.clone()
}

fn set_connected(shared: &Shared, connected: bool) {
    shared.0.lock().unwrap().connected = connected;
}
