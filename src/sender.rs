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

/// An overlay's first successfully transmitted frame, shared across all
/// its redraws so replacing an unsent frame cannot lose its acknowledgement.
#[derive(Clone, Default)]
pub struct Receipt(Arc<Mutex<Option<Instant>>>);

impl Receipt {
    pub fn sent_at(&self) -> Option<Instant> {
        *self.0.lock().unwrap()
    }
    pub fn mark_sent(&self) {
        self.0.lock().unwrap().get_or_insert_with(Instant::now);
    }
}

struct Frame {
    bytes: Vec<u8>,
    receipt: Option<Receipt>,
}

#[derive(Default)]
struct Slot {
    /// The latest frame (header + JPEG).
    frame: Option<Arc<Frame>>,
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
    pub fn publish(&self, frame: &[u8], receipt: Option<Receipt>) {
        let (slot, wake) = &*self.shared;
        let mut slot = slot.lock().unwrap();
        slot.frame = Some(Arc::new(Frame {
            bytes: frame.to_vec(),
            receipt,
        }));
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

            if let Some(frame) = frame {
                if let Err(e) = panel.send(&frame.bytes) {
                    log.error(format!("{e:#}; reconnecting"));
                    break;
                }
                if let Some(receipt) = &frame.receipt {
                    receipt.mark_sent();
                }
            }
        }
        set_connected(shared, false);
    }
}

/// Waits for a newly published frame, or `KEEPALIVE` at most, and returns
/// the latest frame either way.
fn next_frame(shared: &Shared) -> Option<Arc<Frame>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_redraw_keeps_the_same_delivery_receipt() {
        let sender = Sender {
            shared: Shared::default(),
        };
        let receipt = Receipt::default();
        sender.publish(b"first", Some(receipt.clone()));
        sender.publish(b"redrawn", Some(receipt.clone()));
        let frame = next_frame(&sender.shared).unwrap();
        assert_eq!(frame.bytes, b"redrawn");
        assert!(
            receipt.sent_at().is_none(),
            "publishing and selecting aren't delivery"
        );
        frame.receipt.as_ref().unwrap().mark_sent();
        let first = receipt.sent_at().unwrap();
        frame.receipt.as_ref().unwrap().mark_sent();
        assert_eq!(
            receipt.sent_at(),
            Some(first),
            "keepalive must not restart the lifetime"
        );
    }
}
