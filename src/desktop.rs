//! Whether anyone is at the desktop: Hyprland's session is locked, or every
//! monitor has been turned off (DPMS). Asked over Hyprland's IPC socket
//! from a background thread every few seconds.
//!
//! Hyprland reports no lock state directly, but an active session lock is
//! one of the reasons a monitor can't go solitary: `LOCK` in
//! `solitaryBlockedBy` (the same check as `omarchy-hyprland-session-locked`).

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::Deserialize;

const POLL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(1);

pub struct Desktop {
    away: Arc<AtomicBool>,
}

impl Desktop {
    pub fn spawn() -> Self {
        let away = Arc::new(AtomicBool::new(false));
        let a = away.clone();
        thread::spawn(move || {
            loop {
                let was = a.load(Ordering::Relaxed);
                a.store(next(was, poll()), Ordering::Relaxed);
                thread::sleep(POLL);
            }
        });
        Self { away }
    }

    /// True while the session is locked or all monitors are off.
    pub fn away(&self) -> bool {
        self.away.load(Ordering::Relaxed)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Monitor {
    #[serde(default = "yes")]
    dpms_status: bool,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    solitary_blocked_by: Vec<String>,
}

fn yes() -> bool {
    true
}

/// What one poll of Hyprland found out.
#[derive(Debug, PartialEq)]
enum Poll {
    /// Hyprland answered: whether the desktop is away.
    Answer(bool),
    /// Hyprland isn't running: no socket, or none taking connections
    /// (another compositor, or Hyprland exited and left its socket behind).
    Gone,
    /// Hyprland is there, but asking it failed: a reply too slow, cut off
    /// or garbled. Says nothing about the desktop.
    Failed,
}

/// Whether the desktop is away now, given whether it was and what this
/// poll found. A failed poll keeps the last answer however long that goes
/// on, so a lock holds until Hyprland says otherwise; only Hyprland being
/// gone counts as someone being there, so the screens keep going.
fn next(was: bool, poll: Poll) -> bool {
    match poll {
        Poll::Answer(away) => away,
        Poll::Gone => false,
        Poll::Failed => was,
    }
}

fn is_away(monitors: &[Monitor]) -> bool {
    let locked = monitors
        .iter()
        .any(|m| m.solitary_blocked_by.iter().any(|r| r == "LOCK"));
    let mut on = monitors.iter().filter(|m| !m.disabled);
    let dark = on.clone().next().is_some() && on.all(|m| !m.dpms_status);
    locked || dark
}

fn poll() -> Poll {
    poll_at(&sockets())
}

/// Asks each of Hyprland's command sockets in turn about the monitors,
/// until one answers.
fn poll_at(sockets: &[PathBuf]) -> Poll {
    let mut listening = false;
    for path in sockets {
        let stream = match UnixStream::connect(path) {
            Ok(stream) => stream,
            // Nothing there to ask.
            Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
                continue;
            }
            Err(_) => {
                listening = true;
                continue;
            }
        };
        listening = true;
        if let Ok(monitors) = monitors(stream, path) {
            return Poll::Answer(is_away(&monitors));
        }
    }
    if listening { Poll::Failed } else { Poll::Gone }
}

fn monitors(stream: UnixStream, path: &Path) -> Result<Vec<Monitor>> {
    let body = ask(stream, b"j/monitors").with_context(|| format!("asking {}", path.display()))?;
    serde_json::from_str(&body).context("parsing Hyprland monitors")
}

/// Sends one request on a connected command socket and returns the reply.
fn ask(mut stream: UnixStream, request: &[u8]) -> Result<String> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(request)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// Hyprland command sockets to try, best first: the instance
/// `HYPRLAND_INSTANCE_SIGNATURE` names, then the rest newest first. Hyprland
/// may have restarted since the service started, and after a crash the old
/// instance's socket file stays behind with nothing listening, so a socket
/// that refuses the connection falls through to the next.
fn sockets() -> Vec<PathBuf> {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Vec::new();
    };
    let dir = PathBuf::from(runtime).join("hypr");
    let named = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
        .map(|sig| dir.join(sig).join(".socket.sock"));
    let mut others: Vec<(Option<SystemTime>, PathBuf)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(".socket.sock"))
        .filter(|p| p.exists() && Some(p) != named.as_ref())
        .map(|p| (p.metadata().and_then(|m| m.modified()).ok(), p))
        .collect();
    others.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    named
        .filter(|p| p.exists())
        .into_iter()
        .chain(others.into_iter().map(|(_, p)| p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<Monitor> {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_lock_holds_until_hyprland_answers_or_is_gone() {
        let mut away = next(false, Poll::Answer(true));
        for _ in 0..100 {
            away = next(away, Poll::Failed);
        }
        assert!(away, "still locked after any number of failed polls");
        assert!(!next(away, Poll::Answer(false)));
        assert!(!next(away, Poll::Gone), "Hyprland exited");
        assert!(!next(false, Poll::Failed), "never locked, never away");
    }

    #[test]
    fn tells_a_gone_hyprland_from_a_failing_one() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("cooler-lcd-hypr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = |name: &str| dir.join(name);

        // Bound and dropped: the file stays behind, as after a crash.
        drop(UnixListener::bind(socket("dead")).unwrap());
        assert_eq!(poll_at(&[socket("dead"), socket("missing")]), Poll::Gone);
        assert_eq!(poll_at(&[]), Poll::Gone);

        // Takes the connection but never replies.
        let _stuck = UnixListener::bind(socket("stuck")).unwrap();
        assert_eq!(poll_at(&[socket("dead"), socket("stuck")]), Poll::Failed);

        // Answers, after the others have failed.
        let live = UnixListener::bind(socket("live")).unwrap();
        thread::spawn(move || {
            let (mut stream, _) = live.accept().unwrap();
            let mut request = [0; 10];
            stream.read_exact(&mut request).unwrap();
            stream
                .write_all(br#"[{"solitaryBlockedBy":["LOCK"]}]"#)
                .unwrap();
        });
        assert_eq!(
            poll_at(&[socket("dead"), socket("stuck"), socket("live")]),
            Poll::Answer(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spots_lock_and_dark_monitors() {
        let active = parse(r#"[{"dpmsStatus":true,"solitaryBlockedBy":["WINDOWED"]}]"#);
        assert!(!is_away(&active));
        let locked = parse(r#"[{"dpmsStatus":true,"solitaryBlockedBy":["LOCK"]}]"#);
        assert!(is_away(&locked));
        let dark = parse(r#"[{"dpmsStatus":false},{"dpmsStatus":false}]"#);
        assert!(is_away(&dark));
        let one_on = parse(r#"[{"dpmsStatus":false},{"dpmsStatus":true}]"#);
        assert!(!is_away(&one_on));
        let disabled_off = parse(r#"[{"dpmsStatus":true},{"dpmsStatus":false,"disabled":true}]"#);
        assert!(!is_away(&disabled_off));
        assert!(!is_away(&[]));
    }
}
