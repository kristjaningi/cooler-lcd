//! Commands for the running service over a Unix socket, so scripts, hooks
//! and keybindings can steer the panel:
//!
//!   cooler-lcd next          show the next screen
//!   cooler-lcd show NAME     jump to a configured screen
//!   cooler-lcd on NAME       put a screen in the rotation and show it
//!   cooler-lcd off NAME      take a screen out of the rotation
//!   cooler-lcd toggle NAME   one or the other
//!   cooler-lcd flash TEXT    show a message for one rotation
//!   cooler-lcd image FILE    show a scaled/cropped local image
//!   cooler-lcd snapshot FILE save what the panel shows as a JPEG
//!   cooler-lcd timer 25m     count down on the panel; `timer stop` cancels
//!
//! One command per connection, one line each way: a legacy command or JSON
//! content submission, then `ok`
//! (with a message for the user after a space, if there is one) or
//! `error: ...`.

use std::fs::{File, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::content::{Prepared, Submission, Visual};
use crate::log::Log;

/// Verbs accepted on the command line and the socket.
pub const VERBS: &[&str] = &[
    "next", "show", "on", "off", "toggle", "flash", "image", "snapshot", "timer",
];
/// Long enough for the frame loop to get to it (at most a tick).
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);
/// The whole command has to arrive within this, however it trickles in.
const READ_DEADLINE: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Extra time an image gets to be read, decoded and resized.
const IMAGE_BUDGET: Duration = Duration::from_secs(30);
/// How often a --wait connection checks that its client is still there.
const HANGUP_CHECK: Duration = Duration::from_secs(1);
/// Longest command accepted, in bytes.
const MAX_LINE: usize = 16 * 1024;
/// The longest timer accepted.
const MAX_TIMER: Duration = Duration::from_secs(24 * 60 * 60);
/// Connections served at once; more are turned away.
const MAX_CLIENTS: usize = 8;
/// Pause after a failed accept (out of file descriptors, say), so the
/// listener doesn't spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(250);

pub enum Command {
    Next,
    Show(String),
    On(String),
    Off(String),
    Toggle(String),
    /// A fully prepared image or styled message; no I/O on the frame loop.
    Content(Prepared),
    /// Where to save the frame on the panel; an absolute path, since the
    /// service doesn't share the client's working directory.
    Snapshot(PathBuf),
    /// Counts down this long on the panel.
    Timer(Duration),
    /// Cancels the timer, or dismisses it once it's up.
    TimerStop,
}

/// A command from the socket, with where to send its outcome. Once
/// `expires` has passed the client has been told there was no answer, so
/// the command is dropped instead of carried out late. --wait submissions
/// have no deadline and are answered after delivery, replacement or dismissal.
pub struct Request {
    pub command: Command,
    /// Ok with a message for the user (maybe empty), or why it failed.
    pub reply: mpsc::Sender<Result<String, String>>,
    pub expires: Option<Instant>,
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

/// Another instance holds the lock, so this one has to go: it would have
/// no socket, yet still claim the panel whenever the other let go of it.
#[derive(Debug)]
pub struct AlreadyRunning;

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("another cooler-lcd is already running")
    }
}

impl std::error::Error for AlreadyRunning {}

/// Claims the one-instance lock, held for the life of the process, so two
/// instances can't both drive the panel, nor both decide the socket is
/// stale and remove each other's. Fails with `AlreadyRunning` if another
/// instance holds it.
pub fn lock() -> Result<File> {
    lock_in(&runtime_dir()?)
}

fn lock_in(dir: &Path) -> Result<File> {
    let lock = File::create(dir.join("cooler-lcd.lock")).context("creating the lock file")?;
    if lock.try_lock().is_err() {
        return Err(AlreadyRunning.into());
    }
    Ok(lock)
}

/// Starts listening, holding on to `lock`; requests arrive on the returned
/// channel.
pub fn listen(lock: File) -> Result<mpsc::Receiver<Request>> {
    let path = socket_path()?;
    // Left over from a previous run that didn't get to clean up.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, Permissions::from_mode(0o600))?;
    // Requests can own a full image; bound retained frames if the frame
    // loop stalls instead of accumulating expired submissions.
    let (tx, rx) = mpsc::sync_channel(MAX_CLIENTS);
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

/// How long a client waits for its reply, from sending its command. The
/// service gives up a little sooner, so it never carries out a command
/// the client has already reported as failed.
fn reply_budget(image: bool) -> Duration {
    READ_DEADLINE + REPLY_TIMEOUT + if image { IMAGE_BUDGET } else { Duration::ZERO }
}

fn serve(stream: UnixStream, requests: &mpsc::SyncSender<Request>) {
    let accepted = Instant::now();
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let outcome = read_command(&stream, accepted + READ_DEADLINE)
        .and_then(|line| parse(&line))
        .and_then(|command| {
            let (wait, image) = match &command {
                Command::Content(c) => (c.wait, matches!(c.visual, Visual::Image(_))),
                _ => (false, false),
            };
            // Accepted before the client started its clock.
            let deadline = accepted + reply_budget(image) - Duration::from_millis(500);
            if !wait && Instant::now() >= deadline {
                return Err("preparing the content took too long; not carried out".into());
            }
            let (reply, outcome) = mpsc::channel();
            let expires = (!wait).then_some(deadline);
            requests
                .try_send(Request {
                    command,
                    reply,
                    expires,
                })
                .map_err(|e| match e {
                    mpsc::TrySendError::Full(_) => "frame loop busy, try again".to_string(),
                    mpsc::TrySendError::Disconnected(_) => "shutting down".to_string(),
                })?;
            if wait {
                wait_for(&stream, &outcome)
            } else {
                let left = deadline.saturating_duration_since(Instant::now());
                outcome.recv_timeout(left).unwrap_or_else(|_| {
                    Err("no answer from the frame loop; not carried out".into())
                })
            }
        });
    let reply = match outcome {
        Ok(message) if message.is_empty() => "ok\n".to_string(),
        Ok(message) => format!("ok {message}\n"),
        Err(e) => format!("error: {}\n", e.replace(['\n', '\r'], " ")),
    };
    let _ = (&stream).write_all(reply.as_bytes());
}

/// Waits for a --wait submission to be shown, replaced or dismissed, or
/// for its client to give up, so a Ctrl-C'd client doesn't hold on to a
/// connection slot until then.
fn wait_for(
    stream: &UnixStream,
    outcome: &mpsc::Receiver<Result<String, String>>,
) -> Result<String, String> {
    loop {
        match outcome.recv_timeout(HANGUP_CHECK) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("service stopped before the content was shown".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) if hung_up(stream) => {
                return Err("client went away".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// A client sends nothing after its command, so a readable end of file
/// (or an error) means it has closed the connection.
fn hung_up(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut byte = 0u8;
    // SAFETY: a one-byte peek into a local buffer on a socket we own.
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    match n {
        0 => true,
        n if n > 0 => false,
        _ => !matches!(
            std::io::Error::last_os_error().kind(),
            ErrorKind::WouldBlock | ErrorKind::Interrupted
        ),
    }
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
    if line.starts_with('{') {
        return serde_json::from_str::<Submission>(line)
            .map_err(|e| format!("invalid content request: {e}"))?
            .prepare()
            .map(Command::Content)
            .map_err(|e| format!("{e:#}"));
    }
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let rest = rest.trim();
    match (verb, rest) {
        ("next", "") => Ok(Command::Next),
        ("show", name) if !name.is_empty() => Ok(Command::Show(name.into())),
        ("on", name) if !name.is_empty() => Ok(Command::On(name.into())),
        ("off", name) if !name.is_empty() => Ok(Command::Off(name.into())),
        ("toggle", name) if !name.is_empty() => Ok(Command::Toggle(name.into())),
        ("flash", text) if !text.is_empty() && text.len() <= 1024 => {
            Ok(Command::Content(Prepared::message(text.into())))
        }
        ("snapshot", path) if Path::new(path).is_absolute() => Ok(Command::Snapshot(path.into())),
        ("timer", "stop") => Ok(Command::TimerStop),
        ("timer", length) if !length.is_empty() => parse_length(length).map(Command::Timer),
        _ => Err(format!(
            "expected one of: next, show NAME, on NAME, off NAME, toggle NAME, \
             flash TEXT, snapshot /ABSOLUTE/PATH, timer LENGTH, timer stop (got {line:?})"
        )),
    }
}

/// A timer's length, like `25m`, `1h30m`, `90s` or a bare `25` for
/// minutes, from a second up to `MAX_TIMER`.
pub(crate) fn parse_length(s: &str) -> Result<Duration, String> {
    let bad = || format!("expected a length like 25m, 1h30m or 90s (got {s:?})");
    let s: String = s
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let s = if s.bytes().all(|b| b.is_ascii_digit()) {
        format!("{s}m")
    } else {
        s
    };
    let mut secs: u64 = 0;
    let mut number = String::new();
    let mut units = "hms".chars();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        // Each unit at most once, largest first: 1h30m, not 30m1h.
        let scale = match units.find(|&u| u == ch) {
            Some('h') => 3600,
            Some('m') => 60,
            Some('s') => 1,
            _ => return Err(bad()),
        };
        let n: u64 = std::mem::take(&mut number).parse().map_err(|_| bad())?;
        secs = n
            .checked_mul(scale)
            .and_then(|n| secs.checked_add(n))
            .ok_or_else(bad)?;
    }
    if !number.is_empty() || secs == 0 {
        return Err(bad());
    }
    let length = Duration::from_secs(secs);
    if length > MAX_TIMER {
        return Err(format!("{s} is longer than a day"));
    }
    Ok(length)
}

/// Sends argv to the running service, preserving quoted content via JSON.
pub fn send(args: &[String]) -> Result<String> {
    let verb = args.first().context("missing command")?;
    let (line, wait, image) = if matches!(verb.as_str(), "flash" | "image") {
        let submission = Submission::from_args(args)?;
        // A plain message goes as the old line, which a service started
        // before an upgrade still understands.
        let line = match submission.plain_message() {
            Some(message) => format!("flash {message}"),
            None => serde_json::to_string(&submission)?,
        };
        (line, submission.options.wait, verb == "image")
    } else if verb == "snapshot" && args.len() > 1 {
        let path = std::path::absolute(args[1..].join(" "))?;
        if path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|b| matches!(b, b'\n' | b'\r'))
        {
            bail!("snapshot paths can't contain line breaks");
        }
        (format!("snapshot {}", path.display()), false, false)
    } else {
        (args.join(" ").replace(['\n', '\r'], " "), false, false)
    };
    ensure_line(&line)?;
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow!(
            "cooler-lcd isn't running (can't connect to {}: {e})",
            path.display()
        )
    })?;
    // Image preparation is done before enqueueing. Explicit --wait has no
    // display deadline: an unplugged panel can come back much later.
    stream.set_read_timeout((!wait).then_some(reply_budget(image)))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut reply = String::new();
    let read = BufReader::new(&stream)
        .read_line(&mut reply)
        .map_err(|e| match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => {
                anyhow!("no answer from cooler-lcd in time; not carried out")
            }
            _ => e.into(),
        })?;
    ensure_reply(read, &reply)
}

fn ensure_line(line: &str) -> Result<()> {
    if line.len() > MAX_LINE {
        bail!("command too long (over {MAX_LINE} bytes)");
    }
    Ok(())
}

fn ensure_reply(bytes: usize, reply: &str) -> Result<String> {
    if bytes == 0 {
        bail!("service disconnected before replying");
    }
    let reply = reply.trim();
    match reply.strip_prefix("ok") {
        Some(message) if message.is_empty() || message.starts_with(' ') => {
            Ok(message.trim().to_string())
        }
        _ => bail!("{}", reply.strip_prefix("error: ").unwrap_or(reply)),
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
            matches!(parse("flash Claude needs you  \n"), Ok(Command::Content(Prepared { visual: Visual::Flash { message, .. }, .. })) if message == "Claude needs you")
        );
        assert!(matches!(parse("toggle radar"), Ok(Command::Toggle(n)) if n == "radar"));
        assert!(matches!(parse("off radar"), Ok(Command::Off(n)) if n == "radar"));
        assert!(parse("on").is_err());
        assert!(
            matches!(parse("snapshot /tmp/a b.jpg"), Ok(Command::Snapshot(p)) if p == Path::new("/tmp/a b.jpg"))
        );
        assert!(parse("snapshot out.jpg").is_err(), "relative to what?");
        assert!(parse("flash").is_err());
        assert!(matches!(parse("timer stop"), Ok(Command::TimerStop)));
        assert!(matches!(parse("timer 25m"), Ok(Command::Timer(d)) if d.as_secs() == 1500));
        assert!(parse("timer").is_err());
        assert!(parse("next please").is_err());
        assert!(parse("reboot").is_err());
    }

    #[test]
    fn parses_timer_lengths() {
        let secs = |s| parse_length(s).map(|d| d.as_secs());
        assert_eq!(secs("25m"), Ok(1500));
        assert_eq!(secs("25"), Ok(1500), "minutes by default");
        assert_eq!(secs("90s"), Ok(90));
        assert_eq!(secs("1h30m"), Ok(5400));
        assert_eq!(secs("1h 30m"), Ok(5400));
        assert_eq!(secs("1H30M"), Ok(5400));
        assert_eq!(secs("24h"), Ok(86400));
        for bad in [
            "0",
            "0m",
            "m",
            "25x",
            "1h1",
            "30m1h",
            "5m5m",
            "25h",
            "-5m",
            "99999999999999999999h",
        ] {
            assert!(secs(bad).is_err(), "{bad:?}");
        }
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

    #[test]
    fn a_second_instance_is_told_to_go() {
        let dir = std::env::temp_dir().join(format!("cooler-lcd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let held = lock_in(&dir).unwrap();
        let second = lock_in(&dir).unwrap_err();
        assert!(second.is::<AlreadyRunning>());
        drop(held);
        assert!(lock_in(&dir).is_ok(), "free again once the first is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn carousel() -> crate::Carousel {
        crate::Carousel::with_screens(
            vec![(
                "dashboard".into(),
                crate::screens::message("underneath".into(), None, None),
            )],
            Duration::from_secs(15),
        )
    }

    #[test]
    fn wait_replies_after_delivery_and_reports_replacement_over_the_socket() {
        let submission =
            Submission::from_args(&["flash", "hello", "--wait", "--until-next"].map(String::from))
                .unwrap();
        let line = serde_json::to_string(&submission).unwrap();
        for deliver in [false, true] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let (tx, requests) = mpsc::sync_channel(MAX_CLIENTS);
            let worker = thread::spawn(move || serve(server, &tx));
            client.write_all(format!("{line}\n").as_bytes()).unwrap();
            let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(
                request.expires.is_none(),
                "offline waits must not expire in the queue"
            );
            let mut carousel = carousel();
            assert!(crate::handle_request(&mut carousel, request, false, &[]));
            let mut reply = String::new();
            let mut client = BufReader::new(client);
            let error = client.read_line(&mut reply).unwrap_err();
            assert!(matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut
            ));
            if deliver {
                carousel.flash.as_ref().unwrap().receipt.mark_sent();
                carousel.flash_drawn();
            } else {
                carousel
                    .command(Command::Content(Prepared::message("replacement".into())))
                    .unwrap();
            }
            client
                .get_ref()
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            client.read_line(&mut reply).unwrap();
            if deliver {
                assert_eq!(reply, "ok\n");
            } else {
                assert!(reply.contains("replaced or dismissed"), "{reply}");
            }
            worker.join().unwrap();
        }
    }

    #[test]
    fn a_waiting_connection_lets_go_once_its_client_does() {
        let (client, server) = UnixStream::pair().unwrap();
        let (_keep, outcome) = mpsc::channel();
        assert!(!hung_up(&server));
        drop(client);
        assert!(hung_up(&server));
        assert!(
            wait_for(&server, &outcome)
                .unwrap_err()
                .contains("went away")
        );
    }

    #[test]
    fn invalid_images_are_rejected_before_the_frame_loop() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (tx, requests) = mpsc::sync_channel(MAX_CLIENTS);
        let worker = thread::spawn(move || serve(server, &tx));
        let submission =
            Submission::from_args(&["image", "/nonexistent/cooler-lcd.png"].map(String::from))
                .unwrap();
        client
            .write_all(format!("{}\n", serde_json::to_string(&submission).unwrap()).as_bytes())
            .unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        assert!(reply.starts_with("error: opening /nonexistent/"), "{reply}");
        assert!(requests.try_recv().is_err());
        worker.join().unwrap();
    }

    #[test]
    fn queued_image_is_already_decoded_and_survives_source_removal() {
        let path =
            std::env::temp_dir().join(format!("cooler-lcd-push image-{}.png", std::process::id()));
        let mut original = tiny_skia::Pixmap::new(16, 16).unwrap();
        original.fill(tiny_skia::Color::from_rgba8(20, 180, 60, 255));
        original.save_png(&path).unwrap();
        let submission = Submission::from_args(&[
            "image".to_string(),
            path.to_string_lossy().into_owned(),
            "--for".into(),
            "30s".into(),
        ])
        .unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (tx, requests) = mpsc::sync_channel(MAX_CLIENTS);
        let worker = thread::spawn(move || serve(server, &tx));
        client
            .write_all(format!("{}\n", serde_json::to_string(&submission).unwrap()).as_bytes())
            .unwrap();
        let request = requests.recv_timeout(Duration::from_secs(3)).unwrap();
        std::fs::remove_file(path).unwrap();
        let mut carousel = carousel();
        assert!(crate::handle_request(&mut carousel, request, false, &[]));
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        assert_eq!(reply, "ok\n", "normal submission acknowledges acceptance");
        let theme = crate::theme::Theme::fallback();
        let screen = carousel.current(false);
        screen.update(chrono::Local::now());
        let mut px = tiny_skia::Pixmap::new(480, 480).unwrap();
        screen.draw(&mut px, &theme);
        assert_eq!(px.pixel(240, 240).unwrap().green(), 180);
        worker.join().unwrap();
    }

    #[test]
    fn a_full_queue_refuses_content_without_retaining_more_frames() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (tx, _requests) = mpsc::sync_channel(0);
        let worker = thread::spawn(move || serve(server, &tx));
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client.write_all(b"flash hello\n").unwrap();
        let mut reply = String::new();
        BufReader::new(client).read_line(&mut reply).unwrap();
        assert!(reply.contains("busy"));
        worker.join().unwrap();
    }
}
