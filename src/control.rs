//! Commands for the running service over a Unix socket, so scripts, hooks
//! and keybindings can steer the panel:
//!
//!   cooler-lcd next          show the next screen
//!   cooler-lcd show NAME     jump to a configured screen
//!   cooler-lcd flash TEXT    show a message for one rotation
//!
//! One command per connection, one line each way: the command, then `ok`
//! or `error: ...`.

use std::fs::{File, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::log::Log;

/// Verbs accepted on the command line and the socket.
pub const VERBS: &[&str] = &["next", "show", "flash"];
/// Long enough for the frame loop to get to it (at most a tick).
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// The whole command has to arrive within this, however it trickles in.
const READ_DEADLINE: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest command accepted, in bytes.
const MAX_LINE: usize = 1024;
/// Connections served at once; more are turned away.
const MAX_CLIENTS: usize = 8;
/// Pause after a failed accept (out of file descriptors, say), so the
/// listener doesn't spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(250);

pub enum Command {
    Next,
    Show(String),
    Flash(String),
}

/// A command from the socket, with where to send its outcome. Once
/// `expires` has passed the client has been told there was no answer, so
/// the command is dropped instead of carried out late.
pub struct Request {
    pub command: Command,
    pub reply: mpsc::Sender<Result<(), String>>,
    pub expires: Instant,
}

/// The per-user runtime directory, which only this user can get into.
/// There's no safe shared fallback, so without it there's no socket.
fn runtime_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR isn't set")
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("cooler-lcd.sock"))
}

/// Starts listening; requests arrive on the returned channel.
pub fn listen() -> Result<mpsc::Receiver<Request>> {
    let dir = runtime_dir()?;
    let path = dir.join("cooler-lcd.sock");
    // Held for the life of the process, so two instances starting at once
    // can't both decide the socket is stale and remove each other's.
    let lock = File::create(dir.join("cooler-lcd.lock")).context("creating the lock file")?;
    if lock.try_lock().is_err() {
        bail!("another cooler-lcd is already running");
    }
    // Left over from a previous run that didn't get to clean up.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _lock = lock;
        let active = Arc::new(AtomicUsize::new(0));
        let mut log = Log::default();
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    log.error(format!("control socket: accepting: {e}"));
                    thread::sleep(ACCEPT_BACKOFF);
                    continue;
                }
            };
            if active.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
                active.fetch_sub(1, Ordering::SeqCst);
                refuse(&stream, "busy, try again");
                continue;
            }
            let (tx, slot) = (tx.clone(), Slot(active.clone()));
            let spawned = thread::Builder::new().spawn(move || {
                let _slot = slot;
                serve(stream, &tx);
            });
            if let Err(e) = spawned {
                log.error(format!("control socket: starting a thread: {e}"));
            }
        }
    });
    Ok(rx)
}

/// One of the `MAX_CLIENTS` connection slots, given back when dropped.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn refuse(stream: &UnixStream, why: &str) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = (&*stream).write_all(format!("error: {why}\n").as_bytes());
}

fn serve(stream: UnixStream, requests: &mpsc::Sender<Request>) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let outcome = read_command(&stream, Instant::now() + READ_DEADLINE)
        .and_then(|line| parse(&line))
        .and_then(|command| {
            let (reply, outcome) = mpsc::channel();
            let expires = Instant::now() + REPLY_TIMEOUT;
            requests
                .send(Request {
                    command,
                    reply,
                    expires,
                })
                .map_err(|_| "shutting down".to_string())?;
            outcome
                .recv_timeout(REPLY_TIMEOUT)
                .unwrap_or_else(|_| Err("no answer from the frame loop; not carried out".into()))
        });
    let reply = match outcome {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!("error: {e}\n"),
    };
    let _ = (&stream).write_all(reply.as_bytes());
}

/// Reads one command line, which has to arrive whole by `deadline` and be
/// at most `MAX_LINE` bytes. A client that closes its end without a
/// newline has still sent a whole command.
fn read_command(mut stream: &UnixStream, deadline: Instant) -> Result<String, String> {
    let mut line = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out reading the command".into());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err("timed out reading the command".into());
            }
            Err(e) => return Err(e.to_string()),
        };
        let chunk = &buf[..n];
        let end = chunk.iter().position(|&b| b == b'\n');
        line.extend_from_slice(&chunk[..end.unwrap_or(n)]);
        if line.len() > MAX_LINE {
            return Err(format!("command too long (over {MAX_LINE} bytes)"));
        }
        if end.is_some() || n == 0 {
            return String::from_utf8(line).map_err(|_| "command isn't UTF-8".into());
        }
    }
}

fn parse(line: &str) -> Result<Command, String> {
    let line = line.trim();
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    match (verb, rest) {
        ("next", "") => Ok(Command::Next),
        ("show", name) if !name.is_empty() => Ok(Command::Show(name.into())),
        ("flash", text) if !text.is_empty() => Ok(Command::Flash(text.into())),
        _ => Err(format!(
            "expected one of: next, show NAME, flash TEXT (got {line:?})"
        )),
    }
}

/// Sends one command line to the running service and returns its answer.
pub fn send(line: &str) -> Result<()> {
    // Newlines would end the command early; a message is one line.
    let line = line.replace(['\n', '\r'], " ");
    if line.len() > MAX_LINE {
        bail!("command too long (over {MAX_LINE} bytes)");
    }
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow!(
            "cooler-lcd isn't running (can't connect to {}: {e})",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT + READ_DEADLINE))?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    match reply.trim() {
        "ok" => Ok(()),
        other => bail!("{}", other.strip_prefix("error: ").unwrap_or(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert!(matches!(parse("next\n"), Ok(Command::Next)));
        assert!(matches!(parse("show radar"), Ok(Command::Show(n)) if n == "radar"));
        assert!(
            matches!(parse("flash Claude needs you  \n"), Ok(Command::Flash(t)) if t == "Claude needs you")
        );
        assert!(parse("flash").is_err());
        assert!(parse("next please").is_err());
        assert!(parse("reboot").is_err());
    }

    #[test]
    fn reads_whole_commands_only() {
        let soon = || Instant::now() + Duration::from_secs(1);

        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"flash hi\nextra").unwrap();
        assert_eq!(read_command(&server, soon()).unwrap(), "flash hi");

        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"next").unwrap();
        drop(client);
        assert_eq!(read_command(&server, soon()).unwrap(), "next");

        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(&vec![b'a'; MAX_LINE + 1]).unwrap();
        assert!(
            read_command(&server, soon())
                .unwrap_err()
                .contains("too long")
        );

        // A client that never finishes its line is cut off at the deadline.
        let (mut client, server) = UnixStream::pair().unwrap();
        client.write_all(b"fla").unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        assert!(
            read_command(&server, deadline)
                .unwrap_err()
                .contains("timed out")
        );
    }
}
