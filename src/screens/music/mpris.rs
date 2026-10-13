//! What's playing, from the media players on the session bus (MPRIS:
//! Spotify, browsers, VLC, mpv with mpv-mpris), and its album art.
//!
//! Polled with `busctl` on a background thread: a list of the bus's names,
//! then one call per player for all of its state. That's a few ms of CPU
//! each time, so it polls every couple of seconds while the screen is up
//! and less often while it's hidden, when all that matters is whether
//! something is playing. Players don't announce their position, so the
//! screen runs it on from the last poll.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tiny_skia::Pixmap;

use super::art;

/// Between polls while the screen is up, so a new track shows quickly...
const POLL: Duration = Duration::from_secs(2);
/// ...and while it's hidden, only to know whether to put it in the rotation.
const IDLE_POLL: Duration = Duration::from_secs(5);
/// After `busctl` fails (no session bus, say).
const RETRY: Duration = Duration::from_secs(30);
/// Art that failed to load is tried again after this long, up to
/// `ART_TRIES` times in all for the same URL.
const ART_RETRY: Duration = Duration::from_secs(30);
const ART_TRIES: u32 = 3;
/// Titles and artists past this many characters are cut, so a page's
/// runaway title can't slow down laying out the text.
const MAX_CHARS: usize = 200;
/// The screen counts as up for this long after it was last drawn.
const WANTED: Duration = Duration::from_secs(5);
/// How long a paused player stays in the rotation, so a short pause
/// doesn't drop it.
const GRACE: Duration = Duration::from_secs(30);
const PREFIX: &str = "org.mpris.MediaPlayer2.";
/// playerctld mirrors whichever player is active; the player itself is
/// already on the list.
const PROXY: &str = "org.mpris.MediaPlayer2.playerctld";

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub art_url: Option<String>,
    pub length: Option<Duration>,
}

/// One player's state when it was polled.
#[derive(Clone, Debug)]
pub struct Playback {
    /// The player's bus name.
    pub player: String,
    pub track: Track,
    pub playing: bool,
    pub position: Option<Duration>,
    pub rate: f64,
    pub polled_at: Instant,
}

impl Playback {
    /// How far through the track it is, 0..=1, run on from the poll at
    /// `rate` while playing. None without a length or position (streams,
    /// some browsers).
    pub fn progress(&self) -> Option<f32> {
        let length = self.track.length.filter(|l| !l.is_zero())?;
        let mut at = self.position?.as_secs_f64();
        if self.playing {
            at += self.polled_at.elapsed().as_secs_f64() * self.rate;
        }
        Some((at / length.as_secs_f64()).clamp(0.0, 1.0) as f32)
    }
}

#[derive(Default)]
pub struct State {
    /// The player to show, if any has a track.
    pub playback: Option<Playback>,
    /// The art for `playback`'s track as it'll be drawn, and the URL it
    /// came from; none while it's missing or couldn't be loaded.
    pub art: Option<(String, Arc<Pixmap>)>,
    /// Whether the first poll, and the art it asked for, are done.
    pub ready: bool,
    /// Last time a player was seen playing.
    playing_at: Option<Instant>,
    wanted_at: Option<Instant>,
    worker: Option<Thread>,
}

impl State {
    /// Notes that the screen is up, waking the thread to poll at the
    /// faster pace right away if it wasn't.
    pub fn want(&mut self) {
        let idle = !self.wanted();
        self.wanted_at = Some(Instant::now());
        if idle && let Some(worker) = &self.worker {
            worker.unpark();
        }
    }

    fn wanted(&self) -> bool {
        self.wanted_at.is_some_and(|t| t.elapsed() < WANTED)
    }

    /// Whether something is playing, or was a moment ago and is still
    /// there, paused.
    pub fn present(&self) -> bool {
        self.playback.is_some() && self.playing_at.is_some_and(|t| t.elapsed() < GRACE)
    }
}

pub type Shared = Arc<Mutex<State>>;

pub fn spawn() -> Shared {
    let shared = Shared::default();
    let s = shared.clone();
    thread::spawn(move || run(&s));
    shared
}

/// The art loaded last: its URL, and the art, or when loading it last
/// failed and how many times it has.
type Loaded = (String, Result<Arc<Pixmap>, (Instant, u32)>);

fn run(shared: &Shared) {
    shared.lock().unwrap().worker = Some(thread::current());
    let mut failing = false;
    // Kept through a moment without a player, as between tracks.
    let mut art: Option<Loaded> = None;
    loop {
        let previous = shared
            .lock()
            .unwrap()
            .playback
            .as_ref()
            .map(|p| p.player.clone());
        let playback = match poll(previous.as_deref()) {
            Ok(playback) => {
                if std::mem::take(&mut failing) {
                    eprintln!("music: session bus reachable again");
                }
                playback
            }
            Err(e) => {
                if !std::mem::replace(&mut failing, true) {
                    eprintln!("music: {e:#}");
                }
                None
            }
        };
        let url = playback.as_ref().and_then(|p| p.track.art_url.clone());
        let load = url.as_ref().is_some_and(|url| match &art {
            Some((had, result)) if had == url => result
                .as_ref()
                .is_err_and(|(at, tries)| *tries < ART_TRIES && at.elapsed() >= ART_RETRY),
            _ => true,
        });
        // The track shows right away; its art follows once it's loaded.
        let wanted = publish(shared, playback, &art, !load);
        if let Some(url) = url.filter(|_| load) {
            let tries = match &art {
                Some((had, Err((_, tries)))) if *had == url => *tries,
                _ => 0,
            };
            let result = art::load(&url).map(Arc::new).map_err(|e| {
                eprintln!("music: album art: {e:#}");
                (Instant::now(), tries + 1)
            });
            art = Some((url, result));
            // Poll again before handing it over: the load can take a while,
            // and the track may have moved on meanwhile.
            continue;
        }
        let wait = match (failing, wanted) {
            (true, _) => RETRY,
            (false, true) => POLL,
            (false, false) => IDLE_POLL,
        };
        thread::park_timeout(wait);
    }
}

/// Hands the screen what's playing and its art, if `art` is that track's.
/// Returns whether the screen is up.
fn publish(shared: &Shared, playback: Option<Playback>, art: &Option<Loaded>, ready: bool) -> bool {
    let url = playback.as_ref().and_then(|p| p.track.art_url.as_ref());
    let mut s = shared.lock().unwrap();
    s.art = match art {
        Some((had, Ok(px))) if Some(had) == url => Some((had.clone(), px.clone())),
        _ => None,
    };
    if playback.as_ref().is_some_and(|p| p.playing) {
        s.playing_at = Some(Instant::now());
    }
    s.playback = playback;
    s.ready |= ready;
    s.wanted()
}

/// Every player with a track, the one to show first: a playing one,
/// sticking with `previous` while it plays, else `previous` paused, else
/// any paused one.
fn poll(previous: Option<&str>) -> Result<Option<Playback>> {
    let names = busctl(&[
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ListNames",
    ])?;
    let names = names["data"][0].as_array().context("ListNames: no names")?;
    let mut players: Vec<Playback> = names
        .iter()
        .filter_map(Value::as_str)
        .filter(|n| n.starts_with(PREFIX) && *n != PROXY)
        // A player can leave between the list and the call.
        .filter_map(|name| {
            let props = busctl(&[
                name,
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "GetAll",
                "s",
                "org.mpris.MediaPlayer2.Player",
            ])
            .ok()?;
            playback(name, &props["data"][0])
        })
        .collect();
    players.sort_by_key(|p| (!p.playing, previous != Some(p.player.as_str())));
    Ok(players.into_iter().next())
}

/// A player's state from its `GetAll` properties; none while it's stopped
/// or has no title.
fn playback(player: &str, props: &Value) -> Option<Playback> {
    let status = props["PlaybackStatus"]["data"].as_str()?;
    if status == "Stopped" {
        return None;
    }
    let meta = &props["Metadata"]["data"];
    let field = |key: &str| &meta[key]["data"];
    let title = field("xesam:title").as_str()?.trim();
    if title.is_empty() {
        return None;
    }
    // A list of artists, though some players send just the one.
    let artist = match field("xesam:artist") {
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        Value::String(name) => name.clone(),
        _ => String::new(),
    };
    let art_url = field("mpris:artUrl")
        .as_str()
        .filter(|u| !u.is_empty())
        .map(spotify_art);
    // Microseconds, as whichever number type the player picked.
    let micros = |v: &Value| {
        v.as_u64()
            .or_else(|| v.as_i64().map(|n| n.max(0) as u64))
            .or_else(|| {
                v.as_f64()
                    .filter(|n| n.is_finite())
                    .map(|n| n.max(0.0) as u64)
            })
            .map(Duration::from_micros)
    };
    let cut = |s: &str| s.trim().chars().take(MAX_CHARS).collect::<String>();
    Some(Playback {
        player: player.to_string(),
        track: Track {
            title: cut(title),
            artist: cut(&artist),
            art_url,
            length: micros(field("mpris:length")),
        },
        playing: status == "Playing",
        position: micros(&props["Position"]["data"]),
        rate: props["Rate"]["data"]
            .as_f64()
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(1.0),
        polled_at: Instant::now(),
    })
}

/// Older Spotify clients point at open.spotify.com/image/, which is gone;
/// the same image is on its CDN.
fn spotify_art(url: &str) -> String {
    match url.strip_prefix("https://open.spotify.com/image/") {
        Some(id) => format!("https://i.scdn.co/image/{id}"),
        None => url.to_string(),
    }
}

/// Calls a method on the session bus, returning the reply as JSON.
fn busctl(call: &[&str]) -> Result<Value> {
    let out = Command::new("busctl")
        .args(["--user", "--json=short", "--timeout=2", "call"])
        .args(call)
        .output()
        .context("running busctl")?;
    if !out.status.success() {
        bail!(
            "busctl {}: {}",
            call[3],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn props(status: &str, meta: Value) -> Value {
        json!({
            "PlaybackStatus": {"type": "s", "data": status},
            "Rate": {"type": "d", "data": 1.0},
            "Position": {"type": "x", "data": 30_000_000},
            "Metadata": {"type": "a{sv}", "data": meta},
        })
    }

    #[test]
    fn reads_a_players_state() {
        // As mpv-mpris reports it.
        let p = playback(
            "org.mpris.MediaPlayer2.mpv",
            &props(
                "Playing",
                json!({
                    "xesam:title": {"type": "s", "data": "Hoppípolla"},
                    "xesam:artist": {"type": "as", "data": ["Sigur Rós", "Amiina"]},
                    "mpris:length": {"type": "x", "data": 120_000_000},
                    "mpris:artUrl": {"type": "s", "data": "https://open.spotify.com/image/ab67"},
                }),
            ),
        )
        .unwrap();
        assert!(p.playing);
        assert_eq!(p.track.artist, "Sigur Rós, Amiina");
        assert_eq!(p.track.length, Some(Duration::from_secs(120)));
        assert_eq!(
            p.track.art_url.as_deref(),
            Some("https://i.scdn.co/image/ab67")
        );
        assert!((p.progress().unwrap() - 0.25).abs() < 0.01);
    }

    #[test]
    fn copes_with_odd_players() {
        let title = json!({"type": "s", "data": "Live"});
        // A stream: one artist as a plain string, no length, no art.
        let p = playback(
            "x",
            &props(
                "Paused",
                json!({"xesam:title": title, "xesam:artist": {"type": "s", "data": "Radio"}}),
            ),
        )
        .unwrap();
        assert!(!p.playing);
        assert_eq!(p.track.artist, "Radio");
        assert_eq!(p.progress(), None);
        // Length as an unsigned integer.
        let p = playback(
            "x",
            &props(
                "Playing",
                json!({"xesam:title": title, "mpris:length": {"type": "t", "data": 60_000_000u64}}),
            ),
        )
        .unwrap();
        assert_eq!(p.track.length, Some(Duration::from_secs(60)));
        // Length as a double, and a runaway title.
        let long = json!({"type": "s", "data": "na ".repeat(1000)});
        let p = playback(
            "x",
            &props(
                "Playing",
                json!({"xesam:title": long, "mpris:length": {"type": "d", "data": 6e7}}),
            ),
        )
        .unwrap();
        assert_eq!(p.track.length, Some(Duration::from_secs(60)));
        assert_eq!(p.track.title.chars().count(), MAX_CHARS);
        // Nothing to show.
        assert!(playback("x", &props("Stopped", json!({"xesam:title": title}))).is_none());
        let blank = json!({"xesam:title": {"type": "s", "data": " "}});
        assert!(playback("x", &props("Playing", blank)).is_none());
        assert!(playback("x", &json!({})).is_none());
    }

    #[test]
    fn a_paused_player_stays_a_while() {
        let mut s = State::default();
        assert!(!s.present());
        s.playing_at = Some(Instant::now());
        assert!(!s.present(), "played, but the player's gone");
        s.playback = playback(
            "x",
            &json!({
                "PlaybackStatus": {"type": "s", "data": "Paused"},
                "Metadata": {"type": "a{sv}", "data": {"xesam:title": {"type": "s", "data": "Hoppípolla"}}},
            }),
        );
        assert!(s.present());
        s.playing_at = Instant::now().checked_sub(GRACE);
        assert!(!s.present(), "paused too long");
    }
}
