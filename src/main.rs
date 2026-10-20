//! cooler-lcd: drives the LCD on Thermalright Vision coolers with screens
//! (clock, temperatures, ...) drawn in Omarchy theme colors.
//!
//! Usage: see `USAGE`. The verbs steer the running service (see
//! `control`); `--screen` shows just that screen instead of the config's
//! list.

mod config;
mod content;
mod control;
mod desktop;
mod device;
mod draw;
mod log;
mod night;
mod picture;
mod render;
mod screens;
mod sender;
mod sun;
mod theme;

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::{DateTime, Local, TimeDelta};

use config::Config;
use control::{Command, Request};
use desktop::Desktop;
use device::HEADER_LEN;
use log::Log;
use night::Night;
use render::Renderer;
use screens::Screen;
use sender::{Receipt, Sender};
use theme::Theme;

/// How often to check for the panel while it's disconnected.
const OFFLINE_TICK: Duration = Duration::from_millis(500);
/// How often every screen is asked whether it wants to take over.
const ALERT_CHECK: Duration = Duration::from_secs(1);
/// A condition takes over again only after going this long unseen, so one
/// that flickers at a threshold, or drops out of the radar's data between
/// its turns on screen, doesn't keep grabbing the panel.
const REARM: Duration = Duration::from_secs(10 * 60);
/// Time between frames while the rotation bar is moving, so it glides
/// rather than jumps on screens that otherwise draw once a second. Cheap,
/// since moving it re-encodes only the bottom row of the frame. Screens
/// keep their own pace in between.
const BAR_TICK: Duration = Duration::from_millis(33);
/// How often to look for a theme switch.
const THEME_CHECK: Duration = Duration::from_secs(1);

/// Printed for `--help`, and after an argument it doesn't know.
const USAGE: &str = "\
usage:
  cooler-lcd [--screen NAME]                    run, cycling through the configured screens
  cooler-lcd [--screen NAME] --preview [FILE]   render one frame to a JPEG file and exit
                               [--night]         ... dimmed as at night
  cooler-lcd next | show NAME                  steer the running service
  cooler-lcd flash TEXT [OPTIONS]              show a message
  cooler-lcd image FILE [OPTIONS]              show an image, scaled/cropped to 480x480
  cooler-lcd on NAME | off NAME | toggle NAME   put a screen in or out of the rotation
  cooler-lcd snapshot FILE                      save what the panel shows as a JPEG
  cooler-lcd timer LENGTH | timer stop          count down on the panel (25m, 1h30m, 90s)
  cooler-lcd night [on | off | auto]            dim the panel, keep it bright, or follow the schedule

content options:
  --for LENGTH       stay for this long (30s, 2m); default: one rotation
  --until-next       stay until dismissed with next or replaced
  --wait             wait until the first frame is sent to the panel
  --color '#rrggbb'   accent color for a flash message
  --icon TEXT        1-4 characters from the theme font, above the message
  --                 treat all remaining arguments as literal content";

/// The command line, past the program name, when it isn't a verb.
#[derive(Debug, Default, PartialEq)]
struct Args {
    help: bool,
    screen: Option<String>,
    /// Where to write a preview frame, if that's what's asked for.
    preview: Option<String>,
    /// Whether the preview is dimmed as at night.
    night: bool,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut out = Args::default();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => out.help = true,
            "--screen" => match args.next_if(|n| !n.starts_with('-')) {
                Some(name) => out.screen = Some(name.clone()),
                None => bail!("--screen needs a name: {}", screens::NAMES.join(", ")),
            },
            "--preview" => {
                let file = args.next_if(|f| !f.starts_with('-'));
                out.preview = Some(file.map_or("preview.jpg".into(), String::clone));
            }
            "--night" => out.night = true,
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(out)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(verb) = args.get(1)
        && control::VERBS.contains(&verb.as_str())
    {
        if args.get(2).is_some_and(|a| a == "--help" || a == "-h") {
            println!("{USAGE}");
            return Ok(());
        }
        let message = control::send(&args[1..])?;
        if !message.is_empty() {
            println!("{message}");
        }
        return Ok(());
    }
    let args = parse_args(&args[1..])?;
    if args.night && args.preview.is_none() {
        bail!("--night is for --preview; `cooler-lcd night on` dims the running service");
    }
    if args.help {
        println!("{USAGE}");
        return Ok(());
    }
    let mut log = Log::default();

    let config = Config::load().unwrap_or_else(|e| {
        log.error(format!("{e:#}; using defaults"));
        Config::default()
    });
    let mut carousel = Carousel::new(args.screen, &config, &mut log)?;
    // The theme as loaded, and as drawn: at night a light one turns dark.
    let mut day_theme = Theme::load(false)?;
    let mut theme = day_theme.clone();
    let mut renderer = Renderer::new();

    if let Some(path) = args.preview {
        let screen = carousel.current(false);
        screen.activity(true, true);
        screen.update(Local::now());
        // Give background fetchers (network data) a moment to deliver.
        for _ in 0..50 {
            if !screen.loading() {
                break;
            }
            sleep(Duration::from_millis(100));
            screen.update(Local::now());
        }
        if args.night {
            theme = day_theme.for_night();
            renderer.set_night(Some(config.night.brightness));
        }
        renderer.render(screen.as_ref(), &theme)?;
        std::fs::write(&path, &renderer.frame()[HEADER_LEN..])?;
        println!("wrote {path}");
        return Ok(());
    }

    // Before the sender, so a second instance exits instead of lying in
    // wait for the panel. Without a socket the service still runs, holding
    // on to the lock all the same.
    let (_lock, requests) = match control::start() {
        Err(e) if e.is::<control::AlreadyRunning>() => return Err(e),
        Err(e) => (None, Err(e)),
        Ok((lock, requests)) => (Some(lock), requests),
    };
    let requests = requests
        .inspect_err(|e| log.error(format!("control socket: {e:#}")))
        .ok();
    let sender = Sender::spawn();
    let desktop = Desktop::spawn();
    let mut away_screen = screens::away();
    let mut was_away = false;
    let mut showed_away = false;
    let mut theme_stamp = theme::stamp();
    let mut theme_checked_at = Instant::now();
    let mut night = Night::new(&config);
    let mut was_dark = false;
    let mut dirty = true;
    // When the screen on display is next due an update.
    let mut next_update = Instant::now();
    // The rotation bar's length on the panel, in pixels.
    let mut shown_bar = None;
    // A request that cut the last sleep short.
    let mut woken = None;
    loop {
        let tick = Instant::now();
        carousel.flash_drawn();
        // Before any requests, so content pushed after the timer ran out
        // isn't taken for content it ran out under, even when unplugged.
        dirty |= carousel.ring();

        let queued = requests.iter().flat_map(|r| r.try_iter());
        for request in woken.take().into_iter().chain(queued) {
            dirty |= handle_request(
                &mut carousel,
                &mut night,
                request,
                desktop.away(),
                renderer.frame(),
            );
        }

        // Nothing to draw for until the sender has a panel; redraw once it does.
        if !sender.connected() {
            carousel.activity(false);
            dirty = true;
            woken = idle(requests.as_ref(), OFFLINE_TICK);
            continue;
        }

        // Follow Omarchy theme and font switches. The theme directory is
        // briefly missing while Omarchy swaps it in, so ignore that state.
        let stamp = if theme_checked_at.elapsed() >= THEME_CHECK {
            theme_checked_at = Instant::now();
            theme::stamp()
        } else {
            None
        };
        if stamp.is_some() && stamp != theme_stamp {
            match Theme::load(true) {
                Ok(t) => {
                    theme = if was_dark { t.for_night() } else { t.clone() };
                    day_theme = t;
                    theme_stamp = stamp;
                    dirty = true;
                }
                Err(e) => log.error(format!("reloading theme: {e:#}")),
            }
        }

        // While the desktop is locked or dark, a dim clock stands in for the
        // carousel, whose screens then stop updating (and fetching).
        let away = desktop.away();
        if away != was_away {
            if !away {
                carousel.resume();
            }
            log.info(
                if away {
                    "desktop away; dimming"
                } else {
                    "desktop back"
                }
                .into(),
            );
            was_away = away;
        }
        // Expire a flash before choosing what to show, so the carousel's
        // screens never get drawn while away, not even for one tick.
        dirty |= if away {
            carousel.expire_flash()
        } else {
            carousel.advance(&mut log)
        };
        // A running timer shows over the dim clock; one that's up waits
        // behind it, so it doesn't glow all night, as does held content.
        let show_away = away
            && carousel.overlay(away).is_none()
            && !(carousel.timing() && !carousel.timer_up());
        if show_away != showed_away {
            showed_away = show_away;
            dirty = true;
        }
        carousel.activity(!away);
        // At night every frame is dimmed, except the away clock's, which
        // is dim already and would all but vanish.
        let dark = night.dark(Local::now());
        if dark != was_dark {
            log.info(
                if dark {
                    "night: dimming"
                } else {
                    "night: back to day"
                }
                .into(),
            );
            was_dark = dark;
            theme = if dark {
                day_theme.for_night()
            } else {
                day_theme.clone()
            };
            dirty = true;
        }
        dirty |= renderer.set_night((dark && !show_away).then_some(config.night.brightness));
        let rotation = carousel.progress().filter(|_| config.rotate_bar);
        let screen = if show_away {
            &mut away_screen
        } else {
            carousel.current(away)
        };
        // Each screen updates at its own pace (and right away after a
        // switch or a theme change); the bar moves in between.
        if dirty || tick >= next_update {
            dirty |= screen.update(Local::now());
            next_update = tick + screen.interval();
        }
        // A screen's own progress (a track's) takes the place of the
        // countdown to the next screen.
        let bar = (!show_away)
            .then(|| screen.bar().or(rotation))
            .flatten()
            .map(|p| (p * render::SIZE as f32).round() as u32);
        let wake = match bar {
            Some(_) => next_update.min(tick + BAR_TICK),
            None => next_update,
        };

        // On a render error the sender keeps showing the previous frame;
        // retry next tick.
        if dirty || bar != shown_bar {
            let encoded = if dirty {
                renderer.draw(screen.as_ref(), &theme);
                renderer.encode(&theme, bar)
            } else {
                renderer.move_bar(&theme, bar)
            };
            match encoded {
                Ok(()) => {
                    dirty = false;
                    shown_bar = bar;
                    let receipt = carousel.overlay(away).map(|f| f.receipt.clone());
                    sender.publish(renderer.frame(), receipt);
                }
                Err(e) => log.error(format!("rendering: {e:#}")),
            }
        }

        let wake = carousel
            .overlay(away)
            .and_then(Overlay::wake)
            .map_or(wake, |deadline| wake.min(deadline));
        woken = idle(
            requests.as_ref(),
            wake.saturating_duration_since(Instant::now()),
        );
    }
}

/// Sleeps for `length`, or until a request comes in, which it returns, so
/// commands are carried out right away rather than at the next frame.
fn idle(requests: Option<&Receiver<Request>>, length: Duration) -> Option<Request> {
    match requests.map(|r| r.recv_timeout(length)) {
        Some(Ok(request)) => Some(request),
        Some(Err(RecvTimeoutError::Timeout)) => None,
        // No socket, or its listener is gone.
        _ => {
            sleep(length);
            None
        }
    }
}

/// Apply a queued command and transfer --wait replies to its overlay.
/// This runs even while the USB sender is offline.
fn handle_request(
    carousel: &mut Carousel,
    night: &mut Night,
    request: Request,
    away: bool,
    frame: &[u8],
) -> bool {
    let Request {
        command,
        reply,
        expires,
        order,
    } = request;
    if expires.is_some_and(|at| Instant::now() >= at) {
        return false;
    }
    // An image takes a while to decode; one accepted before a command
    // that has since changed what's pushed is out of date.
    if matches!(command, Command::Content(_)) && order < carousel.overlay_order {
        let _ = reply.send(Err("a newer command came first; not shown".into()));
        return false;
    }
    let sets_overlay = matches!(
        command,
        Command::Content(_) | Command::Next | Command::Show(_) | Command::Timer(_)
    );
    let wait = matches!(&command, Command::Content(c) if c.wait);
    let outcome = match command {
        Command::Snapshot(path) => snapshot(frame, &path),
        Command::Night(mode) => Ok(night.command(mode, Local::now())),
        Command::Next if carousel.flashing() || carousel.timer_up() => {
            carousel.command(Command::Next)
        }
        // `show` would take down a timer that's up from under the clock.
        Command::Show(_) if away => Err("the desktop is away, so the panel shows the clock".into()),
        Command::Next if away && !carousel.timing() => {
            Err("the desktop is away, so the panel shows the clock".into())
        }
        command => carousel.command(command),
    };
    let changed = outcome.is_ok();
    if changed && sets_overlay {
        carousel.overlay_order = carousel.overlay_order.max(order);
    }
    if wait && changed {
        carousel.flash.as_mut().unwrap().reply = Some(reply);
    } else {
        let _ = reply.send(outcome);
    }
    changed
}

/// Saves `frame`, the one on the panel, to `path` as a JPEG.
fn snapshot(frame: &[u8], path: &std::path::Path) -> Result<String, String> {
    let jpeg = frame.get(HEADER_LEN..).filter(|j| !j.is_empty());
    let jpeg = jpeg.ok_or("nothing on the panel yet")?;
    write_regular(path, jpeg).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!("wrote {}", path.display()))
}

/// Writes `bytes` to `path`, a new or regular file, without blocking: this
/// runs on the frame loop, where a named pipe with no reader would freeze
/// the panel. Opening a device can have side effects, so check before
/// opening too (as `picture::load` does).
fn write_regular(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let not_regular = || Error::new(ErrorKind::InvalidInput, "not a regular file");
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => return Err(not_regular()),
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // O_NONBLOCK, against a pipe swapped in since the check; truncate
    // only once it's known to be a regular file.
    let mut file = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    file.set_len(0)?;
    file.write_all(bytes)
}

/// The configured screens, shown one at a time for `rotate` each. A screen
/// with a new alert jumps the queue and stays up for a full `rotate` (several
/// take their turns). A timer holds the panel until it's stopped or, once
/// it's up, dismissed, and a flashed message goes in front of them all for
/// a `rotate`.
struct Carousel {
    /// A message from `cooler-lcd flash` and when it was first drawn. Its
    /// time runs from then, so one sent while the panel is unplugged still
    /// gets its full turn once it's back.
    flash: Option<Overlay>,
    /// A countdown from `cooler-lcd timer` and when it's up.
    timer: Option<(Box<dyn Screen>, DateTime<Local>)>,
    /// Whether the timer's time being up has taken down pushed content.
    rang: bool,
    /// The newest accepted request that changed what's pushed.
    overlay_order: u64,
    screens: Vec<(String, Box<dyn Screen>)>,
    /// Which screens are in the rotation; at least one always is. One
    /// that's off is only shown when asked for by name, and doesn't take
    /// over.
    on: Vec<bool>,
    /// Each screen's alert conditions and when each was last seen, to spot
    /// new ones.
    seen: Vec<HashMap<String, Instant>>,
    /// Screens with new alerts waiting for their turn, oldest first.
    pending: VecDeque<usize>,
    /// Whether the current screen is up because of an alert.
    taken_over: bool,
    checked_at: Option<Instant>,
    index: usize,
    shown_at: Instant,
    rotate: Duration,
}

/// An overlay's lifetime starts after USB accepts a frame. A superseded
/// --wait command must be answered too, even if it never reached the panel.
struct Overlay {
    screen: Box<dyn Screen>,
    duration: Option<Duration>,
    drawn_at: Option<Instant>,
    receipt: Receipt,
    reply: Option<std::sync::mpsc::Sender<Result<String, String>>>,
}

impl Overlay {
    /// Notice delivery promptly, then wake at the exact expiry even when
    /// the underlying still screen only updates once a second.
    fn wake(&self) -> Option<Instant> {
        match self.drawn_at {
            None => Some(Instant::now() + Duration::from_millis(50)),
            Some(at) => self.duration.and_then(|length| at.checked_add(length)),
        }
    }

    fn new(screen: Box<dyn Screen>, duration: Option<Duration>) -> Self {
        Self {
            screen,
            duration,
            drawn_at: None,
            receipt: Receipt::default(),
            reply: None,
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let outcome = if self.receipt.sent_at().is_some() {
                Ok(String::new())
            } else {
                Err("content replaced or dismissed before it was shown".into())
            };
            let _ = reply.send(outcome);
        }
    }
}

impl Carousel {
    /// Just `screen`, which has to be one, if asked for by name; else the
    /// configured screens.
    fn new(screen: Option<String>, config: &Config, log: &mut Log) -> Result<Self> {
        Self::configured(screen, config, log, |name| screens::build(name, config))
    }

    /// `new`, with the screens made by `build`. A config with none it can
    /// show gets the defaults, as one that doesn't parse does, rather than
    /// the service exiting (and restarting) over it.
    fn configured(
        screen: Option<String>,
        config: &Config,
        log: &mut Log,
        mut build: impl FnMut(&str) -> Option<Box<dyn Screen>>,
    ) -> Result<Self> {
        let rotate = config.rotate();
        match screen {
            // A screen asked for by name is shown even if the config has it off.
            Some(name) => Self::build(&[name], &[], rotate, log, build),
            None => {
                Self::build(&config.screens, &config.off, rotate, log, &mut build).or_else(|e| {
                    log.error(format!("{e:#}; showing the default screens"));
                    // None off, so the config can't take these out too.
                    Self::build(&Config::default().screens, &[], rotate, log, build)
                })
            }
        }
    }

    /// The carousel of `names`, made by `build`, with those in `off` out of
    /// the rotation.
    fn build(
        names: &[String],
        off: &[String],
        rotate: Duration,
        log: &mut Log,
        mut build: impl FnMut(&str) -> Option<Box<dyn Screen>>,
    ) -> Result<Self> {
        for name in off.iter().filter(|n| !names.contains(n)) {
            log.error(format!("off: {name:?} isn't one of the configured screens"));
        }
        let mut screens = Vec::new();
        for (i, name) in names.iter().enumerate() {
            // `on`, `off` and `show` only reach the first of a name, so a
            // second would be a copy beyond their reach, workers and all.
            if names[..i].contains(name) {
                log.error(format!("screen {name:?} is listed twice; showing it once"));
                continue;
            }
            match build(name) {
                Some(s) => screens.push((name.clone(), s)),
                None => log.error(format!(
                    "unknown screen {name:?} (available: {})",
                    screens::NAMES.join(", ")
                )),
            }
        }
        if screens.is_empty() {
            bail!("no usable screens configured");
        }
        let mut carousel = Self::with_screens(screens, rotate);
        carousel.on = carousel
            .screens
            .iter()
            .map(|(name, _)| !off.contains(name))
            .collect();
        if !carousel.on.contains(&true) {
            log.error("every screen is off; turning the first one on".into());
            carousel.on[0] = true;
        }
        // The first one on with something to show, counting from the top.
        carousel.index = carousel.next_on(carousel.screens.len() - 1);
        Ok(carousel)
    }

    fn with_screens(screens: Vec<(String, Box<dyn Screen>)>, rotate: Duration) -> Self {
        Self {
            flash: None,
            timer: None,
            rang: false,
            overlay_order: 0,
            seen: vec![HashMap::new(); screens.len()],
            on: vec![true; screens.len()],
            screens,
            pending: VecDeque::new(),
            taken_over: false,
            checked_at: None,
            index: 0,
            shown_at: Instant::now(),
            rotate,
        }
    }

    /// Moves to a screen with a new alert, else to the next screen when the
    /// current one's time is up. Returns true on a switch.
    fn advance(&mut self, log: &mut Log) -> bool {
        if self.flash.is_some() {
            // Alerts under pushed content are still noticed, and take
            // their turn once it's down.
            if self.screens.len() >= 2 && self.timer.is_none() {
                self.check_alerts(log);
            }
            return self.expire_flash();
        }
        // The rotation, takeovers included, waits for the timer.
        if self.screens.len() < 2 || self.timer.is_some() {
            return false;
        }
        self.check_alerts(log);
        let due = self.shown_at.elapsed() >= self.rotate;
        // A takeover cuts in right away, unless another one is still
        // having its turn.
        if (!self.taken_over || due)
            && let Some(i) = self.pending.pop_front()
        {
            let switched = i != self.index;
            self.index = i;
            self.shown_at = Instant::now();
            self.taken_over = true;
            return switched;
        }
        if !due {
            return false;
        }
        self.taken_over = false;
        self.shown_at = Instant::now();
        let next = self.next_on(self.index);
        let switched = next != self.index;
        self.index = next;
        switched
    }

    /// The first screen in the rotation after `i` with something to show,
    /// round to `i` itself if it's the only one. If none has, the next one
    /// on, so the panel doesn't stick on a screen that's off.
    fn next_on(&self, i: usize) -> usize {
        let n = self.screens.len();
        let order = || (1..=n).map(move |k| (i + k) % n);
        order()
            .find(|&j| self.on[j] && self.screens[j].1.present())
            .or_else(|| order().find(|&j| self.on[j]))
            .unwrap_or(i)
    }

    /// How far the current screen is through its turn, 0..=1, while another
    /// screen is due after it: not with one screen on, nor during a flash
    /// or a timer.
    fn progress(&self) -> Option<f32> {
        if self.flash.is_some() || self.timer.is_some() || self.next_on(self.index) == self.index {
            return None;
        }
        Some((self.shown_at.elapsed().as_secs_f32() / self.rotate.as_secs_f32()).min(1.0))
    }

    /// Asks every screen for its alerts, at most once per `ALERT_CHECK`, and
    /// queues the screens that have a condition not seen within `REARM`.
    fn check_alerts(&mut self, log: &mut Log) {
        if self.checked_at.is_some_and(|t| t.elapsed() < ALERT_CHECK) {
            return;
        }
        let now = Instant::now();
        self.checked_at = Some(now);
        let local = Local::now();
        for (i, ((name, screen), seen)) in self.screens.iter_mut().zip(&mut self.seen).enumerate() {
            seen.retain(|_, at| now.duration_since(*at) < REARM);
            if !self.on[i] || !screen.present() {
                continue;
            }
            for alert in screen.alerts(local) {
                if seen.insert(alert.clone(), now).is_none() {
                    log.info(format!("{name}: {alert}"));
                    if !self.pending.contains(&i) {
                        self.pending.push_back(i);
                    }
                }
            }
        }
    }

    /// Carries out a command from the control socket. Returns a message
    /// for the user, which may be empty.
    fn command(&mut self, command: Command) -> Result<String, String> {
        match command {
            Command::Next if self.flashing() => {
                self.flash = None;
                self.taken_over = false;
                self.shown_at = Instant::now();
            }
            Command::Next | Command::Show(_) if self.timing() && !self.timer_up() => {
                return Err("a timer is running; `cooler-lcd timer stop` cancels it".into());
            }
            // Dismisses a timer that's up, back to the screen it covered.
            Command::Next if self.timer.is_some() => self.end_timer(),
            Command::Next => self.switch_to(self.next_on(self.index)),
            // A screen that's off gets one turn, then the rotation goes on.
            Command::Show(name) => {
                let i = self.find(&name)?;
                self.timer = None;
                self.switch_to(i);
            }
            Command::On(name) => return self.turn_on(&name),
            Command::Off(name) => return self.turn_off(&name),
            Command::Toggle(name) => {
                return match self.on[self.find(&name)?] {
                    true => self.turn_off(&name),
                    false => self.turn_on(&name),
                };
            }
            Command::Content(content) => {
                let duration = match content.lifetime {
                    content::Lifetime::Rotation => Some(self.rotate),
                    content::Lifetime::For(length) => Some(length),
                    content::Lifetime::UntilNext => None,
                };
                self.flash = Some(Overlay::new(content.screen(), duration));
            }
            Command::Snapshot(_) => return Err("snapshots are taken by the frame loop".into()),
            Command::Night(_) => return Err("night mode is the frame loop's".into()),
            Command::Timer(length) => {
                let ends = Local::now() + TimeDelta::from_std(length).map_err(|e| e.to_string())?;
                self.timer = Some((screens::timer(length, ends), ends));
                self.rang = false;
                self.flash = None;
                return Ok(ends.format("until %H:%M").to_string());
            }
            Command::TimerStop => {
                if self.timer.is_none() {
                    return Err("no timer is running".into());
                }
                self.end_timer();
            }
        }
        Ok(String::new())
    }

    fn find(&self, name: &str) -> Result<usize, String> {
        self.screens
            .iter()
            .position(|(n, _)| n == name)
            .ok_or_else(|| format!("{name:?} isn't one of the configured screens"))
    }

    /// Shows screen `i` now, for a full turn, in place of any flash.
    fn switch_to(&mut self, i: usize) {
        self.index = i;
        self.flash = None;
        self.taken_over = false;
        self.shown_at = Instant::now();
    }

    /// Puts a screen in the rotation and shows it. What it would alert
    /// about is on screen now, so that doesn't take over again.
    fn turn_on(&mut self, name: &str) -> Result<String, String> {
        let i = self.find(name)?;
        self.on[i] = true;
        let now = Instant::now();
        for alert in self.screens[i].1.alerts(Local::now()) {
            self.seen[i].insert(alert, now);
        }
        // While off, its workers were parked and what it knows is stale.
        // Waking them now is enough: the next tick sets the real activity.
        let screen = &mut self.screens[i].1;
        if !screen.present() {
            screen.activity(true, false);
            screen.refresh();
        }
        // With nothing to show yet, it waits for its turn once it has.
        if !screen.present() {
            return Ok(format!("{name} on, shown once it has something to show"));
        }
        // Not from under a timer or pushed content; it comes up after.
        if self.timing() || self.flashing() {
            self.index = i;
        } else {
            self.switch_to(i);
        }
        Ok(format!("{name} on"))
    }

    /// Takes a screen out of the rotation, moving on if it's up.
    fn turn_off(&mut self, name: &str) -> Result<String, String> {
        let i = self.find(name)?;
        if self.on[i] && self.on.iter().filter(|&&on| on).count() == 1 {
            return Err(format!(
                "{name} is the only screen on; turn another on first"
            ));
        }
        self.on[i] = false;
        self.pending.retain(|&p| p != i);
        if self.index == i {
            self.index = self.next_on(i);
            self.taken_over = false;
            self.shown_at = Instant::now();
        }
        Ok(format!("{name} off"))
    }

    /// Gives the screen on display a fresh turn, after the desktop was away
    /// (when the carousel stands still), so it isn't swapped out the moment
    /// someone's back.
    fn resume(&mut self) {
        self.shown_at = Instant::now();
    }

    /// Takes down the timer, giving the screen it covered a fresh turn.
    fn end_timer(&mut self) {
        self.timer = None;
        self.taken_over = false;
        self.shown_at = Instant::now();
    }

    fn timing(&self) -> bool {
        self.timer.is_some()
    }

    /// Whether there's a timer and its time is up.
    fn timer_up(&self) -> bool {
        self.timer
            .as_ref()
            .is_some_and(|(_, ends)| Local::now() >= *ends)
    }

    /// Takes down pushed content when the timer under it runs out, so the
    /// panel says time's up. Content pushed after that still goes over it.
    /// Returns true if it took some down.
    fn ring(&mut self) -> bool {
        if self.rang || !self.timer_up() {
            return false;
        }
        self.rang = true;
        self.flash.take().is_some()
    }

    /// Takes down a flash whose time is up. Returns true if it did.
    fn expire_flash(&mut self) -> bool {
        let expired = self.flash.as_ref().is_some_and(|f| {
            f.duration
                .zip(f.drawn_at)
                .is_some_and(|(length, at)| at.elapsed() >= length)
        });
        if expired {
            // A takeover it covered has had its turn hidden; alerts that
            // came up meanwhile go next, not after another full turn.
            self.flash = None;
            self.taken_over = false;
            self.shown_at = Instant::now();
        }
        expired
    }

    /// Observe actual USB delivery, rather than counting time spent queued
    /// in the sender. Redraws and keepalives share the same receipt.
    fn flash_drawn(&mut self) {
        if let Some(flash) = &mut self.flash
            && flash.drawn_at.is_none()
            && let Some(at) = flash.receipt.sent_at()
        {
            flash.drawn_at = Some(at);
            if let Some(reply) = flash.reply.take() {
                let _ = reply.send(Ok(String::new()));
            }
        }
    }

    fn flashing(&self) -> bool {
        self.flash.is_some()
    }

    /// Pass the carousel's lifecycle to background workers without making
    /// them infer visibility from a delayed heartbeat.
    fn activity(&mut self, active: bool) {
        let uncovered = self.flash.is_none() && self.timer.is_none();
        for (i, (_, screen)) in self.screens.iter_mut().enumerate() {
            let visible = active && uncovered && i == self.index;
            screen.activity(active && (self.on[i] || visible), visible);
        }
    }

    /// The pushed content on display, if any. Content held until `next`
    /// waits behind the away clock, like a timer that's up.
    fn overlay(&self, away: bool) -> Option<&Overlay> {
        self.flash
            .as_ref()
            .filter(|f| !(away && f.duration.is_none()))
    }

    fn current(&mut self, away: bool) -> &mut Box<dyn Screen> {
        if let Some(flash) = &mut self.flash
            && !(away && flash.duration.is_none())
        {
            return &mut flash.screen;
        }
        match &mut self.timer {
            Some((screen, _)) => screen,
            None => &mut self.screens[self.index].1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use tiny_skia::Pixmap;

    /// A screen whose alerts the test sets.
    struct Fake(Rc<RefCell<Vec<String>>>);

    impl Screen for Fake {
        fn update(&mut self, _now: chrono::DateTime<Local>) -> bool {
            false
        }
        fn draw(&self, _px: &mut Pixmap, _theme: &Theme) {}
        fn alerts(&mut self, _now: chrono::DateTime<Local>) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    /// A screen with something to show only while the test says so.
    struct Sometimes(Rc<Cell<bool>>);

    impl Screen for Sometimes {
        fn update(&mut self, _now: chrono::DateTime<Local>) -> bool {
            false
        }
        fn draw(&self, _px: &mut Pixmap, _theme: &Theme) {}
        fn present(&self) -> bool {
            self.0.get()
        }
    }

    /// `carousel(n)` with its screen 1 there only while the returned flag
    /// is set.
    fn with_sometimes(n: usize) -> (Carousel, Rc<Cell<bool>>) {
        let (mut c, _) = carousel(n);
        let present = Rc::new(Cell::new(false));
        c.screens[1].1 = Box::new(Sometimes(Rc::clone(&present)));
        (c, present)
    }

    fn night() -> Night {
        Night::new(&Config::default())
    }

    fn carousel(n: usize) -> (Carousel, Vec<Rc<RefCell<Vec<String>>>>) {
        let alerts: Vec<_> = (0..n).map(|_| Rc::default()).collect();
        let screens = alerts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                (
                    format!("s{i}"),
                    Box::new(Fake(Rc::clone(a))) as Box<dyn Screen>,
                )
            })
            .collect();
        (
            Carousel::with_screens(screens, Duration::from_secs(60)),
            alerts,
        )
    }

    /// One tick, skipping the once-a-second throttle.
    fn tick(c: &mut Carousel) -> bool {
        c.checked_at = None;
        c.advance(&mut Log::default())
    }

    #[test]
    fn parses_arguments() {
        let parse = |s: &str| {
            let args: Vec<String> = s.split_whitespace().map(String::from).collect();
            parse_args(&args)
        };
        assert_eq!(parse("").unwrap(), Args::default());
        assert!(parse("--help").unwrap().help);
        let a = parse("--preview --screen radar").unwrap();
        assert_eq!(a.preview.as_deref(), Some("preview.jpg"));
        assert_eq!(a.screen.as_deref(), Some("radar"));
        let a = parse("--screen usage --preview out.jpg").unwrap();
        assert_eq!(a.preview.as_deref(), Some("out.jpg"));
        assert!(parse("--screen").is_err());
        assert!(parse("--scren usage").is_err());
        assert!(parse("help").is_err(), "not a verb, so not run");
        assert!(parse("--screen radar --preview --night").unwrap().night);
        assert!(parse("--preview out.jpg --night").unwrap().night);
    }

    #[test]
    fn coming_back_gets_a_fresh_turn() {
        let (mut c, _) = carousel(2);
        // Away for longer than a turn.
        c.shown_at -= c.rotate * 3;
        c.resume();
        assert!(!tick(&mut c));
        assert_eq!(c.index, 0);
        assert!(c.progress().unwrap() < 0.1, "the bar starts over");
    }

    #[test]
    fn simultaneous_alerts_take_turns() {
        let (mut c, alerts) = carousel(3);
        alerts[1].replace(vec!["CPU over 85°".into()]);
        alerts[2].replace(vec!["ICE614 squawking emergency".into()]);
        assert!(tick(&mut c));
        assert_eq!(c.index, 1);
        // The second waits for the first's turn to end, then gets its own.
        assert!(!tick(&mut c));
        assert_eq!(c.index, 1);
        c.shown_at -= c.rotate;
        assert!(tick(&mut c));
        assert_eq!(c.index, 2);
    }

    #[test]
    fn a_second_condition_on_the_same_screen_takes_over() {
        let (mut c, alerts) = carousel(2);
        alerts[1].replace(vec!["CPU over 85°".into()]);
        tick(&mut c);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 0, "back to rotating");
        alerts[1].replace(vec!["CPU over 85°".into(), "GPU over 85°".into()]);
        assert!(tick(&mut c));
        assert_eq!(c.index, 1);
    }

    #[test]
    fn a_flickering_condition_takes_over_once() {
        let (mut c, alerts) = carousel(2);
        alerts[1].replace(vec!["ICE614 landing BIKF 19".into()]);
        tick(&mut c);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 0);
        alerts[1].replace(vec![]);
        tick(&mut c);
        alerts[1].replace(vec!["ICE614 landing BIKF 19".into()]);
        assert!(!tick(&mut c));
        assert_eq!(c.index, 0);
    }

    #[test]
    fn a_flash_gets_its_full_turn_once_drawn() {
        let (mut c, _) = carousel(2);
        c.command(Command::Content(content::Prepared::message(
            "Back in 5".into(),
        )))
        .unwrap();
        assert_eq!(c.progress(), None, "no countdown over a flash");
        // Not yet drawn (the panel is unplugged, say), it doesn't run out.
        assert!(!tick(&mut c));
        assert!(c.flashing());
        c.flash.as_ref().unwrap().receipt.mark_sent();
        c.flash_drawn();
        assert!(!tick(&mut c));
        // Later frames don't restart its time.
        if let Some(flash) = &mut c.flash {
            *flash.drawn_at.as_mut().unwrap() -= c.rotate;
        }
        c.flash_drawn();
        assert!(tick(&mut c));
        assert!(!c.flashing());
    }

    #[test]
    fn screens_that_are_off_are_skipped_and_dont_take_over() {
        let (mut c, alerts) = carousel(3);
        assert_eq!(c.command(Command::Off("s1".into())).unwrap(), "s1 off");
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 2, "skipped s1");
        alerts[1].replace(vec!["ICE614 squawking emergency".into()]);
        assert!(!tick(&mut c));
        assert_eq!(c.index, 2);

        // Turning it on shows it, and it's back in the rotation; what it
        // would have alerted about is already in view.
        assert_eq!(c.command(Command::Toggle("s1".into())).unwrap(), "s1 on");
        assert_eq!(c.index, 1);
        assert!(!tick(&mut c));
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 2);
    }

    #[test]
    fn a_timer_holds_the_panel_until_dismissed() {
        let (mut c, alerts) = carousel(2);
        c.command(Command::Content(content::Prepared::message(
            "Back in 5".into(),
        )))
        .unwrap();
        c.command(Command::Timer(Duration::from_secs(1500)))
            .unwrap();
        assert!(c.timing());
        assert!(!c.flashing(), "the timer was just asked for");
        assert_eq!(c.progress(), None);
        // Neither the rotation nor a takeover moves it.
        c.shown_at -= c.rotate * 3;
        alerts[1].replace(vec!["CPU over 85°".into()]);
        assert!(!tick(&mut c));
        assert!(c.timing());
        assert!(
            c.command(Command::Next).is_err(),
            "running, so `timer stop`"
        );
        assert!(c.command(Command::Show("s1".into())).is_err());

        // Once it's up, `next` dismisses it, back to the screen it covered.
        if let Some((_, ends)) = &mut c.timer {
            *ends = Local::now();
        }
        assert!(c.timer_up());
        c.command(Command::Next).unwrap();
        assert!(!c.timing());
        assert_eq!(c.index, 0);
        assert!(c.progress().unwrap() < 0.1, "with a fresh turn");
        // News from while it ran comes next.
        assert!(tick(&mut c));
        assert_eq!(c.index, 1);
    }

    #[test]
    fn a_timer_can_be_stopped() {
        let (mut c, _) = carousel(2);
        assert!(c.command(Command::TimerStop).is_err(), "none running");
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.command(Command::TimerStop).unwrap();
        assert!(!c.timing());
        // `on` puts a screen in the rotation without taking the panel.
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.command(Command::Off("s1".into())).unwrap();
        c.command(Command::On("s1".into())).unwrap();
        assert!(c.timing());
        assert_eq!(c.index, 1, "up next once the timer's done");
        c.command(Command::TimerStop).unwrap();
        // A new timer replaces a running one, and once it's up, `show`
        // dismisses it too.
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.command(Command::Timer(Duration::from_secs(120))).unwrap();
        assert!(c.command(Command::Show("s1".into())).is_err());
        if let Some((_, ends)) = &mut c.timer {
            *ends = Local::now();
        }
        c.command(Command::Show("s1".into())).unwrap();
        assert!(!c.timing());
        assert_eq!(c.index, 1);
        // A flash still goes over a timer for its turn.
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.command(Command::Content(content::Prepared::message(
            "Build finished".into(),
        )))
        .unwrap();
        assert!(c.flashing() && c.timing());
    }

    #[test]
    fn turning_off_the_current_screen_moves_on() {
        let (mut c, _) = carousel(2);
        c.command(Command::Off("s0".into())).unwrap();
        assert_eq!(c.index, 1);
        assert_eq!(c.progress(), None, "nothing else to count down to");
        assert!(c.command(Command::Off("s1".into())).is_err(), "last one on");
        // `show` still reaches it for a turn, with a countdown back.
        c.command(Command::Show("s0".into())).unwrap();
        assert_eq!(c.index, 0);
        assert!(c.progress().is_some());
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 1);
        assert!(c.command(Command::On("nope".into())).is_err());
    }

    #[test]
    fn a_screen_with_nothing_to_show_is_skipped() {
        let (mut c, present) = with_sometimes(3);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 2, "skipped s1");
        present.set(true);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 0);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 1, "in the rotation now");
        // Gone while up: it keeps its turn, then the rotation passes it by.
        present.set(false);
        assert!(!tick(&mut c));
        assert_eq!(c.index, 1);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 2);
        // `show` still brings it up, and `on` says why it may not stay.
        c.command(Command::Show("s1".into())).unwrap();
        assert_eq!(c.index, 1);
        c.command(Command::Off("s1".into())).unwrap();
        let on = c.command(Command::On("s1".into())).unwrap();
        assert!(on.contains("once it has something"), "{on}");
        assert_eq!(c.index, 2, "not shown with nothing to show");
        present.set(true);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 0);
        c.shown_at -= c.rotate;
        tick(&mut c);
        assert_eq!(c.index, 1, "back in the rotation");
    }

    #[test]
    fn with_nothing_else_to_show_the_rotation_rests() {
        let (mut c, present) = with_sometimes(2);
        assert_eq!(c.progress(), None, "s0 is the only screen with anything");
        c.shown_at -= c.rotate;
        assert!(!tick(&mut c));
        assert_eq!(c.index, 0);
        present.set(true);
        assert!(c.progress().is_some(), "counting down to s1");
        // With s0 off, an absent s1 still shows rather than nothing.
        present.set(false);
        c.command(Command::Off("s0".into())).unwrap();
        assert_eq!(c.index, 1);
    }

    #[test]
    fn workers_follow_visibility_off_away_and_explicit_show() {
        struct Activity(Rc<Cell<(bool, bool)>>);
        impl Screen for Activity {
            fn update(&mut self, _: DateTime<Local>) -> bool {
                false
            }
            fn draw(&self, _: &mut Pixmap, _: &Theme) {}
            fn activity(&mut self, active: bool, visible: bool) {
                self.0.set((active, visible));
            }
        }
        let (mut c, _) = carousel(2);
        let states = [
            Rc::new(Cell::new((false, false))),
            Rc::new(Cell::new((false, false))),
        ];
        for (i, state) in states.iter().enumerate() {
            c.screens[i].1 = Box::new(Activity(state.clone()));
        }
        c.activity(true);
        assert_eq!(states[0].get(), (true, true));
        assert_eq!(states[1].get(), (true, false));
        c.command(Command::Off("s1".into())).unwrap();
        c.activity(true);
        assert_eq!(states[1].get(), (false, false));
        c.command(Command::Show("s1".into())).unwrap();
        c.activity(true);
        assert_eq!(
            states[1].get(),
            (true, true),
            "show overrides off for its turn"
        );
        c.command(Command::Content(content::Prepared::message("hello".into())))
            .unwrap();
        c.activity(true);
        assert_eq!(states[0].get(), (true, false));
        assert_eq!(states[1].get(), (false, false));
        c.activity(false);
        assert!(
            states.iter().all(|s| s.get() == (false, false)),
            "away/offline suspends everyone"
        );
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.activity(true);
        assert_eq!(states[0].get(), (true, false));
        assert_eq!(states[1].get(), (false, false));
    }

    #[test]
    fn turning_on_a_parked_screen_refreshes_it_before_deciding() {
        /// Knows it has something to show only after a refresh.
        struct Parked(Rc<Cell<bool>>);
        impl Screen for Parked {
            fn update(&mut self, _: DateTime<Local>) -> bool {
                false
            }
            fn draw(&self, _: &mut Pixmap, _: &Theme) {}
            fn present(&self) -> bool {
                self.0.get()
            }
            fn refresh(&mut self) {
                self.0.set(true);
            }
        }
        for command in [Command::On("s1".into()), Command::Toggle("s1".into())] {
            let (mut c, _) = carousel(2);
            c.screens[1].1 = Box::new(Parked(Rc::default()));
            c.command(Command::Off("s1".into())).unwrap();
            assert_eq!(c.command(command).unwrap(), "s1 on");
            assert_eq!(c.index, 1, "shown right away");
        }
    }

    fn content(lifetime: content::Lifetime) -> Command {
        Command::Content(content::Prepared {
            visual: content::Visual::Flash {
                message: "script message".into(),
                color: None,
                icon: None,
            },
            lifetime,
            wait: false,
        })
    }

    #[test]
    fn custom_duration_starts_after_delivery_and_redraws_dont_restart_it() {
        let (mut c, _) = carousel(2);
        let length = Duration::from_secs(30);
        c.command(content(content::Lifetime::For(length))).unwrap();
        assert_eq!(c.flash.as_ref().unwrap().duration, Some(length));
        c.shown_at -= c.rotate * 5;
        c.flash_drawn();
        assert!(!tick(&mut c), "unplugged content has not started its time");
        assert!(c.flash.as_ref().unwrap().drawn_at.is_none());
        c.flash.as_ref().unwrap().receipt.mark_sent();
        c.flash_drawn();
        *c.flash.as_mut().unwrap().drawn_at.as_mut().unwrap() -= length;
        c.flash_drawn();
        assert!(tick(&mut c));
        assert!(!c.flashing());
        assert!(c.shown_at.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn held_content_can_be_dismissed_while_away_without_canceling_a_timer() {
        let (mut c, _) = carousel(2);
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        c.flash.as_ref().unwrap().receipt.mark_sent();
        c.flash_drawn();
        *c.flash.as_mut().unwrap().drawn_at.as_mut().unwrap() -= Duration::from_secs(86400);
        assert!(!tick(&mut c));
        let (reply, outcome) = std::sync::mpsc::channel();
        let request = Request {
            command: Command::Next,
            reply,
            expires: None,
            order: 0,
        };
        assert!(handle_request(&mut c, &mut night(), request, true, &[]));
        assert!(outcome.recv().unwrap().is_ok());
        assert!(!c.flashing());
        assert!(
            c.timing(),
            "next dismisses the overlay and reveals the running timer"
        );
        assert!(c.command(Command::Next).is_err());
    }

    #[test]
    fn expired_requests_dont_replace_content_and_waiters_get_dismissal_errors() {
        let (mut c, _) = carousel(2);
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        let receipt = c.flash.as_ref().unwrap().receipt.clone();
        let (reply, outcome) = std::sync::mpsc::channel();
        let request = Request {
            command: Command::Next,
            reply,
            expires: Some(Instant::now() - Duration::from_secs(1)),
            order: 0,
        };
        assert!(!handle_request(&mut c, &mut night(), request, false, &[]));
        assert!(outcome.recv().is_err());
        assert!(c.flashing());
        let (reply, outcome) = std::sync::mpsc::channel();
        c.flash.as_mut().unwrap().reply = Some(reply);
        c.command(Command::Next).unwrap();
        assert!(outcome.recv().unwrap().unwrap_err().contains("dismissed"));
        assert!(receipt.sent_at().is_none());
    }

    #[test]
    fn alerts_under_held_content_take_their_turn_once_it_is_down() {
        let (mut c, alerts) = carousel(2);
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        alerts[1].replace(vec!["CPU over 85°".into()]);
        assert!(!tick(&mut c), "held content isn't pre-empted");
        assert!(c.flashing());
        c.command(Command::Next).unwrap();
        assert!(tick(&mut c));
        assert_eq!(c.index, 1, "the alert seen meanwhile comes next");
    }

    #[test]
    fn a_timer_running_out_takes_down_held_content_once() {
        let (mut c, _) = carousel(2);
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        assert!(!c.ring(), "still running");
        if let Some((_, ends)) = &mut c.timer {
            *ends = Local::now();
        }
        assert!(c.ring());
        assert!(!c.flashing() && c.timer_up());
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        assert!(!c.ring(), "content pushed after it rang stays");
        assert!(c.flashing());
    }

    #[test]
    fn turning_a_screen_on_leaves_pushed_content_up() {
        let (mut c, _) = carousel(2);
        c.on[1] = false;
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        c.command(Command::On("s1".into())).unwrap();
        assert!(c.flashing());
        assert_eq!(c.index, 1, "it comes up after");
    }

    #[test]
    fn held_content_waits_behind_the_away_clock() {
        let (mut c, _) = carousel(2);
        c.command(content(content::Lifetime::For(Duration::from_secs(30))))
            .unwrap();
        assert!(c.overlay(true).is_some(), "a timed message still shows");
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        assert!(c.overlay(true).is_none());
        assert!(c.overlay(false).is_some(), "and is back with the desktop");
    }

    fn request(
        command: Command,
        order: u64,
    ) -> (Request, std::sync::mpsc::Receiver<Result<String, String>>) {
        let (reply, outcome) = std::sync::mpsc::channel();
        let request = Request {
            command,
            reply,
            expires: None,
            order,
        };
        (request, outcome)
    }

    #[test]
    fn content_slow_to_prepare_doesnt_replace_newer_commands() {
        let (mut c, _) = carousel(2);
        let (newer, _) = request(content(content::Lifetime::UntilNext), 2);
        assert!(handle_request(&mut c, &mut night(), newer, false, &[]));
        let (older, outcome) = request(content(content::Lifetime::UntilNext), 1);
        assert!(!handle_request(&mut c, &mut night(), older, false, &[]));
        assert!(outcome.recv().unwrap().unwrap_err().contains("newer"));
        let (dismiss, _) = request(Command::Next, 3);
        assert!(handle_request(&mut c, &mut night(), dismiss, false, &[]));
        let (stale, _) = request(content(content::Lifetime::UntilNext), 2);
        assert!(
            !handle_request(&mut c, &mut night(), stale, false, &[]),
            "dismissed stays dismissed"
        );
        assert!(!c.flashing());
        let (unrelated, _) = request(Command::Off("s1".into()), 5);
        assert!(handle_request(&mut c, &mut night(), unrelated, false, &[]));
        let (image, _) = request(content(content::Lifetime::UntilNext), 4);
        assert!(
            handle_request(&mut c, &mut night(), image, false, &[]),
            "off doesn't touch content"
        );
    }

    #[test]
    fn content_pushed_after_the_timer_ran_out_stays_up() {
        let (mut c, _) = carousel(2);
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        if let Some((_, ends)) = &mut c.timer {
            *ends = Local::now();
        }
        // As the frame loop does each tick, unplugged too: ring, then requests.
        assert!(!c.ring(), "nothing to take down");
        let (pushed, _) = request(content(content::Lifetime::UntilNext), 1);
        assert!(handle_request(&mut c, &mut night(), pushed, false, &[]));
        assert!(!c.ring());
        assert!(c.flashing());
    }

    #[test]
    fn snapshots_write_regular_files_only_and_never_block() {
        let dir = std::env::temp_dir().join(format!("cooler-lcd-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut frame = vec![0; HEADER_LEN];
        frame.extend_from_slice(b"\xff\xd8 jpeg \xff\xd9");
        let jpeg = &frame[HEADER_LEN..];

        let new = dir.join("new.jpg");
        assert!(snapshot(&frame, &new).unwrap().starts_with("wrote"));
        assert_eq!(std::fs::read(&new).unwrap(), jpeg);
        let old = dir.join("old.jpg");
        std::fs::write(&old, vec![b'x'; 4096]).unwrap();
        snapshot(&frame, &old).unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), jpeg, "overwritten whole");
        assert!(snapshot(&frame, &dir).is_err(), "a directory");
        assert!(snapshot(&frame[..HEADER_LEN], &new).is_err(), "nothing yet");

        // A pipe nobody reads, on its own thread so that a regression
        // fails the test rather than hanging it.
        let fifo = dir.join("pipe");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the path is a valid NUL-terminated pathname in our temp dir.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let (done, outcome) = std::sync::mpsc::channel();
        let (frame, path) = (frame.clone(), fifo.clone());
        std::thread::spawn(move || {
            let _ = done.send(snapshot(&frame, &path));
        });
        let outcome = outcome.recv_timeout(Duration::from_secs(5));
        if outcome.is_err() {
            // Let a writer stuck in open or write go, then fail.
            use std::io::Read;
            use std::os::unix::fs::OpenOptionsExt;
            if let Ok(mut reader) = std::fs::File::options()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                std::thread::sleep(Duration::from_millis(100));
                let _ = reader.read_to_end(&mut Vec::new());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(outcome.expect("snapshot blocked on a pipe").is_err());
    }

    #[test]
    fn show_is_refused_while_away_even_with_a_timer_up() {
        let (mut c, _) = carousel(2);
        c.command(Command::Timer(Duration::from_secs(60))).unwrap();
        if let Some((_, ends)) = &mut c.timer {
            *ends = Local::now();
        }
        let (show, outcome) = request(Command::Show("s1".into()), 1);
        assert!(!handle_request(&mut c, &mut night(), show, true, &[]));
        assert!(outcome.recv().unwrap().unwrap_err().contains("away"));
        assert!(c.timing() && c.timer_up(), "still waiting behind the clock");
        assert_eq!(c.index, 0);
        // Pushed content can still be dismissed.
        let (mut c, _) = carousel(2);
        c.command(content(content::Lifetime::UntilNext)).unwrap();
        let (next, outcome) = request(Command::Next, 1);
        assert!(handle_request(&mut c, &mut night(), next, true, &[]));
        assert!(outcome.recv().unwrap().is_ok());
        assert!(!c.flashing());
        let (next, outcome) = request(Command::Next, 2);
        assert!(!handle_request(&mut c, &mut night(), next, true, &[]));
        assert!(outcome.recv().unwrap().is_err(), "nothing left to dismiss");
    }

    #[test]
    fn an_alert_waiting_on_covered_takeover_comes_up_once_content_is_gone() {
        for expire in [false, true] {
            let (mut c, alerts) = carousel(3);
            alerts[1].replace(vec!["CPU over 85°".into()]);
            assert!(tick(&mut c));
            assert!(c.taken_over);
            c.command(content(content::Lifetime::For(Duration::from_secs(30))))
                .unwrap();
            alerts[2].replace(vec!["ICE614 squawking emergency".into()]);
            assert!(!tick(&mut c));
            assert_eq!(c.pending, [2]);
            if expire {
                c.flash.as_ref().unwrap().receipt.mark_sent();
                c.flash_drawn();
                *c.flash.as_mut().unwrap().drawn_at.as_mut().unwrap() -= c.rotate;
                assert!(tick(&mut c), "expired");
            } else {
                c.command(Command::Next).unwrap();
            }
            assert!(!c.flashing());
            assert!(tick(&mut c), "no waiting a turn behind hidden s1");
            assert_eq!(c.index, 2);
        }
    }

    /// Fake screens for the names `screens::build` knows, counting each made.
    fn builder(made: &RefCell<Vec<String>>) -> impl FnMut(&str) -> Option<Box<dyn Screen>> {
        |name| {
            screens::NAMES.contains(&name).then(|| {
                made.borrow_mut().push(name.into());
                Box::new(Fake(Rc::default())) as Box<dyn Screen>
            })
        }
    }

    fn names(c: &Carousel) -> Vec<&str> {
        c.screens.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn unusable_screen_lists_fall_back_to_the_defaults() {
        let made = RefCell::default();
        let configured = |screens: &[&str], off: &[&str]| {
            let config = Config {
                screens: screens.iter().map(|&s| s.into()).collect(),
                off: off.iter().map(|&s| s.into()).collect(),
                rotate_seconds: 42,
                ..Config::default()
            };
            Carousel::configured(None, &config, &mut Log::default(), builder(&made)).unwrap()
        };
        for (screens, off) in [
            (&[][..], &[][..]),
            (&["dashbord"], &[]),
            (&["dashbord", "usgae"], &["dashboard"]),
        ] {
            let c = configured(screens, off);
            assert_eq!(names(&c), ["dashboard"], "{screens:?}");
            assert_eq!(c.on, [true], "off can't take out the fallback");
            assert_eq!(c.rotate, Duration::from_secs(42), "other settings kept");
        }
        let c = configured(&["radar", "dashbord", "usage"], &["usage"]);
        assert_eq!(names(&c), ["radar", "usage"]);
        assert_eq!(c.on, [true, false]);

        let asked = |name: &str| {
            Carousel::configured(
                Some(name.into()),
                &Config::default(),
                &mut Log::default(),
                builder(&made),
            )
        };
        assert!(asked("bogus").is_err(), "asked for by name, so no fallback");
        assert_eq!(names(&asked("radar").unwrap()), ["radar"]);
    }

    #[test]
    fn a_screen_listed_twice_is_made_once() {
        let made = RefCell::default();
        let list = |names: &[&str]| names.iter().map(|&s| s.into()).collect::<Vec<String>>();
        let mut c = Carousel::build(
            &list(&["usage", "radar", "usage", "dashboard", "radar"]),
            &list(&["radar"]),
            Duration::from_secs(60),
            &mut Log::default(),
            builder(&made),
        )
        .unwrap();
        assert_eq!(*made.borrow(), ["usage", "radar", "dashboard"]);
        assert_eq!(names(&c), ["usage", "radar", "dashboard"]);
        assert_eq!(c.on, [true, false, true]);
        assert_eq!(
            c.command(Command::Off("usage".into())).unwrap(),
            "usage off"
        );
        assert_eq!(c.on, [false, false, true], "no copy left on");
        assert!(
            c.command(Command::Off("dashboard".into())).is_err(),
            "last on"
        );
        assert_eq!(
            c.command(Command::Toggle("usage".into())).unwrap(),
            "usage on"
        );
        c.command(Command::On("radar".into())).unwrap();
        assert_eq!(c.on, [true, true, true]);
        assert_eq!(c.index, 1);
    }
}
