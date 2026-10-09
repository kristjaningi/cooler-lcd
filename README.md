# cooler-lcd

A tiny dashboard for the LCD on Thermalright Vision coolers (USB `87ad:70db`,
e.g. Phantom Spirit 120 Vision). Shows the clock, date, CPU and GPU temperature
in the colors of the active Omarchy theme, and follows theme switches live.

## Run

```sh
cargo run --release                          # drive the screen
cargo run --release -- --preview out.jpg     # render one frame to a file
```

Only one program can use the screen at a time, so stop TRCC first (`trcc kill`).

## Install

```sh
cargo install --path .
sudo cp dist/70-cooler-lcd.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger
cp dist/cooler-lcd.service ~/.config/systemd/user/
systemctl --user enable --now cooler-lcd
```

Logs: `journalctl --user -u cooler-lcd`.
