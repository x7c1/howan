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
#   6. Check the GNOME compositor's idle-delay against the daemon's T1 and
#      warn if the two would race (see check_gnome_compat below).
set -e

cd "$(dirname "$0")/.."

UNIT_DST="$HOME/.config/systemd/user/howan.service"

cargo install --path crates/howan
install -Dm644 packaging/systemd/howan.service "$UNIT_DST"
systemctl --user daemon-reload
systemctl --user enable howan.service
systemctl --user restart howan.service

# Extract the effective T1 (idle-timeout, seconds) from the installed unit's
# ExecStart line. If `--idle-timeout <N>` is absent or malformed, fall back
# to the daemon's built-in default of 300 seconds (see crates/howan).
extract_t1_from_unit() {
    unit_file=$1
    # Pull the first ExecStart line and the token following --idle-timeout.
    # `awk` is portable; we deliberately avoid bash-only string ops.
    value=$(awk '
        /^ExecStart=/ {
            for (i = 1; i <= NF; i++) {
                if ($i == "--idle-timeout" && (i + 1) <= NF) {
                    print $(i + 1)
                    exit
                }
            }
        }
    ' "$unit_file" 2>/dev/null || true)
    case "$value" in
        ''|*[!0-9]*) printf '%s\n' 300 ;;
        *)           printf '%s\n' "$value" ;;
    esac
}

# Extract the trailing integer from `gsettings get` output of the form
# `uint32 300`. Echoes nothing if the input has no integer tail.
extract_gsettings_uint() {
    raw=$1
    # Strip everything up to the last whitespace, then validate it is digits.
    tail=$(printf '%s' "$raw" | awk '{print $NF}')
    case "$tail" in
        ''|*[!0-9]*) ;;
        *)           printf '%s\n' "$tail" ;;
    esac
}

check_gnome_compat() {
    if ! command -v gsettings >/dev/null 2>&1; then
        printf '[howan] GNOME compatibility check skipped (gsettings not found)\n'
        return 0
    fi

    t1=$(extract_t1_from_unit "$UNIT_DST")

    if ! raw=$(gsettings get org.gnome.desktop.session idle-delay 2>/dev/null); then
        printf '[howan] GNOME compatibility check skipped (gsettings get idle-delay failed)\n'
        return 0
    fi

    idle_delay=$(extract_gsettings_uint "$raw")
    if [ -z "$idle_delay" ]; then
        printf '[howan] GNOME compatibility check skipped (unparseable idle-delay: %s)\n' "$raw"
        return 0
    fi

    recommended=$((t1 + 60))

    if [ "$idle_delay" -eq 0 ]; then
        printf '[howan] WARNING: org.gnome.desktop.session idle-delay is 0 (compositor idle timer disabled).\n' >&2
        printf '[howan] WARNING:   Phase 3 (DPMS handoff) will NOT blank the screen even after the saver releases\n' >&2
        printf '[howan] WARNING:   its idle inhibitor at T_dpms, because the compositor has no idle timer to take over.\n' >&2
        printf '[howan] WARNING:   Recommend: gsettings set org.gnome.desktop.session idle-delay '"'"'uint32 %d'"'"'\n' "$recommended" >&2
        printf '[howan] WARNING:   See docs/guides/40-resident-daemon.md for the idle-delay > T1 requirement.\n' >&2
        return 0
    fi

    if [ "$idle_delay" -le "$t1" ]; then
        printf '[howan] WARNING: org.gnome.desktop.session idle-delay (%ds) <= howan T1 (%ds).\n' "$idle_delay" "$t1" >&2
        printf '[howan] WARNING:   Mutter and howan share the same idle threshold, so the screen may DPMS-off at\n' >&2
        printf '[howan] WARNING:   the moment the saver is supposed to appear. idle-inhibit only prevents future\n' >&2
        printf '[howan] WARNING:   blanks, not one already in flight, and Mutter wins the race in practice.\n' >&2
        printf '[howan] WARNING:   Recommend: gsettings set org.gnome.desktop.session idle-delay '"'"'uint32 %d'"'"'\n' "$recommended" >&2
        printf '[howan] WARNING:   See docs/guides/40-resident-daemon.md for the idle-delay > T1 requirement.\n' >&2
        return 0
    fi

    printf '[howan] GNOME compatibility check: idle-delay=%ds, T1=%ds, ok\n' "$idle_delay" "$t1"
}

check_gnome_compat
