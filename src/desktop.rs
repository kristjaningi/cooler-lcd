//! Whether anyone is at the desktop: Hyprland's session is locked, or every
//! monitor has been turned off (DPMS). Asked over Hyprland's IPC socket
//! from a background thread every few seconds.
//!
//! Hyprland reports no lock state directly, but an active session lock is
//! one of the reasons a monitor can't go solitary: `LOCK` in
//! `solitaryBlockedBy` (the same check as `omarchy-hyprland-session-locked`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

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
                // Unreachable Hyprland (not running, another compositor)
                // counts as someone being there, so the screens keep going.
                let now_away = monitors().is_ok_and(|m| is_away(&m));
                a.store(now_away, Ordering::Relaxed);
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

fn is_away(monitors: &[Monitor]) -> bool {
    let locked = monitors
        .iter()
        .any(|m| m.solitary_blocked_by.iter().any(|r| r == "LOCK"));
    let mut on = monitors.iter().filter(|m| !m.disabled);
    let dark = on.clone().next().is_some() && on.all(|m| !m.dpms_status);
    locked || dark
}

fn monitors() -> Result<Vec<Monitor>> {
    let body = ask(b"j/monitors")?;
    serde_json::from_str(&body).context("parsing Hyprland monitors")
}

/// Sends one request to Hyprland's command socket and returns the reply.
fn ask(request: &[u8]) -> Result<String> {
    let path = socket().context("no Hyprland socket")?;
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("connecting to {}", path.display()))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(request)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// The running instance's socket: the one `HYPRLAND_INSTANCE_SIGNATURE`
/// names, else the newest, since Hyprland may have restarted since the
/// service started.
fn socket() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("hypr");
    let named = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
        .map(|sig| dir.join(sig).join(".socket.sock"))
        .filter(|p| p.exists());
    named.or_else(|| {
        std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path().join(".socket.sock"))
            .filter(|p| p.exists())
            .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<Monitor> {
        serde_json::from_str(json).unwrap()
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
