---
status: completed
pipeline_phase: null
plan: null
base_ref: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings && grep -q 'ExecStart=' packaging/systemd/howan.service && grep -q 'graphical-session.target' packaging/systemd/howan.service && grep -qE '^install:' Makefile && grep -qE '^uninstall:' Makefile && grep -q '^#!/bin/sh' packaging/install.sh && grep -q '^#!/bin/sh' packaging/uninstall.sh && grep -q 'make install' README.md"
assignee: null
branch: task/0527-0527-m10-systemd-user-unit
created_at: 2026-05-27T05:27:28Z
updated_at: 2026-05-27T10:09:32Z
---

# feat(packaging): systemd `--user` unit for howan daemon (cargo install-based)

## Overview

M4 completed the daemon's functional surface — idle detection, idle inhibit,
the 3-phase lifecycle with Q4 re-arm gating and Phase 3 surface retention.
Today the daemon still has to be launched manually from a terminal
(`./target/release/howan daemon`), runs in the foreground, does not survive
a reboot, and has no log management.

M10 turns the daemon into a **systemd `--user` service** so the OS owns its
lifecycle: auto-start at graphical-session login, restart on failure, logs
routed through the journal, standard `systemctl --user` controls. This
makes the M5+ implementation loop (multi-monitor, shader player, etc.)
significantly faster to iterate on real hardware.

**Distribution scope is intentionally minimal at this stage.** The author
is the only user. The install path is `cargo install`-based, exposed
through a top-level `Makefile` that acts purely as a task-runner facade
delegating to shell scripts in `packaging/`:

```
make install     # → ./packaging/install.sh
make uninstall   # → ./packaging/uninstall.sh
```

The actual logic (`cargo install`, file placement, `systemctl` calls)
lives in the shell scripts where it is natural to write conditionals,
error handling, and the `$HOME` expansion. The Makefile stays a thin
two-line dispatcher — long recipes inside Makefiles tend to become
unreadable.

Distro packages (deb / rpm / AUR / Homebrew, etc.) are explicitly out of
scope and will be considered much later, once there are users beyond the
author. CLI configuration also stays minimal: the service launches the
daemon with no flags (defaults: T1=5min, T_grace=60min, T_dpms=120min);
overriding values is done by editing the local service file. Proper
config-file driven tuning is M11.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] A systemd unit file is committed at
      `packaging/systemd/howan.service` (new directory). It is a `--user`
      unit (no `[Install] WantedBy=multi-user.target` etc.) with:
      - `Description=howan Wayland screensaver` (or close equivalent)
      - `After=graphical-session.target` and
        `PartOf=graphical-session.target`
      - `Type=simple`
      - `ExecStart=%h/.cargo/bin/howan daemon` — uses the systemd `%h`
        specifier (user home) so it works for whichever user installs
        howan via `cargo install` without baking in a hard path.
      - `Restart=on-failure` with a sensible `RestartSec=` (e.g. 5
        seconds) so a transient failure self-heals
      - `[Install] WantedBy=graphical-session.target` so `systemctl
        --user enable` wires the service to start at graphical login
- [x] A top-level `Makefile` is committed at the repository root with
      `install` and `uninstall` phony targets (both listed under
      `.PHONY:`). **Each target is a single-line delegation to the
      corresponding shell script** under `packaging/` — no install /
      uninstall logic in the Makefile itself. The Makefile is purely a
      task-runner facade.
- [x] `packaging/install.sh` is committed (executable, `#!/bin/sh`
      shebang, `set -e`). It performs, in order:
      1. `cd "$(dirname "$0")/.."` so the script works regardless of
         the caller's CWD.
      2. `cargo install --path crates/howan`.
      3. `install -Dm644 packaging/systemd/howan.service
         "$HOME/.config/systemd/user/howan.service"`.
      4. `systemctl --user daemon-reload`.
      5. `systemctl --user enable howan.service` — set auto-start at
         login (idempotent).
      6. `systemctl --user restart howan.service` — start fresh (first
         install) or replace the running binary (re-install). Always
         restart rather than relying on `enable --now`, because the
         latter would leave the old binary running across a re-install,
         which is the wrong behavior when iterating during development.
- [x] `packaging/uninstall.sh` is committed (executable, `#!/bin/sh`
      shebang, `set -e`). It is idempotent against the never-installed
      case (no error if the service was not enabled or the unit file is
      absent). It performs, in order:
      1. `systemctl --user disable --now howan.service 2>/dev/null
         || true`.
      2. `rm -f "$HOME/.config/systemd/user/howan.service"`.
      3. `systemctl --user daemon-reload`.
      4. `cargo uninstall howan`.
- [x] `README.md` gains a short "Install" section (or extends an existing
      one if present) documenting the make-driven workflow:
      - Install: clone the repo, run `make install`. Document what each
        step does in one sentence so a reader can audit before running.
      - Uninstall: `make uninstall`.
      - How to inspect status and logs (`systemctl --user status
        howan.service`, `journalctl --user -u howan.service`).
      - How to override CLI flags (edit
        `~/.config/systemd/user/howan.service`, then `systemctl --user
        daemon-reload && systemctl --user restart howan.service`).
- [x] No code changes to `crates/howan/src/`. M10 is purely packaging
      + documentation. The check command still runs `cargo build && cargo
      test && cargo clippy --all-targets -- -D warnings` as a regression
      gate; no new tests are added (there is nothing to unit-test in a
      static unit file or a Makefile).
- [x] The check_command grep gates pass:
      - `grep -q 'ExecStart=' packaging/systemd/howan.service` — service
        file exists and declares an exec line
      - `grep -q 'graphical-session.target' packaging/systemd/howan.service`
        — service is wired into the graphical session target
      - `grep -qE '^install:' Makefile` and `grep -qE '^uninstall:' Makefile`
        — the two task-runner targets exist
      - `grep -q '^#!/bin/sh' packaging/install.sh` and `grep -q '^#!/bin/sh'
        packaging/uninstall.sh` — the delegated shell scripts exist with
        the expected shebang
      - `grep -q 'make install' README.md` — README documents the
        make-driven install path

### Manual / on-hardware (verified by a human before merge)

- [ ] **Install + enable works end-to-end.** Run `make install` from
      the repo root on a fresh GNOME session. Confirm `systemctl --user
      status howan.service` reports `active (running)`, and `journalctl
      --user -u howan.service` shows the daemon's startup line.
- [ ] **Idle cycle works under the service.** With the service running,
      idle the seat past `T1` (default 5 min, or temporarily edit the unit
      to `--idle-timeout 30` for a faster check) and confirm the saver
      appears, dismiss it with input, leave it idle again, saver
      reappears. Same behavior as foreground `howan daemon`.
- [ ] **Restart on failure works.** While the service is running, kill
      the daemon process (`pkill -KILL howan`); within `RestartSec`
      systemd should bring it back. Confirm via `journalctl --user -u
      howan.service` and a follow-up `status` call.
- [ ] **Survives a logout / login cycle.** Log out of the GNOME session
      and log back in. The service should already be active without any
      manual intervention.
- [ ] **Daemon startup line records the effective thresholds.**
      `journalctl --user -u howan.service --since today` shows the
      `daemon starting` event with `backend=mutter`, `t1_secs`,
      `t_grace_secs`, and `t_dpms_secs` fields matching the configured
      values (or the built-in defaults when the unit is unmodified).
- [ ] **Phase 1 dismiss cycle is visible in the journal.** A full cycle
      appears in order: `idle watch armed` -> `idle detected` -> `saver
      shown` -> `input received phase=Phase1` -> `saver dismissed` ->
      `inhibitor released reason=dismiss` -> `idle watch armed
      trigger=dismiss`. Verifiable by idling past `T1` and producing
      input within `T_grace`.
- [ ] **Phase 2 cycle is visible in the journal.** With the saver up
      past `T_grace` but before `T_dpms`, input produces `input received
      phase=Phase2` followed by `lock-session issued` (or
      `lock-session failed reason=...` and the saver still dismisses,
      per the log + proceed contract) and then the same dismiss /
      re-arm tail as Phase 1.
- [ ] **Phase 3 cycle is visible in the journal.** Leaving the saver
      idle past `T_dpms` produces `phase transition 2->3` followed by
      `inhibitor released reason=dpms_handoff` and `dpms handoff:
      saver surface retained`. After the next real user activity the
      journal shows `user-active watch fired` -> `input received
      phase=Phase3` -> `saver dismissed` -> `idle watch armed
      trigger=add_user_active_watch`. The
      `trigger=add_user_active_watch` field is what distinguishes the
      Q4-gated re-arm from the M3 immediate re-arm.
- [ ] **`make install` prints a GNOME compatibility check result.**
      The final line of `make install` is either
      `[howan] GNOME compatibility check: idle-delay=<N>s, T1=<M>s, ok`
      (configuration is fine), one of the WARNING blocks documented
      below, or an informational `... check skipped ...` line.
- [ ] **`make install` warns when `gsettings idle-delay <= T1`.** Set
      `gsettings set org.gnome.desktop.session idle-delay 'uint32 300'`
      with the daemon's default `T1=300`, run `make install`, and
      confirm stderr contains a `[howan] WARNING:` block that
      (a) names the race between Mutter's blank and howan's
      `AddIdleWatch`, (b) prints a concrete
      `gsettings set org.gnome.desktop.session idle-delay 'uint32 360'`
      command with the recommended value, and (c) points to
      [`docs/guides/40-resident-daemon.md`](../../guides/40-resident-daemon.md).
      The install must still succeed (exit 0).
- [ ] **`make install` warns when `gsettings idle-delay == 0`.** Set
      `gsettings set org.gnome.desktop.session idle-delay 'uint32 0'`,
      run `make install`, and confirm stderr contains a `[howan]
      WARNING:` block that calls out that the compositor idle timer is
      disabled so Phase 3 DPMS handoff will not blank the screen, and
      recommends a concrete `gsettings set ... 'uint32 <T1+60>'`
      command. Install still succeeds.
- [ ] **`make install` skips the check silently when `gsettings` is
      unavailable.** On a system without `gsettings` (or with the GNOME
      schema absent), the check prints a single informational line
      mentioning that it was skipped — **no** `WARNING:` lines — and
      the install completes normally.

## Out of scope

- **Distro-level packaging** (deb / rpm / AUR / Homebrew / Flatpak,
  etc.). Cargo-install only at this stage; broader distribution comes
  much later, after the author has been the only user for a while.
- **System-level systemd unit** at `/usr/lib/systemd/system/` or
  `/etc/systemd/system/`. This task ships a `--user` unit only.
- **CLI / config-file integration for the service.** The unit launches
  `howan daemon` with no flags and relies on the built-in defaults
  (`T1=5min`, `T_grace=60min`, `T_dpms=120min`). Tunable values via a
  TOML config file is M11. Users who need different values for now
  edit the unit file directly.
- **Automated install / uninstall scripts.** No `cargo xtask install`,
  no Makefile, no shell wrapper. The README's four manual steps are the
  install procedure.
- **GNOME desktop entry** (`.desktop` file) or autostart via XDG. The
  daemon does not present a UI and is driven by the systemd unit, not
  by the desktop's autostart machinery.
- **Lock-failure fallback verification (Phase 2 with logind masked).**
  Still deferred from the M4 follow-up; not blocking this task.

## Implementation notes

- **Unit file path inside the repo.** Pick `packaging/systemd/howan.service`
  (new top-level directory). The `packaging/` directory name signals
  "things needed to install this project that are not source code", and
  the `systemd/` subdirectory leaves room for a future system unit or a
  drop-in fragment without renaming. The `install.sh` / `uninstall.sh`
  scripts live in `packaging/` directly (`packaging/install.sh`,
  `packaging/uninstall.sh`) so the directory groups install-time
  artifacts in one place.

- **Makefile is a thin facade.** The Makefile holds no recipe logic
  beyond a single-line call to the corresponding `.sh`. A `make install`
  target like:

  ```
  install:
  	./packaging/install.sh
  ```

  delegates to shell where loops, `set -e`, the
  `cd "$(dirname "$0")/.."` idiom, and `|| true` for idempotence read
  cleanly. Multi-line shell inside Makefile recipes runs each line in a
  fresh sub-shell unless joined with `; \`, which is exactly the kind
  of paper-cut to avoid. Keep the Makefile boring; put intelligence in
  the scripts.

- **`%h` vs hardcoded path.** Use `ExecStart=%h/.cargo/bin/howan daemon`
  so the unit works for any user who installs via `cargo install`. The
  systemd `%h` specifier expands at runtime to the user's home directory.
  An alternative `ExecStart=howan daemon` (relying on `PATH`) is fragile
  because systemd's default user-unit `PATH` may not include
  `~/.cargo/bin`. The explicit `%h/.cargo/bin/howan` is the safest form
  for the cargo-install path.

- **`graphical-session.target` vs `default.target`.** Use
  `graphical-session.target` so the service is tied to an actual
  graphical login session (Wayland needs `WAYLAND_DISPLAY` and a real
  seat). Tying to `default.target` would start it for any user login
  including headless ones, which makes no sense for a screensaver. Per
  systemd docs, `graphical-session.target` is the right anchor for
  desktop-session-dependent user services.

- **Environment.** Systemd user manager inherits `WAYLAND_DISPLAY`,
  `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR` etc. from the user
  session manager (`pam_systemd`) for services started under
  `graphical-session.target`. No explicit `Environment=` lines should
  be required for the typical GNOME-Wayland case. Document this
  assumption in the README's install section so a user on a non-standard
  setup knows where to look if the daemon can't reach Wayland.

- **`Restart=on-failure` + `RestartSec=5`.** Five seconds is a
  conservative starting value: long enough that a tight crash loop
  doesn't flood the journal, short enough that a transient hiccup
  self-heals during a saver cycle. If `WatchdogSec=` is added later it
  would belong with M9 (security guards), not here.

- **No `--no-start-on-isolate` / extra hardening yet.** Things like
  `PrivateTmp=`, `ProtectSystem=`, `NoNewPrivileges=` are tightening
  options that improve security posture but are not necessary for
  correctness. Adding them is sensible follow-up work; out of scope here
  to keep the diff focused on "service runs at login".

- **README placement.** howan's README is currently
  "overview + command reference only" per the repo's CLAUDE.md. The
  Install section is exactly the kind of content README owns; do not
  push install steps into `docs/guides/40-resident-daemon.md` (that's
  daemon-internals territory). One README section, four numbered steps,
  plus a few lines on logs / overriding flags. Keep the section short
  and concrete; do not duplicate the daemon's design rationale.

- **Conventions.** Run `cargo build && cargo test && cargo clippy
  --all-targets -- -D warnings` and fix issues before done (it should
  remain clean as no source files change). Documentation, code comments,
  commit messages, and the PR description in English. This repository
  is public — cite only public systemd / freedesktop / Wayland
  references; do not reference private repositories or internal trackers.
