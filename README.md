# cooler-lcd

A tiny driver for the LCD on Thermalright Vision coolers (USB `87ad:70db`,
e.g. Phantom Spirit 120 Vision). It cycles through screens drawn in the colors
and font of the active Omarchy theme, and follows theme switches live. The
screens are:

- `dashboard`: the clock, date, and CPU and GPU temperature.
- `usage`: Claude Code and Codex plan usage as horizontal bars, with a pace
  marker and the time until each limit resets.
- `radar`: a surveillance radar scope over Reykjavik and Keflavik with live
  air traffic: a rotating sweep, range rings, the coastline and runways, and
  each aircraft with its history trail, velocity vector and data block
  (callsign, altitude in hundreds of feet, climb/descent, ground speed).

A lightweight replacement for TRCC: ~0.1% CPU for the still screens, ~6% of
one core while the animated radar is up, and ~35 MB of memory (most of that
is NVIDIA's NVML library, loaded for the GPU temperature).

## How it works

- **Screen:** a 480x480 panel that only accepts whole pictures. Each frame is a
  64-byte header + JPEG sent over USB bulk (`src/device.rs`). The firmware
  falls back to the Thermalright logo after ~2-3 s without a frame, so the last
  frame is resent every second.
- **Screens:** each screen in `src/screens/` gathers its own data and draws
  itself with the helpers in `src/draw.rs`. A frame is redrawn and re-encoded
  only when the screen reports a change, the theme changes, or the next screen
  rotates in (`src/render.rs`). Still screens run at one frame a second;
  animated ones set their own `interval()`.
- **Takeovers:** every screen is asked about once a second whether something
  needs attention, even while it's hidden. When one starts, its screen jumps
  the queue and stays up for a full rotation: the dashboard when the CPU or
  GPU reaches 85° (it must cool below 75° to trigger again), usage when a
  plan limit passes 90%, and the radar on an emergency squawk or an
  Icelandair flight landing. A condition that lasts doesn't pin its screen;
  only a new one takes over again. The radar only sees traffic while it has
  been on screen recently, since it doesn't fetch just to check.
- **Sensors:** CPU from hwmon (`k10temp` Tdie/Tctl, `coretemp`), GPU from
  NVML, polled every 3 s. Missing sensors are retried every 30 s
  (`src/screens/sensors.rs`).
- **Theme:** colors from `~/.local/state/omarchy/current/theme/colors.toml`,
  font from `omarchy-font-current` via fontconfig, falling back to the system
  sans (`src/theme.rs`).
- **Plan usage:** read from the records Omarchy's Agents bar widget
  (`omarchy.agents`) writes to `~/.local/state/omarchy/agents/usage/`, one
  JSON file per agent, re-read whenever one changes. No network and no
  credentials of its own; the numbers are as fresh as the bar's (every 15
  minutes by default, `omarchy bar set omarchy.agents refreshIntervalSec 300
  --json` for 5). Each bar's notch marks where an even spend across the
  window would be by now, and records older than 45 minutes are dimmed with
  their age (`src/screens/usage.rs`).
- **Radar:** aircraft from [adsb.lol](https://adsb.lol)'s free API (community
  ADS-B receivers, no key), polled every 10 s only while the radar is on
  screen, backing off on errors and HTTP 429. Positions are dead reckoned
  between polls, so blips glide at the 15 fps animation rate. The static
  scope, rings and map are drawn once per theme. Coastline from Natural
  Earth, runways from OurAirports (`src/screens/radar/`).
- **Recovery:** reconnects when the cooler is unplugged or a write fails, and
  after suspend (detected as the wall clock jumping ahead of the monotonic
  clock).

## Run

```sh
cargo run --release                                        # drive the screen
cargo run --release -- --preview out.jpg                   # render one frame to a file
cargo run --release -- --screen dashboard --preview out.jpg  # preview a specific screen
cargo test                                                 # unit tests
```

Only one program can use the screen at a time, so stop the service (or TRCC,
with `trcc kill`) first.

## Config

Optional, at `~/.config/cooler-lcd/config.toml` (see `dist/config.toml`):

```toml
screens = ["dashboard", "usage", "radar"]   # shown in order
rotate_seconds = 15       # time per screen when more than one is listed
```

Restart the service after editing it. A broken config is logged and the
defaults are used.

## Adding a screen

Implement the `Screen` trait in a new module under `src/screens/` and register
it in `build()` and `NAMES` in `src/screens/mod.rs`. `update()` runs inside the
frame loop, so fetch anything slow (network, big files) on a background thread.
Implement `alert()` to take over the panel when something happens.

## Install

```sh
cargo install --path .
sudo cp dist/70-cooler-lcd.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger
cp dist/cooler-lcd.service ~/.config/systemd/user/
systemctl --user enable --now cooler-lcd
```

The udev rule gives the logged-in user access to the screen and keeps USB
autosuspend off for it.

If TRCC is installed, keep it from grabbing the screen on login: remove any
`trcc` line from `~/.config/hypr/autostart.lua`, and add `Hidden=true` to
`~/.config/autostart/trcc.desktop`.

## Day to day

```sh
journalctl --user -u cooler-lcd                              # logs
cargo install --path . && systemctl --user restart cooler-lcd # deploy changes
systemctl --user stop cooler-lcd                             # hand the screen back to TRCC
```

## License

MIT, see [LICENSE](LICENSE).
