#!/bin/sh
# Uninstall the howan systemd --user service and binary.
#
# Idempotent: each step swallows the "not installed" case so running this
# on a system where howan was never installed (or was already removed) is
# not an error.
set -e

systemctl --user disable --now howan.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/howan.service"
systemctl --user daemon-reload
cargo uninstall howan 2>/dev/null || true
