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
use std::time::{Duration, Instant, SystemTime};

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
/// How long a paused player stays in the rotation, so a short pause
/// doesn't drop it.
const GRACE: Duration = Duration::from_secs(30);
const PREFIX: &str = "org.mpris.MediaPlayer2.";
/// playerctld mirrors whichever player is active; the player itself is
/// already on the list.
const PROXY: &str = "org.mpris.MediaPlayer2.playerctld";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub id: Option<String>,
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

/// Discovery is useful while hidden, but all work stops while disabled,
/// away or disconnected. Only the visible screen needs artwork.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Activity {
    #[default]
    Suspended,
    Hidden,
    Visible,
}

#[derive(Default)]
pub struct State {
    pub playback: Option<Playback>,
    pub art: Option<Arc<Pixmap>>,
    /// Whether the first poll, and any visible artwork, are done.
    pub ready: bool,
    /// Whether a poll has been accepted: until then there's no telling
    /// whether anything's playing, so not ready, art or no art.
    polled: bool,
    playing_at: Option<Instant>,
    activity: Activity,
    worker: Option<Thread>,
    art_worker: Option<Thread>,
    desired: Option<ArtKey>,
    cached: Option<Loaded>,
}

impl State {
    pub fn activity(&mut self, active: bool, visible: bool) {
        let next = match (active, visible) {
            (true, true) => Activity::Visible,
            (true, false) => Activity::Hidden,
            (false, _) => Activity::Suspended,
        };
        if self.activity != next {
            self.activity = next;
            // Discard artwork from an in-flight request; a fresh poll
            // names the track again. Coming into view, the cached cover of
            // the track still playing shows right away rather than the
            // placeholder until that poll, which also checks the file's
            // revision.
            self.desired = match (&self.cached, &self.playback) {
                (Some((key, Ok(_))), Some(playback))
                    if next == Activity::Visible && key.names(playback) =>
                {
                    Some(key.clone())
                }
                _ => None,
            };
            self.refresh_art();
            if let Some(worker) = &self.worker {
                worker.unpark();
            }
        }
    }

    /// Asked while suspended too (`on music`, coming back from away), when
    /// the last poll is all there is. Suspended polls aren't accepted, so a
    /// stale player still ages out after `GRACE`.
    pub fn present(&self) -> bool {
        self.playback.is_some() && self.playing_at.is_some_and(|t| t.elapsed() < GRACE)
    }

    fn accept(&mut self, playback: Option<Playback>, desired: Option<ArtKey>) {
        // A poll that was already in flight when suspended can't restart
        // artwork fetching or bring an old player back into the rotation.
        if self.activity == Activity::Suspended {
            return;
        }
        self.desired = desired.filter(|_| self.activity == Activity::Visible);
        if playback.as_ref().is_some_and(|p| p.playing) {
            self.playing_at = Some(Instant::now());
        }
        self.playback = playback;
        self.polled = true;
        self.refresh_art();
        if self.next_art().is_some()
            && let Some(worker) = &self.art_worker
        {
            worker.unpark();
        }
    }

    fn refresh_art(&mut self) {
        self.art = match (&self.desired, &self.cached) {
            (Some(key), Some((had, Ok(px)))) if key == had => Some(px.clone()),
            _ => None,
        };
        self.ready |= self.polled
            && (self.desired.is_none()
                || self
                    .cached
                    .as_ref()
                    .is_some_and(|(key, _)| Some(key) == self.desired.as_ref()));
    }

    fn next_art(&self) -> Option<ArtKey> {
        let key = self.desired.as_ref()?;
        match &self.cached {
            Some((had, Ok(_))) if had == key => None,
            Some((had, Err((at, tries))))
                if had == key && (*tries >= ART_TRIES || at.elapsed() < ART_RETRY) =>
            {
                None
            }
            _ => Some(key.clone()),
        }
    }
}

pub type Shared = Arc<Mutex<State>>;

pub fn spawn() -> Shared {
    let shared = Shared::default();
    let s = shared.clone();
    thread::spawn(move || run(&s));
    let s = shared.clone();
    thread::spawn(move || {
        s.lock().unwrap().art_worker = Some(thread::current());
        loop {
            if !load_next_art(&s, art::load) {
                thread::park();
            }
        }
    });
    shared
}

/// Polls once on the caller's thread, for a command that needs to know
/// right away whether anything's playing after the poller was parked.
/// Blocks for the `busctl` round trips, a few ms.
pub fn refresh(shared: &Shared) {
    let previous = shared
        .lock()
        .unwrap()
        .playback
        .as_ref()
        .map(|p| p.player.clone());
    // On failure the poller says why on its next round.
    if let Ok(playback) = poll(previous.as_deref()) {
        publish(shared, playback);
    }
}

/// Identity includes the player, track metadata and local file revision:
/// players can reuse a cover path, even without changing the track ID.
/// Not the length, which browsers and Spotify often send a poll or two
/// after the rest, so it would load the same cover twice.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ArtKey {
    player: String,
    id: Option<String>,
    title: String,
    artist: String,
    url: String,
    file_stamp: Option<(SystemTime, u64)>,
}

impl ArtKey {
    fn of(playback: &Playback) -> Option<Self> {
        let key = Self::unstamped(playback)?;
        Some(Self {
            file_stamp: art::file_stamp(&key.url),
            ..key
        })
    }

    fn unstamped(playback: &Playback) -> Option<Self> {
        let track = &playback.track;
        Some(Self {
            player: playback.player.clone(),
            id: track.id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            url: track.art_url.clone()?,
            file_stamp: None,
        })
    }

    /// Whether this is `playback`'s art, as far as can be told without
    /// looking at the file.
    fn names(&self, playback: &Playback) -> bool {
        Self::unstamped(playback).is_some_and(|key| {
            *self
                == Self {
                    file_stamp: self.file_stamp,
                    ..key
                }
        })
    }
}

type Loaded = (ArtKey, Result<Arc<Pixmap>, (Instant, u32)>);

/// The single artwork worker has at most one load in flight and one
/// desired track. Changes replace that desired track, rather than queue
/// downloads; an obsolete result is never published.
fn load_next_art(shared: &Shared, load: impl FnOnce(&str) -> Result<Pixmap>) -> bool {
    let (key, tries) = {
        let s = shared.lock().unwrap();
        let Some(key) = s.next_art() else {
            return false;
        };
        let tries = match &s.cached {
            Some((had, Err((_, tries)))) if *had == key => *tries,
            _ => 0,
        };
        (key, tries)
    };
    let result = load(&key.url).map(Arc::new).map_err(|e| {
        eprintln!("music: album art: {e:#}");
        (Instant::now(), tries + 1)
    });
    let mut s = shared.lock().unwrap();
    if s.desired.as_ref() == Some(&key) {
        s.cached = Some((key, result));
        s.refresh_art();
    }
    true
}

/// Stat local files outside the shared lock: the frame loop must never
/// wait for filesystem I/O on the poller's behalf. If visibility flips
/// between the two locks, the key is worked out again for the new one.
fn publish(shared: &Shared, playback: Option<Playback>) -> Activity {
    let mut visible = shared.lock().unwrap().activity == Activity::Visible;
    loop {
        let key = visible
            .then(|| playback.as_ref().and_then(ArtKey::of))
            .flatten();
        let mut s = shared.lock().unwrap();
        if (s.activity == Activity::Visible) == visible {
            s.accept(playback, key);
            return s.activity;
        }
        visible = !visible;
    }
}

fn run(shared: &Shared) {
    shared.lock().unwrap().worker = Some(thread::current());
    let mut failing = false;
    loop {
        let previous = {
            let s = shared.lock().unwrap();
            if s.activity == Activity::Suspended {
                drop(s);
                thread::park();
                continue;
            }
            s.playback.as_ref().map(|p| p.player.clone())
        };
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
        let activity = publish(shared, playback);
        let wait = match (failing, activity) {
            (_, Activity::Suspended) => continue,
            (true, _) => RETRY,
            (false, Activity::Visible) => POLL,
            (false, Activity::Hidden) => IDLE_POLL,
        };
        thread::park_timeout(wait);
    }
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
            id: field("mpris:trackid").as_str().map(String::from),
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
                    "mpris:trackid": {"type": "o", "data": "/track/123"},
                    "mpris:length": {"type": "x", "data": 120_000_000},
                    "mpris:artUrl": {"type": "s", "data": "https://open.spotify.com/image/ab67"},
                }),
            ),
        )
        .unwrap();
        assert!(p.playing);
        assert_eq!(p.track.id.as_deref(), Some("/track/123"));
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
        s.activity(true, false);
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

    fn track(title: &str, url: &str) -> Playback {
        playback(
            "test-player",
            &props(
                "Playing",
                json!({
                    "xesam:title": {"data": title},
                    "mpris:trackid": {"data": format!("/track/{title}")},
                    "mpris:artUrl": {"data": url},
                }),
            ),
        )
        .unwrap()
    }

    fn visible() -> Shared {
        let shared = Shared::default();
        shared.lock().unwrap().activity(true, true);
        shared
    }

    #[test]
    fn reloads_a_reused_cover_path_for_new_tracks_and_file_revisions() {
        use std::fs::{File, FileTimes};
        let path = std::env::temp_dir().join(format!(
            "cooler-lcd-reused-cover-{}.png",
            std::process::id()
        ));
        let url = format!("file://{}", path.display());
        let write = |color, when| {
            let mut px = Pixmap::new(16, 16).unwrap();
            px.fill(color);
            px.save_png(&path).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(when))
                .unwrap();
        };
        let now = SystemTime::now();
        write(tiny_skia::Color::from_rgba8(255, 0, 0, 255), now);
        let shared = visible();
        publish(&shared, Some(track("A", &url)));
        assert!(load_next_art(&shared, art::load));
        assert!(
            shared
                .lock()
                .unwrap()
                .art
                .as_ref()
                .unwrap()
                .pixel(0, 0)
                .unwrap()
                .red()
                > 200
        );
        // Same URL and metadata, but the file changed during the track.
        write(
            tiny_skia::Color::from_rgba8(0, 255, 0, 255),
            now + Duration::from_secs(2),
        );
        publish(&shared, Some(track("A", &url)));
        assert!(shared.lock().unwrap().art.is_none());
        assert!(load_next_art(&shared, art::load));
        assert!(
            shared
                .lock()
                .unwrap()
                .art
                .as_ref()
                .unwrap()
                .pixel(0, 0)
                .unwrap()
                .green()
                > 200
        );
        assert!(!load_next_art(&shared, |_| panic!(
            "unchanged art must stay cached"
        )));
        // A new track or player must invalidate even an unchanged URL.
        publish(&shared, Some(track("B", &url)));
        assert!(load_next_art(&shared, art::load));
        let mut other = track("B", &url);
        other.player = "other-player".into();
        publish(&shared, Some(other));
        assert!(load_next_art(&shared, art::load));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn slow_art_doesnt_block_playback_and_obsolete_results_are_discarded() {
        use std::sync::mpsc;
        let shared = visible();
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        let (started, waiting) = mpsc::channel();
        let (release, finish) = mpsc::channel();
        let worker = shared.clone();
        let worker = thread::spawn(move || {
            load_next_art(&worker, |_| {
                started.send(()).unwrap();
                finish.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(Pixmap::new(480, 480).unwrap())
            })
        });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        // This is the same publish path used by the independent poller.
        publish(&shared, Some(track("B", "https://example.test/b.jpg")));
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .playback
                .as_ref()
                .unwrap()
                .track
                .title,
            "B"
        );
        release.send(()).unwrap();
        assert!(worker.join().unwrap());
        assert!(
            shared.lock().unwrap().art.is_none(),
            "A must not overwrite B"
        );
        assert!(load_next_art(&shared, |url| {
            assert!(url.ends_with("/b.jpg"));
            Ok(Pixmap::new(480, 480).unwrap())
        }));
        assert!(shared.lock().unwrap().art.is_some());
    }

    #[test]
    fn hidden_music_discovers_players_without_loading_art_and_suspension_cancels_work() {
        let shared = Shared::default();
        let mut s = shared.lock().unwrap();
        s.activity(true, false);
        drop(s);
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        let s = shared.lock().unwrap();
        assert!(s.present(), "hidden discovery keeps music in the rotation");
        drop(s);
        assert!(!load_next_art(&shared, |_| panic!(
            "hidden screen must not fetch"
        )));
        let mut s = shared.lock().unwrap();
        s.activity(true, true);
        drop(s);
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        let mut s = shared.lock().unwrap();
        assert!(s.next_art().is_some());
        s.activity(false, false);
        drop(s);
        publish(&shared, Some(track("B", "https://example.test/b.jpg")));
        let s = shared.lock().unwrap();
        assert!(s.present(), "the last poll still counts while suspended");
        assert!(s.next_art().is_none());
        assert_eq!(
            s.playback.as_ref().unwrap().track.title,
            "A",
            "ignore a suspended poll"
        );
        drop(s);
        assert!(!load_next_art(&shared, |_| panic!(
            "suspended screen must not fetch"
        )));
    }

    #[test]
    fn not_ready_until_the_first_poll() {
        // As `--preview` starts it: in view before the poller has answered.
        let shared = visible();
        assert!(!shared.lock().unwrap().ready, "no poll yet");
        publish(&shared, None);
        assert!(
            shared.lock().unwrap().ready,
            "nothing playing, no art to wait for"
        );

        // A track with art waits for the cover too.
        let shared = visible();
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        assert!(!shared.lock().unwrap().ready);
        assert!(load_next_art(&shared, |_| Ok(
            Pixmap::new(480, 480).unwrap()
        )));
        assert!(shared.lock().unwrap().ready);
    }

    #[test]
    fn art_stays_up_when_music_comes_back_into_view() {
        let shared = visible();
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        assert!(load_next_art(&shared, |_| Ok(
            Pixmap::new(480, 480).unwrap()
        )));
        shared.lock().unwrap().activity(true, false);
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        let mut s = shared.lock().unwrap();
        s.activity(true, true);
        assert!(s.art.is_some(), "no placeholder before the next poll");
        assert!(s.next_art().is_none());
        // A different track while hidden gets no stale cover.
        s.activity(true, false);
        drop(s);
        publish(&shared, Some(track("B", "https://example.test/a.jpg")));
        let mut s = shared.lock().unwrap();
        s.activity(true, true);
        assert!(s.art.is_none());
    }

    #[test]
    fn a_late_length_doesnt_reload_the_cover() {
        let shared = visible();
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        assert!(load_next_art(&shared, |_| Ok(
            Pixmap::new(480, 480).unwrap()
        )));
        let mut later = track("A", "https://example.test/a.jpg");
        later.track.length = Some(Duration::from_secs(180));
        publish(&shared, Some(later));
        assert!(shared.lock().unwrap().art.is_some());
        assert!(!load_next_art(&shared, |_| panic!("same cover")));
    }

    #[test]
    fn failed_art_is_retried_with_a_limit_and_new_tracks_get_a_fresh_budget() {
        let shared = visible();
        publish(&shared, Some(track("A", "https://example.test/a.jpg")));
        for _ in 0..ART_TRIES {
            assert!(load_next_art(&shared, |_| anyhow::bail!("offline")));
            let mut s = shared.lock().unwrap();
            assert!(s.next_art().is_none(), "back off after failure");
            if let Some((_, Err((at, _)))) = &mut s.cached {
                *at -= ART_RETRY;
            }
        }
        assert!(!load_next_art(&shared, |_| panic!(
            "retry budget exhausted"
        )));
        publish(&shared, Some(track("B", "https://example.test/a.jpg")));
        assert!(load_next_art(&shared, |_| Ok(
            Pixmap::new(480, 480).unwrap()
        )));
    }
}
