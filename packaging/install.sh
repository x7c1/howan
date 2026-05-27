#!/bin/sh
# Install howan as a systemd --user service.
#
# Steps:
#   1. cargo install the daemon binary into ~/.cargo/bin
#   2. Place the unit file at ~/.config/systemd/user/howan.service
#   3. Reload the user systemd manager so it picks up the new unit
#   4. Enable the service so it auto-starts at graphical-session login
#   5. Restart the service so a re-install always picks up the new binary
#      (using restart rather than `enable --now` so an existing process is
#      replaced rather than left running with the old binary).
set -e

cd "$(dirname "$0")/.."

cargo install --path crates/howan
install -Dm644 packaging/systemd/howan.service \
    "$HOME/.config/systemd/user/howan.service"
systemctl --user daemon-reload
systemctl --user enable howan.service
systemctl --user restart howan.service
