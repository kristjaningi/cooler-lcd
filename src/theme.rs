//! Colors and font from the active Omarchy theme.

use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use ab_glyph::FontVec;
use anyhow::{Context, Result, bail};

/// Longest a font lookup may take. Theme switches are loaded in the frame
/// loop, so one that hung would freeze the panel; it's killed instead.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub struct Theme {
    /// Cache identity, including font reloads even when an allocator reuses
    /// the old font's address while a screen is hidden.
    pub revision: u64,
    pub background: Rgb,
    pub surface: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub red: Rgb,
    pub cyan: Rgb,
    pub font: FontVec,
}

pub fn colors_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/state/omarchy/current/theme/colors.toml")
}

/// When the theme's colors and the user's font config last changed: a
/// new value means the theme needs loading again. None while the colors
/// are missing, which they briefly are while Omarchy swaps a theme in.
/// Omarchy's font setter rewrites only the font config, and it may not
/// exist at all.
pub fn stamp() -> Option<(SystemTime, Option<SystemTime>)> {
    let mtime = |p: PathBuf| fs::metadata(p).and_then(|m| m.modified()).ok();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    Some((
        mtime(colors_path())?,
        mtime(home.join(".config/fontconfig/fonts.conf")),
    ))
}

impl Theme {
    /// Loads the current theme. At startup (`strict == false`) a missing or
    /// broken colors.toml falls back to Tokyo Night; on reload it is an error,
    /// so the caller keeps the previous theme and tries again later.
    pub fn load(strict: bool) -> Result<Self> {
        let table = match read_colors() {
            Ok(t) => t,
            Err(e) if strict => return Err(e),
            Err(_) => toml::Table::new(),
        };
        Ok(Self::from_table(&table, load_font()?))
    }

    /// The fallback colors with whatever font there is, for tests.
    #[cfg(test)]
    pub fn fallback() -> Self {
        Self::from_table(&toml::Table::new(), load_font().unwrap())
    }

    fn from_table(table: &toml::Table, font: FontVec) -> Self {
        // Tokyo Night as the fallback, matching Omarchy's default.
        let color = |key: &str, fallback: Rgb| {
            table
                .get(key)
                .and_then(|v| v.as_str())
                .and_then(parse_hex)
                .unwrap_or(fallback)
        };
        static REVISION: AtomicU64 = AtomicU64::new(1);
        Self {
            revision: REVISION.fetch_add(1, Ordering::Relaxed),
            background: color("background", Rgb(0x1a, 0x1b, 0x26)),
            surface: color("lighter_background", Rgb(0x24, 0x28, 0x3b)),
            foreground: color("bright_foreground", Rgb(0xc0, 0xca, 0xf5)),
            muted: color("dark_foreground", Rgb(0x56, 0x5f, 0x89)),
            accent: color("accent", Rgb(0x7a, 0xa2, 0xf7)),
            green: color("green", Rgb(0x9e, 0xce, 0x6a)),
            yellow: color("yellow", Rgb(0xe0, 0xaf, 0x68)),
            red: color("red", Rgb(0xf7, 0x76, 0x8e)),
            cyan: color("cyan", Rgb(0x7d, 0xcf, 0xff)),
            font,
        }
    }
}

fn read_colors() -> Result<toml::Table> {
    let path = colors_path();
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Parses `#rrggbb` (the `#` is optional).
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some(Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// The bold variant of Omarchy's current font, falling back to the system
/// sans-serif. Each candidate is parsed so an unusable file is skipped.
fn load_font() -> Result<FontVec> {
    let current = run(&mut Command::new("omarchy-font-current"));
    for family in current
        .iter()
        .map(String::as_str)
        .chain(["sans", "monospace"])
    {
        let Some(path) = fc_match(&format!("{family}:bold")) else {
            continue;
        };
        if let Some(font) = fs::read(&path)
            .ok()
            .and_then(|b| FontVec::try_from_vec(b).ok())
        {
            return Ok(font);
        }
    }
    bail!("no usable font found via fontconfig")
}

fn fc_match(pattern: &str) -> Option<String> {
    run(Command::new("fc-match").args(["-f", "%{file}", pattern]))
}

/// Runs `command` and returns its trimmed output, if it has any and all of
/// it arrives within `LOOKUP_TIMEOUT`. The command runs in a process group
/// of its own, which is killed afterwards, so nothing it started outlives
/// it, or holds its output open past the deadline.
fn run(command: &mut Command) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // The output is whole once every process holding the pipe has let go
    // of it: the command, and anything it started.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut out = String::new();
        let _ = tx.send(stdout.read_to_string(&mut out).ok().map(|_| out));
    });
    let out = rx.recv_timeout(LOOKUP_TIMEOUT).ok().flatten();
    // The group bears the command's id, which can't be reused until the
    // command is reaped by the wait below.
    // SAFETY: kill(2) only sends a signal; a negative id names a group.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
    let out = out?;
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    #[test]
    fn runs_lookups_with_a_deadline() {
        assert_eq!(run(Command::new("echo").arg(" hi ")).as_deref(), Some("hi"));
        assert_eq!(run(&mut Command::new("true")), None, "no output");
        let started = Instant::now();
        assert_eq!(run(Command::new("sleep").arg("10")), None);
        assert!(started.elapsed() < LOOKUP_TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn a_lingering_child_neither_holds_up_the_lookup_nor_outlives_it() {
        let pid_file = std::env::temp_dir().join(format!("cooler-lcd-run-{}", std::process::id()));
        // Exits at once, but leaves a child holding its output open.
        let script = format!("sleep 30 & echo $! > {}; printf hi", pid_file.display());
        let started = Instant::now();
        run(Command::new("sh").args(["-c", &script]));
        assert!(started.elapsed() < LOOKUP_TIMEOUT + Duration::from_secs(1));

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let _ = std::fs::remove_file(&pid_file);
        let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
        // Killed; reparented and reaped shortly after.
        let gone = (0..100).any(|_| {
            let state = fs::read_to_string(proc.join("stat")).unwrap_or_default();
            let gone = state.is_empty() || state.contains(") Z ");
            if !gone {
                thread::sleep(Duration::from_millis(10));
            }
            gone
        });
        assert!(gone, "the child outlived the lookup");
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex("#7aa2f7"), Some(Rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(parse_hex("1A1B26"), Some(Rgb(0x1a, 0x1b, 0x26)));
    }

    #[test]
    fn rejects_bad_hex_without_panicking() {
        for bad in [
            "", "#123", "#12345", "#1234567", "#zzzzzz", "#aé1234", "#ééé",
        ] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
    }
}
