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
mod render;
mod screens;
mod theme;

use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};
use chrono::Local;

use config::Config;
use device::{HEADER_LEN, Panel};
use render::Renderer;
use screens::Screen;
use theme::Theme;

/// The panel shows its own logo after ~2-3 s without a frame, so resend often.
const INTERVAL: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(3);
/// Wall clock running this far ahead of the monotonic clock means we slept;
/// the USB handle may be stale, so reconnect.
const WAKE_GAP: Duration = Duration::from_secs(5);

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
        renderer.render(screen.as_ref(), &theme)?;
        std::fs::write(path, &renderer.frame()[HEADER_LEN..])?;
        println!("wrote {path}");
        return Ok(());
    }

    let mut theme_mtime = theme::colors_mtime();
    loop {
        let mut panel = match Panel::open() {
            Ok(p) => p,
            Err(e) => {
                log.error(format!("{e:#}"));
                sleep(RETRY);
                continue;
            }
        };
        log.info(format!("connected to screen (PM={})", panel.pm));

        let mut clocks = (SystemTime::now(), Instant::now());
        let mut dirty = true;
        loop {
            let tick = Instant::now();

            let now = (SystemTime::now(), tick);
            let wall = now.0.duration_since(clocks.0).unwrap_or_default();
            if wall.saturating_sub(now.1 - clocks.1) > WAKE_GAP {
                log.info("woke from sleep; reconnecting".into());
                break;
            }
            clocks = now;

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

            dirty |= carousel.advance();
            let screen = carousel.current();
            dirty |= screen.update(Local::now());

            // On a render error, keep showing the previous frame and retry next tick.
            if dirty {
                match renderer.render(screen.as_ref(), &theme) {
                    Ok(()) => dirty = false,
                    Err(e) => log.error(format!("rendering: {e:#}")),
                }
            }
            if !renderer.frame().is_empty()
                && let Err(e) = panel.send(renderer.frame())
            {
                log.error(format!("{e:#}; reconnecting"));
                break;
            }

            sleep(INTERVAL.saturating_sub(tick.elapsed()));
        }
    }
}

/// The configured screens, shown one at a time for `rotate` each.
struct Carousel {
    screens: Vec<Box<dyn Screen>>,
    index: usize,
    shown_at: Instant,
    rotate: Duration,
}

impl Carousel {
    fn new(names: &[String], rotate: Duration, log: &mut Log) -> Result<Self> {
        let mut screens = Vec::new();
        for name in names {
            match screens::build(name) {
                Some(s) => screens.push(s),
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
            screens,
            index: 0,
            shown_at: Instant::now(),
            rotate,
        })
    }

    /// Moves to the next screen when its time is up. Returns true on a switch.
    fn advance(&mut self) -> bool {
        if self.screens.len() < 2 || self.shown_at.elapsed() < self.rotate {
            return false;
        }
        self.index = (self.index + 1) % self.screens.len();
        self.shown_at = Instant::now();
        true
    }

    fn current(&mut self) -> &mut Box<dyn Screen> {
        &mut self.screens[self.index]
    }
}

/// Prints each message once, so a condition that persists (cooler unplugged)
/// doesn't flood the journal.
#[derive(Default)]
struct Log {
    last: String,
}

impl Log {
    fn error(&mut self, msg: String) {
        if msg != self.last {
            eprintln!("{msg}");
            self.last = msg;
        }
    }

    fn info(&mut self, msg: String) {
        eprintln!("{msg}");
        self.last = msg;
    }
}
