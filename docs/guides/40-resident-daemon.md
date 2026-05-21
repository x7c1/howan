# Resident Daemon

## Overview

howan runs as a single **resident daemon** (`howan daemon`) that owns idle
detection, surface display, and (in later milestones) the phased lifecycle. It
connects to Wayland once, stays alive with no surface, and shows the saver
autonomously when the seat has been idle for `T1`. The first input destroys the
*surface* — not the process — and the daemon re-arms for the next idle period.
`SIGTERM`/`SIGINT` terminate the whole daemon cleanly.

Key points:

- Idle detection is **built into** the daemon, not delegated to `swayidle`.
  Mutter does not implement `ext-idle-notify-v1`, so `swayidle` cannot detect
  idle on GNOME (see "Why idle detection is built in").
- The GNOME backend uses `org.gnome.Mutter.IdleMonitor` over D-Bus.
- Idle detection sits behind an [`IdleSource`](#the-idlesource-seam) trait, so a
  future wlroots `ext-idle-notify-v1` backend can be added without touching the
  daemon loop. Only the GNOME backend exists today.
- `T1` defaults to 5 minutes and is overridable with `--idle-timeout <seconds>`.
- The manual/debug `howan start` / `howan stop` CLI is unchanged; it is no
  longer the activation path (see [20-swayidle.md](20-swayidle.md)).

The composited-surface invariants the saver relies on (no `set_fullscreen`, no
opaque region — the Blackwell safety rationale) are **not** repeated here; see
[30-composited-surface.md](30-composited-surface.md). The daemon recreates the
saver the same safe way on every idle cycle, at the single construction site in
`crates/howan/src/app.rs` (`Saver::new`).

## Why idle detection is built in (not swayidle)

The original design delegated idle detection to an external `swayidle` watcher
that invoked `howan start` / `howan stop`. That does not work on the primary
target: **Mutter does not implement `ext-idle-notify-v1`** (it offers only
`zwp_idle_inhibit_manager_v1`, i.e. idle *inhibit*, not *detection*), so
`swayidle` exits immediately with `Compositor doesn't support idle protocol`.
The only justification for the external-watcher split was reusing an
off-the-shelf idle tool; with that gone, idle detection has to live inside howan
either way. So howan became a single resident daemon. The Q1 finding that Mutter
lacks `ext-idle-notify-v1` is recorded in [20-swayidle.md](20-swayidle.md).

## The daemon model

```text
howan daemon --idle-timeout <seconds>
```

1. Connect to Wayland and bind the durable globals (registry, seat, output,
   `wl_compositor`, `xdg_wm_base`, `wl_shm`). **No surface is shown yet.**
2. Start the idle source (the GNOME backend below). On failure to reach the
   idle transport the daemon exits non-zero with a diagnostic — it never hangs
   silently.
3. When the idle source reports the seat has been idle for `T1`, create and map
   the saver surface (the composited black overlay).
4. On the first keyboard / pointer / touch input, **drop the saver surface** and
   forget the active output. The durable Wayland state persists. The daemon
   re-arms the idle source and stays resident.
5. `SIGTERM`/`SIGINT` set a process-exit flag, releasing the seat input handles
   on the way out.

### Surface lifecycle vs. process lifecycle

`HowanApp` holds the durable Wayland state plus an `Option<Saver>`. The `Saver`
(window + `wl_shm` renderer) is created on demand and dropped on dismiss, so the
show → hide → show cycle is repeatable within one process. Two dismiss paths
deliberately diverge:

- **Input** calls `HowanApp::dismiss()`, which drops only the `Saver` and sets a
  `pending_rearm` flag. It does **not** set the process-exit flag. The daemon
  loop observes `pending_rearm`, asks the idle source to re-arm, and continues.
- **`SIGTERM`/`SIGINT`** call `HowanApp::request_exit()`, which sets the
  process-exit flag the loop checks. Signals always terminate the whole daemon.

Keeping these on separate flags is what lets input mean "stay resident" while a
signal means "shut down". The one-shot `howan start` path reuses the same
`dismiss()` but, having no idle source, simply notices the surface is gone and
exits.

### PID file

The daemon does **not** participate in the `howan start` / `howan stop` PID file
(`$XDG_RUNTIME_DIR/howan.pid`). That file is owned exclusively by the one-shot
`start`/`stop` pair: `PidFileGuard::acquire()` rejects launch if a live owner
exists and `stop` SIGTERMs whatever PID it finds, so a daemon sharing the file
would make `howan start` error and `howan stop` kill the daemon. Keeping the
daemon out of the file entirely avoids that collision; the daemon is managed
directly (foreground for now, a systemd `--user` unit later — M10). It still
shuts down cleanly on `SIGTERM`/`SIGINT`.

## The `IdleSource` seam

The daemon loop consumes idle events through the `IdleSource` trait
(`crates/howan/src/daemon.rs`), never a concrete backend type:

```rust
pub trait IdleSource {
    fn start(&self, sender: Sender<IdleEvent>) -> Result<Box<dyn IdleHandle>, Box<dyn Error>>;
    fn rearm(&self) -> Result<(), Box<dyn Error>>;
}
```

`start` is handed a `calloop::channel::Sender`; the backend forwards
`IdleEvent::Idle` whenever the seat reaches `T1` idle. `run_daemon` takes a
`Box<dyn IdleSource>`, so adding a second backend (e.g. wlroots
`ext-idle-notify-v1`) means writing a new implementation in its own module and
constructing it in `main` — the loop does not change. That second backend is out
of scope here; only the GNOME backend is implemented.

### GNOME backend: `org.gnome.Mutter.IdleMonitor`

`crates/howan/src/daemon/mutter.rs` talks to the session-bus interface
`org.gnome.Mutter.IdleMonitor` (object `/org/gnome/Mutter/IdleMonitor/Core`) via
`zbus`. There is no dependency on `swayidle` or `ext-idle-notify-v1` anywhere in
the crate.

- **Bridging async D-Bus into sync calloop.** The watch runs on a dedicated
  thread using `zbus`'s blocking API; it forwards `IdleEvent`s into the calloop
  loop through a `calloop::channel`, keeping all Wayland work on the main
  thread.
- **Reachability probe.** `start` first connects to the session bus and calls
  `GetIdletime` synchronously. If the IdleMonitor interface is unavailable (e.g.
  a non-GNOME session, or no session bus), `start` returns an error and `howan
  daemon` exits non-zero with a clear diagnostic instead of hanging.
- **Re-arm strategy.** `AddIdleWatch(interval_ms)` is one-shot — it fires
  `WatchFired(id)` once when the seat has been idle for `interval_ms` and does
  **not** re-fire on later idle periods. To get an event on *every* idle period
  the backend thread runs a small state machine: on the idle watch firing it
  emits `IdleEvent::Idle` and adds an `AddUserActiveWatch`; when that fires (the
  user returned and dismissed the saver) it adds a fresh `AddIdleWatch`. The
  loop repeats indefinitely, so the daemon's `IdleSource::rearm` is a no-op for
  this backend.
- **Mid-run failures.** Once the watch loop is running, an error on the backend
  thread (the D-Bus connection dropping, or a `WatchFired` subscription / watch
  re-arm failing) ends the loop and logs `howan: Mutter idle watch loop ended:
  <cause>` to stderr. The daemon process itself stays alive but stops detecting
  idle — it will not show the saver again until restarted. Watch the daemon's
  stderr for that line; recovery is a manual restart (automatic supervision is a
  systemd-unit concern, M10). The initial connect is probed synchronously at
  startup, so an unreachable bus at launch instead fails fast with a non-zero
  exit (see "Reachability probe" above).

## Verification

The deterministic checks below run in the canonical
`cargo build && cargo test && cargo clippy --all-targets -- -D warnings`:

| Check                                                                 | Result |
| --------------------------------------------------------------------- | ------ |
| `howan daemon` subcommand parses; `--idle-timeout` overrides the 5-minute default | PASS (unit tests in `cli.rs`) |
| Daemon loop consumes idle events through the `IdleSource` trait object  | PASS (fake backend test in `daemon.rs`) |
| `IdleSource::rearm` is a no-op for the Mutter backend; `T1` → ms        | PASS (unit tests in `mutter.rs`) |
| `grep -rn set_fullscreen crates/` returns comments only, no call site   | PASS |
| No opaque region is declared on the (re)created surface                 | PASS (by inspection of `Saver::new`) |

Fast-fail diagnostics (manual, no surface mapped):

| Check                                                          | Result (2026-05-21) |
| -------------------------------------------------------------- | ------------------- |
| No Wayland display → exit 1, `Could not find wayland compositor` | PASS |
| Unreachable session bus → exit 1, clear D-Bus diagnostic         | PASS |

### Buffer reuse fix (found during live verification)

The first live run logged `howan: failed to attach buffer: Buffer was already
active` once per saver show. The renderer cached a single `wl_shm` buffer and
re-attached it on every `render`, but `render` fires more than once per show
(configure plus output events), so it re-attached a buffer the compositor had
not yet released — a `wl_buffer` protocol error. The single-shot `start` path
rarely triggered it; the daemon's repeated show/redraw cycles did. Fixed by
taking a fresh buffer from the `SlotPool` on every `render` (the pool reuses a
released slot when one is free, giving correct double-buffering); the re-run was
clean. See `crates/howan/src/app/render.rs`.

### Stage 1 (safe) — live GNOME idle cycle

**Status: PASS (2026-05-21).**

On the GNOME / Mutter Wayland session, `howan daemon --idle-timeout 20` was run
and observed across multiple cycles: the saver auto-appeared after the idle
period, input dismissed it, and it auto-appeared **again** on the next idle
period, with the daemon process staying alive throughout (confirmed still
running after the input dismissals, and a clean `SIGTERM` shutdown). After the
buffer reuse fix above, the daemon's stderr was empty across the cycles.

### Stage 2 (Blackwell sign-off, SSH-guarded)

**Status: PASS (2026-05-21).**

The target machine is an NVIDIA Blackwell GPU (GeForce RTX 5060 Ti, GB206). The
daemon was run autonomously on the actual Blackwell + GNOME/Mutter session with
an out-of-band SSH lifeline from a second device standing by for remote recovery
(kill / log capture / reboot). Across multiple idle → show → dismiss → re-show
cycles the display engine was **not** wedged — no GSP / modeset crash symptoms —
consistent with the composited-surface safety design in
[30-composited-surface.md](30-composited-surface.md). The buffer reuse fix above
was made and re-verified within this guarded session. Coverage is the active
output only (Q2 / multi-output is a later milestone), so a residual top bar is
expected and not a regression.
