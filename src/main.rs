//! cooler-lcd: drives the LCD on Thermalright Vision coolers with a small
//! dashboard (clock, date, CPU and GPU temperature) in Omarchy theme colors.
//!
//! Usage:
//!   cooler-lcd                  run, updating the screen every second
//!   cooler-lcd --preview FILE   render one frame to a JPEG file and exit

mod device;
mod render;
mod stats;
mod theme;

use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use chrono::Local;

use device::{HEADER_LEN, Panel};
use render::{Renderer, Snapshot};
use stats::Sensors;
use theme::Theme;

/// The panel shows its own logo after ~2-3 s without a frame, so resend often.
const INTERVAL: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(3);
/// Wall clock running this far ahead of the monotonic clock means we slept;
/// the USB handle may be stale, so reconnect.
const WAKE_GAP: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut sensors = Sensors::new();
    let mut theme = Theme::load(false)?;
    let mut renderer = Renderer::new(&theme);

    if let Some(i) = args.iter().position(|a| a == "--preview") {
        let path = args.get(i + 1).map_or("preview.jpg", String::as_str);
        renderer.update(&theme, &snapshot(&mut sensors))?;
        std::fs::write(path, &renderer.frame()[HEADER_LEN..])?;
        println!("wrote {path}");
        return Ok(());
    }

    let mut log = Log::default();
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
                        renderer.set_theme(&theme);
                        theme_mtime = mtime;
                    }
                    Err(e) => log.error(format!("reloading theme: {e:#}")),
                }
            }

            // On a render error, keep showing the previous frame.
            if let Err(e) = renderer.update(&theme, &snapshot(&mut sensors)) {
                log.error(format!("rendering: {e:#}"));
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

fn snapshot(sensors: &mut Sensors) -> Snapshot {
    let (cpu_temp, gpu_temp) = sensors.temps();
    Snapshot {
        now: Local::now(),
        cpu_temp,
        gpu_temp,
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
