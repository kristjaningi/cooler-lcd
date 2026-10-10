//! cooler-lcd: drives the LCD on Thermalright Vision coolers with screens
//! (clock, temperatures, ...) drawn in Omarchy theme colors.
//!
//! Usage:
//!   cooler-lcd [--screen NAME]                  run, cycling through the configured screens
//!   cooler-lcd [--screen NAME] --preview FILE   render one frame to a JPEG file and exit
//!
//! `--screen` shows just that screen instead of the config's list.

mod config;
mod device;
mod draw;
mod log;
mod render;
mod screens;
mod sender;
mod theme;

use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Local;

use config::Config;
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
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
    let mut carousel = Carousel::new(&names, config.rotate(), &mut log)?;
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

    let sender = Sender::spawn();
    let mut theme_mtime = theme::colors_mtime();
    let mut dirty = true;
    loop {
        let tick = Instant::now();

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

        dirty |= carousel.advance(&mut log);
        let screen = carousel.current();
        dirty |= screen.update(Local::now());

        // On a render error the sender keeps showing the previous frame;
        // retry next tick.
        if dirty {
            match renderer.render(screen.as_ref(), &theme) {
                Ok(()) => {
                    dirty = false;
                    sender.publish(renderer.frame());
                }
                Err(e) => log.error(format!("rendering: {e:#}")),
            }
        }

        sleep(screen.interval().saturating_sub(tick.elapsed()));
    }
}

/// The configured screens, shown one at a time for `rotate` each. A screen
/// that raises a new alert jumps the queue and stays up for a full `rotate`.
struct Carousel {
    screens: Vec<(String, Box<dyn Screen>)>,
    /// Each screen's alert as of the last check, to spot new ones.
    alerts: Vec<Option<String>>,
    checked_at: Option<Instant>,
    index: usize,
    shown_at: Instant,
    rotate: Duration,
}

impl Carousel {
    fn new(names: &[String], rotate: Duration, log: &mut Log) -> Result<Self> {
        let mut screens = Vec::new();
        for name in names {
            match screens::build(name) {
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
        Ok(Self {
            alerts: vec![None; screens.len()],
            screens,
            checked_at: None,
            index: 0,
            shown_at: Instant::now(),
            rotate,
        })
    }

    /// Moves to a screen with a new alert, else to the next screen when the
    /// current one's time is up. Returns true on a switch.
    fn advance(&mut self, log: &mut Log) -> bool {
        if self.screens.len() < 2 {
            return false;
        }
        if let Some(i) = self.new_alert(log) {
            self.shown_at = Instant::now();
            let switched = i != self.index;
            self.index = i;
            return switched;
        }
        if self.shown_at.elapsed() < self.rotate {
            return false;
        }
        self.index = (self.index + 1) % self.screens.len();
        self.shown_at = Instant::now();
        true
    }

    /// Asks every screen for its alert, at most once per `ALERT_CHECK`, and
    /// returns the first screen whose alert is new.
    fn new_alert(&mut self, log: &mut Log) -> Option<usize> {
        if self.checked_at.is_some_and(|t| t.elapsed() < ALERT_CHECK) {
            return None;
        }
        self.checked_at = Some(Instant::now());
        let now = Local::now();
        let mut first = None;
        for (i, (name, screen)) in self.screens.iter_mut().enumerate() {
            let alert = screen.alert(now);
            if let Some(msg) = &alert
                && self.alerts[i].as_ref() != Some(msg)
            {
                log.info(format!("{name}: {msg}"));
                first.get_or_insert(i);
            }
            self.alerts[i] = alert;
        }
        first
    }

    fn current(&mut self) -> &mut Box<dyn Screen> {
        &mut self.screens[self.index].1
    }
}
