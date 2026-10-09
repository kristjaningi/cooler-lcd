# cooler-lcd

A tiny driver for the LCD on Thermalright Vision coolers (USB `87ad:70db`,
e.g. Phantom Spirit 120 Vision). It cycles through screens drawn in the colors
and font of the active Omarchy theme, and follows theme switches live. The
screens are:

- `dashboard`: the clock, date, and CPU and GPU temperature.
- `usage`: Claude and Codex plan usage as ring gauges, with time until each
  limit resets.

A lightweight replacement for TRCC: ~0.1% CPU and ~33 MB of memory (most of
that is NVIDIA's NVML library, loaded for the GPU temperature).

## How it works

- **Screen:** a 480x480 panel that only accepts whole pictures. Each frame is a
  64-byte header + JPEG sent over USB bulk (`src/device.rs`). The firmware
  falls back to the Thermalright logo after ~2-3 s without a frame, so the last
  frame is resent every second.
- **Screens:** each screen in `src/screens/` gathers its own data and draws
  itself with the helpers in `src/draw.rs`. A frame is redrawn and re-encoded
  only when the screen reports a change, the theme changes, or the next screen
  rotates in (`src/render.rs`).
- **Sensors:** CPU from hwmon (`k10temp` Tdie/Tctl, `coretemp`), GPU from
  NVML, polled every 3 s. Missing sensors are retried every 30 s
  (`src/screens/sensors.rs`).
- **Theme:** colors from `~/.local/state/omarchy/current/theme/colors.toml`,
  font from `omarchy-font-current` via fontconfig, falling back to the system
  sans (`src/theme.rs`).
- **Claude usage:** fetched every 5 minutes on a background thread from the
  OAuth usage endpoint that Claude Code's `/usage` uses, with the token Claude
  Code keeps in `~/.claude/.credentials.json` (sent only to
  api.anthropic.com). The endpoint is undocumented, so failures are soft:
  errors and HTTP 429 back off up to an hour, an expired or rejected token is
  never retried until Claude Code refreshes it, and old numbers are dimmed
  with their age (`src/screens/usage/claude.rs`).
- **Codex usage:** read from the newest local session log in
  `~/.codex/sessions/`, which records the account's rate limits on every
  reply. No network; updates after you use Codex (`src/screens/usage/codex.rs`).
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
screens = ["dashboard", "usage"]   # shown in order
rotate_seconds = 15       # time per screen when more than one is listed
```

Restart the service after editing it. A broken config is logged and the
defaults are used.

## Adding a screen

Implement the `Screen` trait in a new module under `src/screens/` and register
it in `build()` and `NAMES` in `src/screens/mod.rs`. `update()` runs inside the
frame loop, so fetch anything slow (network, big files) on a background thread.

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
