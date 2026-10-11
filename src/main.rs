//! cooler-lcd: drives the LCD on Thermalright Vision coolers with screens
//! (clock, temperatures, ...) drawn in Omarchy theme colors.
//!
//! Usage:
//!   cooler-lcd [--screen NAME]                  run, cycling through the configured screens
//!   cooler-lcd [--screen NAME] --preview FILE   render one frame to a JPEG file and exit
//!   cooler-lcd next | show NAME | flash TEXT    steer the running service (see `control`)
//!
//! `--screen` shows just that screen instead of the config's list.

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
use chrono::Local;

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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(verb) = args.get(1)
        && control::VERBS.contains(&verb.as_str())
    {
        return control::send(&args[1..].join(" "));
    }
    let flag = |name: &str| args.iter().position(|a| a == name).map(|i| args.get(i + 1));
    let mut log = Log::default();

    let config = Config::load().unwrap_or_else(|e| {
        log.error(format!("{e:#}; using defaults"));
        Config::default()
    });
    let names = match flag("--screen") {
        Some(Some(name)) => vec![name.clone()],
        Some(None) => bail!("--screen needs a name: {}", screens::NAMES.join(", ")),
        None => config.screens.clone(),
    };
    let mut carousel = Carousel::new(&names, &config, &mut log)?;
    let mut theme = Theme::load(false)?;
    let mut renderer = Renderer::new();

    if let Some(path) = flag("--preview") {
        let path = path.map_or("preview.jpg", String::as_str);
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
        std::fs::write(path, &renderer.frame()[HEADER_LEN..])?;
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
    let mut theme_mtime = theme::colors_mtime();
    let mut dirty = true;
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
                Command::Next | Command::Show(_) if desktop.away() => {
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

        // Follow Omarchy theme switches. The theme directory is briefly
        // missing while Omarchy swaps it in, so ignore that state.
        let mtime = theme::colors_mtime();
        if mtime.is_some() && mtime != theme_mtime {
            match Theme::load(true) {
                Ok(t) => {
                    theme = t;
                    theme_mtime = mtime;
                    dirty = true;
                }
                Err(e) => log.error(format!("reloading theme: {e:#}")),
            }
        }

        // While the desktop is locked or dark, a dim clock stands in for the
        // carousel, whose screens then stop updating (and fetching).
        let away = desktop.away();
        if away != was_away {
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
        let show_away = away && !carousel.flashing();
        if show_away != showed_away {
            showed_away = show_away;
            dirty = true;
        }
        let screen = if show_away {
            &mut away_screen
        } else {
            carousel.current()
        };
        dirty |= screen.update(Local::now());
        let interval = screen.interval();

        // On a render error the sender keeps showing the previous frame;
        // retry next tick.
        if dirty {
            match renderer.render(screen.as_ref(), &theme) {
                Ok(()) => {
                    dirty = false;
                    sender.publish(renderer.frame());
                    carousel.flash_drawn();
                }
                Err(e) => log.error(format!("rendering: {e:#}")),
            }
        }

        sleep(interval.saturating_sub(tick.elapsed()));
    }
}

/// The configured screens, shown one at a time for `rotate` each. A screen
/// with a new alert jumps the queue and stays up for a full `rotate` (several
/// take their turns), and a flashed message goes in front of them all for as
/// long.
struct Carousel {
    /// A message from `cooler-lcd flash` and when it was first drawn. Its
    /// time runs from then, so one sent while the panel is unplugged still
    /// gets its full turn once it's back.
    flash: Option<(Box<dyn Screen>, Option<Instant>)>,
    screens: Vec<(String, Box<dyn Screen>)>,
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
    fn new(names: &[String], config: &Config, log: &mut Log) -> Result<Self> {
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
        Ok(Self::with_screens(screens, config.rotate()))
    }

    fn with_screens(screens: Vec<(String, Box<dyn Screen>)>, rotate: Duration) -> Self {
        Self {
            flash: None,
            seen: vec![HashMap::new(); screens.len()],
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
        if self.screens.len() < 2 {
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
        self.index = (self.index + 1) % self.screens.len();
        self.shown_at = Instant::now();
        true
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

    /// Carries out a command from the control socket.
    fn command(&mut self, command: Command) -> Result<(), String> {
        match command {
            Command::Next => self.index = (self.index + 1) % self.screens.len(),
            Command::Show(name) => {
                self.index = self
                    .screens
                    .iter()
                    .position(|(n, _)| *n == name)
                    .ok_or_else(|| format!("{name:?} isn't one of the configured screens"))?;
            }
            Command::Flash(text) => {
                self.flash = Some((screens::flash(text), None));
                return Ok(());
            }
        }
        self.flash = None;
        self.taken_over = false;
        self.shown_at = Instant::now();
        Ok(())
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
        match &mut self.flash {
            Some((screen, _)) => screen,
            None => &mut self.screens[self.index].1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
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
}
