//! cooler-lcd: drives the LCD on Thermalright Vision coolers with screens
//! (clock, temperatures, ...) drawn in Omarchy theme colors.
//!
//! Usage: see `USAGE`. The verbs steer the running service (see
//! `control`); `--screen` shows just that screen instead of the config's
//! list.

mod config;
mod control;
mod desktop;
mod device;
mod draw;
mod log;
mod render;
mod screens;
mod sender;
mod theme;

use std::collections::{HashMap, VecDeque};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::{DateTime, Local, TimeDelta};

use config::Config;
use control::{Command, Request};
use desktop::Desktop;
use device::HEADER_LEN;
use log::Log;
use render::Renderer;
use screens::Screen;
use sender::Sender;
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
  cooler-lcd next | show NAME | flash TEXT      steer the running service
  cooler-lcd on NAME | off NAME | toggle NAME   put a screen in or out of the rotation
  cooler-lcd snapshot FILE                      save what the panel shows as a JPEG
  cooler-lcd timer LENGTH | timer stop          count down on the panel (25m, 1h30m, 90s)";

/// The command line, past the program name, when it isn't a verb.
#[derive(Debug, Default, PartialEq)]
struct Args {
    help: bool,
    screen: Option<String>,
    /// Where to write a preview frame, if that's what's asked for.
    preview: Option<String>,
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
        let mut line = args[1..].join(" ");
        // The service doesn't share this directory, so it gets the full path.
        if verb == "snapshot" && args.len() > 2 {
            let path = std::path::absolute(args[2..].join(" "))?;
            line = format!("snapshot {}", path.display());
        }
        let message = control::send(&line)?;
        if !message.is_empty() {
            println!("{message}");
        }
        return Ok(());
    }
    let args = parse_args(&args[1..])?;
    if args.help {
        println!("{USAGE}");
        return Ok(());
    }
    let mut log = Log::default();

    let config = Config::load().unwrap_or_else(|e| {
        log.error(format!("{e:#}; using defaults"));
        Config::default()
    });
    // A screen asked for by name is shown even if the config has it off.
    let (names, off) = match args.screen {
        Some(name) => (vec![name], Vec::new()),
        None => (config.screens.clone(), config.off.clone()),
    };
    let mut carousel = Carousel::new(&names, &off, &config, &mut log)?;
    let mut theme = Theme::load(false)?;
    let mut renderer = Renderer::new();

    if let Some(path) = args.preview {
        let screen = carousel.current();
        screen.update(Local::now());
        // Give background fetchers (network data) a moment to deliver.
        for _ in 0..50 {
            if !screen.loading() {
                break;
            }
            sleep(Duration::from_millis(100));
            screen.update(Local::now());
        }
        renderer.render(screen.as_ref(), &theme)?;
        std::fs::write(&path, &renderer.frame()[HEADER_LEN..])?;
        println!("wrote {path}");
        return Ok(());
    }

    // Before the sender, so a second instance exits instead of lying in
    // wait for the panel. Without a socket the service still runs.
    let requests = match control::lock() {
        Err(e) if e.is::<control::AlreadyRunning>() => return Err(e),
        lock => lock.and_then(control::listen),
    }
    .inspect_err(|e| log.error(format!("control socket: {e:#}")))
    .ok();
    let sender = Sender::spawn();
    let desktop = Desktop::spawn();
    let mut away_screen = screens::away();
    let mut was_away = false;
    let mut showed_away = false;
    let mut theme_stamp = theme::stamp();
    let mut theme_checked_at = Instant::now();
    let mut dirty = true;
    // When the screen on display is next due an update.
    let mut next_update = Instant::now();
    // The rotation bar's length on the panel, in pixels.
    let mut shown_bar = None;
    loop {
        let tick = Instant::now();

        for Request {
            command,
            reply,
            expires,
        } in requests.iter().flat_map(|r| r.try_iter())
        {
            // The client already gave up waiting and was told so.
            if Instant::now() >= expires {
                continue;
            }
            let outcome = match command {
                Command::Snapshot(path) => snapshot(renderer.frame(), &path),
                // Dismissing a timer that's up works even while away.
                Command::Next if carousel.timer_up() => carousel.command(Command::Next),
                // While a timer runs, the carousel says how to stop it.
                Command::Next | Command::Show(_) if desktop.away() && !carousel.timing() => {
                    Err("the desktop is away, so the panel shows the clock".into())
                }
                command => carousel.command(command),
            };
            dirty |= outcome.is_ok();
            let _ = reply.send(outcome);
        }

        // Nothing to draw for until the sender has a panel; redraw once it does.
        if !sender.connected() {
            dirty = true;
            sleep(OFFLINE_TICK);
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
                    theme = t;
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
        // behind it, so it doesn't glow all night.
        let show_away =
            away && !carousel.flashing() && !(carousel.timing() && !carousel.timer_up());
        if show_away != showed_away {
            showed_away = show_away;
            dirty = true;
        }
        let rotation = carousel.progress().filter(|_| config.rotate_bar);
        let screen = if show_away {
            &mut away_screen
        } else {
            carousel.current()
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
                    sender.publish(renderer.frame());
                    carousel.flash_drawn();
                }
                Err(e) => log.error(format!("rendering: {e:#}")),
            }
        }

        sleep(wake.saturating_duration_since(Instant::now()));
    }
}

/// Saves `frame`, the one on the panel, to `path` as a JPEG.
fn snapshot(frame: &[u8], path: &std::path::Path) -> Result<String, String> {
    let jpeg = frame.get(HEADER_LEN..).filter(|j| !j.is_empty());
    let jpeg = jpeg.ok_or("nothing on the panel yet")?;
    std::fs::write(path, jpeg).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!("wrote {}", path.display()))
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
    flash: Option<(Box<dyn Screen>, Option<Instant>)>,
    /// A countdown from `cooler-lcd timer` and when it's up.
    timer: Option<(Box<dyn Screen>, DateTime<Local>)>,
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

impl Carousel {
    fn new(names: &[String], off: &[String], config: &Config, log: &mut Log) -> Result<Self> {
        for name in off.iter().filter(|n| !names.contains(n)) {
            log.error(format!("off: {name:?} isn't one of the configured screens"));
        }
        let mut screens = Vec::new();
        for name in names {
            match screens::build(name, config) {
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
        let mut carousel = Self::with_screens(screens, config.rotate());
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
            Command::Flash(text) => self.flash = Some((screens::flash(text), None)),
            Command::Snapshot(_) => return Err("snapshots are taken by the frame loop".into()),
            Command::Timer(length) => {
                let ends = Local::now() + TimeDelta::from_std(length).map_err(|e| e.to_string())?;
                self.timer = Some((screens::timer(length, ends), ends));
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
        // With nothing to show yet, it waits for its turn once it has.
        if !self.screens[i].1.present() {
            return Ok(format!("{name} on, shown once it has something to show"));
        }
        // Not from under a timer; it comes up after.
        if self.timing() {
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

    /// Takes down a flash whose time is up. Returns true if it did.
    fn expire_flash(&mut self) -> bool {
        if self
            .flash
            .as_ref()
            .is_none_or(|(_, drawn_at)| drawn_at.is_none_or(|t| t.elapsed() < self.rotate))
        {
            return false;
        }
        self.flash = None;
        self.shown_at = Instant::now();
        true
    }

    /// Notes that a frame of the flash went to the panel, starting its time
    /// if this was the first.
    fn flash_drawn(&mut self) {
        if let Some((_, drawn_at @ None)) = &mut self.flash {
            *drawn_at = Some(Instant::now());
        }
    }

    fn flashing(&self) -> bool {
        self.flash.is_some()
    }

    fn current(&mut self) -> &mut Box<dyn Screen> {
        if let Some((screen, _)) = &mut self.flash {
            return screen;
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
        c.command(Command::Flash("Claude needs you".into()))
            .unwrap();
        assert_eq!(c.progress(), None, "no countdown over a flash");
        // Not yet drawn (the panel is unplugged, say), it doesn't run out.
        assert!(!tick(&mut c));
        assert!(c.flashing());
        c.flash_drawn();
        assert!(!tick(&mut c));
        // Later frames don't restart its time.
        if let Some((_, Some(drawn_at))) = &mut c.flash {
            *drawn_at -= c.rotate;
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
        c.command(Command::Flash("Claude needs you".into()))
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
        c.command(Command::Flash("Build finished".into())).unwrap();
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
}
