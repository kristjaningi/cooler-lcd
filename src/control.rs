//! Commands for the running service over a Unix socket, so scripts, hooks
//! and keybindings can steer the panel:
//!
//!   cooler-lcd next          show the next screen
//!   cooler-lcd show NAME     jump to a configured screen
//!   cooler-lcd flash TEXT    show a message for one rotation
//!
//! One command per connection, one line each way: the command, then `ok`
//! or `error: ...`.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Verbs accepted on the command line and the socket.
pub const VERBS: &[&str] = &["next", "show", "flash"];
/// Long enough for the frame loop to get to it (at most a tick).
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest message accepted, in bytes.
const MAX_LINE: usize = 1024;

pub enum Command {
    Next,
    Show(String),
    Flash(String),
}

/// A command from the socket, with where to send its outcome.
pub struct Request {
    pub command: Command,
    pub reply: mpsc::Sender<Result<(), String>>,
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("cooler-lcd.sock")
}

/// Starts listening; requests arrive on the returned channel.
pub fn listen() -> Result<mpsc::Receiver<Request>> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!(
            "another cooler-lcd is already listening on {}",
            path.display()
        );
    }
    // Left over from a previous run that didn't get to clean up.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || serve(stream, &tx));
        }
    });
    Ok(rx)
}

fn serve(stream: UnixStream, requests: &mpsc::Sender<Request>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new(&stream)
        .take(MAX_LINE as u64)
        .read_line(&mut line);
    let outcome = match read.map_err(|e| e.to_string()).and_then(|_| parse(&line)) {
        Ok(command) => {
            let (reply, outcome) = mpsc::channel();
            let sent = requests.send(Request { command, reply });
            match sent {
                Ok(()) => outcome
                    .recv_timeout(REPLY_TIMEOUT)
                    .unwrap_or_else(|_| Err("no answer from the frame loop".into())),
                Err(_) => Err("shutting down".into()),
            }
        }
        Err(e) => Err(e),
    };
    let reply = match outcome {
        Ok(()) => "ok\n".to_string(),
        Err(e) => format!("error: {e}\n"),
    };
    let _ = (&stream).write_all(reply.as_bytes());
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
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("cooler-lcd isn't running ({} missing)", path.display()))?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT + IO_TIMEOUT))?;
    // Newlines would end the command early; a message is one line.
    let line = line.replace(['\n', '\r'], " ");
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
}
