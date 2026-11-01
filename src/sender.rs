//! Keeps the panel fed from a thread of its own.
//!
//! The firmware falls back to its logo after ~2-3 s without a frame, so the
//! latest frame is resent every second whatever the frame loop is doing: a
//! slow sensor (NVML after resume), a theme reload running fontconfig, a big
//! usage reload. This thread also owns the connection: it reconnects when the
//! cooler is unplugged, a write fails, or the machine wakes from suspend.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
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
pub struct Receipt(Arc<OnceLock<Instant>>);

impl Receipt {
    pub fn sent_at(&self) -> Option<Instant> {
        self.0.get().copied()
    }
    pub fn mark_sent(&self) {
        self.0.get_or_init(Instant::now);
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
    generation: u64,
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

    /// Queues a frame only if the connection it was drawn for is still current.
    pub fn publish(&self, connection: u64, frame: &[u8], receipt: Option<Receipt>) -> bool {
        let (slot, wake) = &*self.shared;
        let mut slot = slot.lock().unwrap();
        if !slot.connected || slot.generation != connection {
            return false;
        }
        slot.frame = Some(Arc::new(Frame {
            bytes: frame.to_vec(),
            receipt,
        }));
        slot.fresh = true;
        wake.notify_one();
        true
    }

    /// A new generation on every connection, including outages between ticks.
    pub fn connection(&self) -> Option<u64> {
        let slot = self.shared.0.lock().unwrap();
        slot.connected.then_some(slot.generation)
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
    let mut slot = shared.0.lock().unwrap();
    slot.connected = connected;
    slot.frame = None;
    slot.fresh = false;
    if connected {
        slot.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_redraw_keeps_the_same_delivery_receipt() {
        let sender = Sender {
            shared: Shared::default(),
        };
        set_connected(&sender.shared, true);
        let connection = sender.connection().unwrap();
        let receipt = Receipt::default();
        assert!(sender.publish(connection, b"first", Some(receipt.clone())));
        assert!(sender.publish(connection, b"redrawn", Some(receipt.clone())));
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

        set_connected(&sender.shared, false);
        set_connected(&sender.shared, true);
        assert!(sender.publish(
            sender.connection().unwrap(),
            b"still active",
            Some(receipt.clone())
        ));
        next_frame(&sender.shared)
            .unwrap()
            .receipt
            .as_ref()
            .unwrap()
            .mark_sent();
        assert_eq!(
            receipt.sent_at(),
            Some(first),
            "reconnect isn't a new lifetime"
        );
    }

    #[test]
    fn reconnect_discards_queued_content_and_rejects_in_flight_redraws() {
        let sender = Sender {
            shared: Shared::default(),
        };
        let dismissed = Receipt::default();
        assert!(sender.connection().is_none());
        assert!(!sender.publish(0, b"offline", Some(dismissed.clone())));

        set_connected(&sender.shared, true);
        let old_connection = sender.connection().unwrap();
        assert!(sender.publish(old_connection, b"dismissed", Some(dismissed.clone())));
        set_connected(&sender.shared, false);
        assert!(sender.connection().is_none());
        {
            let slot = sender.shared.0.lock().unwrap();
            assert!(slot.frame.is_none());
            assert!(!slot.fresh);
        }
        assert!(!sender.publish(old_connection, b"offline", Some(dismissed.clone())));

        // Reconnect before the frame loop observes the outage.
        set_connected(&sender.shared, true);
        let connection = sender.connection().unwrap();
        assert_ne!(connection, old_connection);
        assert!(next_frame(&sender.shared).is_none(), "nothing to replay");
        assert!(!sender.publish(old_connection, b"old render", Some(dismissed.clone())));
        assert!(sender.publish(connection, b"current", None));
        assert!(!sender.publish(old_connection, b"late render", Some(dismissed.clone())));
        assert_eq!(next_frame(&sender.shared).unwrap().bytes, b"current");
        assert_eq!(
            next_frame(&sender.shared).unwrap().bytes,
            b"current",
            "keepalive"
        );
        assert!(dismissed.sent_at().is_none());
    }
}
