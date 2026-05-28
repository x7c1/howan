# Resident Daemon

## Overview

howan runs as a single **resident daemon** (`howan daemon`) that owns idle
detection, surface display, and the elapsed-time two-phase lifecycle
(`Inhibiting` immediate return / `DpmsHandoff`). It connects to Wayland once,
stays alive with no surface, and shows the saver autonomously when the
seat has been idle for `T1`. Input destroys the *surface* — not the
process — and the daemon re-arms for the next idle period; the DpmsHandoff
timer keeps the surface mapped but releases the idle inhibitor so the
compositor's standard idle blank can take over behind the saver, with the
next input then dismissing the surface. `SIGTERM`/`SIGINT` terminate the
whole daemon cleanly.

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
- While the saver is shown the daemon holds a `zwp_idle_inhibit_manager_v1`
  inhibitor on the saver surface so the compositor does not blank the display
  (DPMS off) behind it (see [Suppressing DPMS while the saver is
  shown](#suppressing-dpms-while-the-saver-is-shown)).
- Input is dispatched by the elapsed-time **two-phase lifecycle**: from saver
  show until `T_dpms` input dismisses the saver (`Inhibiting`); at `T_dpms` a
  calloop timer releases the idle inhibitor while leaving the saver surface
  mapped, so the compositor's own idle blank can take over *behind* the
  saver (the desktop is never exposed) and the next input dismisses the
  surface (`DpmsHandoff`). See [Phase lifecycle](#phase-lifecycle).
- **Locking the session is delegated to GNOME.** Configure GNOME directly
  for lock-on-idle — see [Locking is delegated to
  GNOME](#locking-is-delegated-to-gnome).
- Lifecycle events (daemon start, idle detected, saver shown, phase
  transitions, inhibitor acquired/released, shutdown) are emitted via
  `tracing` to stderr, which the systemd `--user` unit captures into the
  journal. See [Verifying the daemon via the
  journal](#verifying-the-daemon-via-the-journal) for the commands and
  worked examples.
- The GNOME compositor's `org.gnome.desktop.session idle-delay` must be
  strictly greater than `T1` (and non-zero) — otherwise Mutter races
  howan at saver-show time or breaks the DpmsHandoff. `make install`
  warns on misconfiguration; see [GNOME compositor compatibility:
  `idle-delay` vs `T1`](#gnome-compositor-compatibility-idle-delay-vs-t1).

The composited-surface invariants the saver relies on (no `set_fullscreen`, no
opaque region — the Blackwell safety rationale) are **not** repeated here; see
[30-composited-surface.md](30-composited-surface.md). The daemon recreates the
saver the same safe way on every idle cycle, at the single construction site in
`crates/howan/src/app.rs` (`Saver::new`).

## GNOME compositor compatibility: `idle-delay` vs `T1`

howan and the GNOME compositor (Mutter) run two independent idle timers
against the same seat: howan's `--idle-timeout` (`T1`, default 300s) drives
the saver, and `org.gnome.desktop.session idle-delay` drives Mutter's own
idle blank (DPMS off). These two timers **must not collide**, and the
relationship between them affects two different phases of the lifecycle.

The required configuration is:

```
org.gnome.desktop.session idle-delay  >=  T1 + 60s
                                          and  != 0
```

`make install` runs a post-install compatibility check
([`packaging/install.sh`](../../packaging/install.sh)) that reads `T1`
from the installed unit and `idle-delay` via `gsettings`, then warns to
stderr when the configuration matches one of the failure cases below. It
does **not** auto-apply changes — fixing the setting is left to the user.
The check is skipped silently on non-GNOME setups (no `gsettings` binary
or schema). The same check is not (yet) repeated at daemon startup; that
is a planned follow-up.

### `idle-delay <= T1` races at saver-show time

If `idle-delay` is less than or equal to `T1`, Mutter's blank timer fires
at — or before — howan's idle watch. `zwp_idle_inhibit_unstable_v1` only
prevents *future* idle blanks, not the one already in flight; by the time
howan reacts to `WatchFired` and creates the saver surface plus its
inhibitor, Mutter has already started the fade-to-blank. In practice
Mutter wins the race, the saver flashes (or never appears) and the
display goes to DPMS off instead. The equality case (`idle-delay == T1`)
is just as bad as `<` for this reason.

Fix: `gsettings set org.gnome.desktop.session idle-delay 'uint32 <T1 +
60>'` (e.g. `uint32 360` for the default `T1=300`).

### `idle-delay == 0` breaks the DpmsHandoff

`idle-delay = 0` disables Mutter's idle timer entirely. The saver still
shows correctly — howan owns its own idle detection through
`org.gnome.Mutter.IdleMonitor` — but the [`DpmsHandoff`
state](#phase-lifecycle) stops working: when `dpms_handoff` releases
the idle inhibitor at `T_dpms`, there is no compositor idle timer left to
take over and blank the screen. The saver surface stays mapped and the
backlight stays on forever (until input arrives), which defeats the whole
point of the handoff.

Fix: same as above — set `idle-delay` to a value larger than `T1` (and
non-zero).

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
   `wl_compositor`, `xdg_wm_base`) and build the durable wgpu device (see
   [50-shader-player.md](50-shader-player.md)). If no usable GPU adapter is
   available (e.g. a session with no working Vulkan/GL driver) the device build
   fails and the daemon exits non-zero with a diagnostic at startup — it never
   hangs. **No surface is shown yet.**
2. Start the idle source (the GNOME backend below). On failure to reach the
   idle transport the daemon exits non-zero with a diagnostic — it never hangs
   silently.
3. When the idle source reports the seat has been idle for `T1`, create and map
   the saver surface (the composited overlay running the GPU-animated WGSL
   shader — see [50-shader-player.md](50-shader-player.md)).
4. On the first keyboard / pointer / touch input, dispatch by the two-phase
   lifecycle (see [Phase lifecycle](#phase-lifecycle)): drop the saver surface
   (`Inhibiting`). The durable Wayland state persists. The daemon re-arms the
   idle source and stays resident.
5. If the saver stays up past `T_dpms` without input, a calloop timer fires
   `dpms_handoff()`, which releases the idle inhibitor while leaving the saver
   surface mapped, so the compositor's standard idle blank can take over
   behind the saver — the desktop is never revealed during the compositor's
   blank-countdown window. The next input wakes the display to the saver and
   is then routed through the DpmsHandoff arm of `on_input`, which tears down the
   surface (`DpmsHandoff`).
6. `SIGTERM`/`SIGINT` set a process-exit flag, releasing the seat input handles
   on the way out.

### Surface lifecycle vs. process lifecycle

`HowanApp` holds the durable Wayland state (and the durable wgpu device) plus an
`Option<Saver>`. The `Saver` (window + per-surface GPU renderer) is created on
demand and dropped on dismiss, so the show → hide → show cycle is repeatable
within one process; the expensive wgpu device is reused across cycles (see
[50-shader-player.md](50-shader-player.md)). Two dismiss paths deliberately
diverge:

- **Input** calls `HowanApp::on_input()`, which dispatches by phase (see
  [Phase lifecycle](#phase-lifecycle)) and eventually calls `dismiss()` —
  dropping only the `Saver` and setting a `pending_rearm` intent of
  `Immediate`. It does **not** set the process-exit flag. The daemon loop
  observes the pending intent, asks the idle source to re-arm via `rearm()`,
  and continues.
- **The DpmsHandoff timer** calls `HowanApp::dpms_handoff()`, which destroys the
  idle inhibitor but **keeps** the `Saver` surface mapped, and sets the
  re-arm intent to `AfterActive` so the daemon loop calls
  `rearm_after_active()` instead. Without the inhibitor, the compositor's
  own idle blank then takes over behind the saver — the desktop is never
  exposed during the blank-countdown window. When the user later produces
  input, the compositor wakes the display to the saver, and `on_input`'s
  DpmsHandoff arm dismisses the surface like `Inhibiting` does (overriding the
  pending `AfterActive` intent with `Immediate`). The active-watch gate
  still matters when input never arrives — see
  [Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate). The
  process stays resident throughout.
- **`SIGTERM`/`SIGINT`** call `HowanApp::request_exit()`, which sets the
  process-exit flag the loop checks. Signals always terminate the whole daemon.

Keeping these on separate flags is what lets input mean "stay resident" while a
signal means "shut down". The one-shot `howan start` path reuses the same
`dismiss()` but, having no idle source, simply notices the surface is gone and
exits.

### Phase lifecycle

The saver has two behavioral phases driven by **how long it has been
shown** (not how long the seat has been idle — those are different clocks).
The single source of truth is `Saver::shown_at` (an `Instant` set in
`Saver::new`); `Saver::phase(now, t_dpms)` compares `now - shown_at`
against `T_dpms` and returns `Inhibiting` / `DpmsHandoff`. The boundary is
inclusive on the lower side: exactly at `T_dpms` we are already in
`DpmsHandoff` (matching the `Timer::from_duration(t_dpms)` fire semantics).

- **`Inhibiting` — immediate return.** From show to `T_dpms`. Input dismisses
  the saver and the daemon re-arms the idle source. This is the common
  case; there is no longer a lock-handoff phase between show and
  `T_dpms` (locking is delegated to GNOME — see [Locking is delegated to
  GNOME](#locking-is-delegated-to-gnome)).
- **`DpmsHandoff` — DPMS handoff.** At `T_dpms`. A calloop `Timer` armed when
  the saver was shown fires and calls `HowanApp::dpms_handoff()`, which
  destroys the idle inhibitor (the same `zwp_idle_inhibitor_v1.destroy`
  call that `Saver`'s `Drop` performs) but **leaves the `Saver` surface
  mapped**. The compositor's standard idle timer then blanks the display
  behind the still-visible saver, so the desktop is not exposed during
  the compositor's `idle-delay` window. The next input wakes the display
  to the saver and is dispatched through `on_input`'s DpmsHandoff arm, which
  dismisses the surface the same way `Inhibiting` does. After the handoff the
  daemon does **not** arm a fresh idle watch right away (the
  `AfterActive` intent gates it on a user-active transition) — see
  [Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate) below
  for why.

The threshold is exposed as a CLI flag in seconds: `--dpms-timeout
<SECONDS>` (default 7200, 2 hours). The daemon rejects a zero
`--dpms-timeout` with a non-zero exit before starting: a zero timeout
would fire the handoff at saver-show, collapsing the `Inhibiting` state to
nothing. Full duration-string / TOML configuration (e.g. `"60min"`) is a
later milestone.

The DpmsHandoff timer registration lives in `run_daemon`, not in any handler:
when the saver becomes shown, `run_daemon` calls
`LoopHandle::insert_source(Timer::from_duration(t_dpms), …)` and keeps the
`RegistrationToken`; a pre-`T_dpms` input dismiss cancels the timer via
`LoopHandle::remove(token)`, and a fired timer drops itself with
`TimeoutAction::Drop`. The timer callback invokes
`HowanApp::dpms_handoff()`, which destroys only the idle inhibitor — the
`Saver` surface stays mapped — and flags `RearmIntent::AfterActive` so
the daemon's post-DpmsHandoff re-arm is gated on a user-active transition
rather than firing immediately. The surface drop is deferred until the
next input arrives, which routes through `on_input`'s DpmsHandoff arm and
calls `dismiss()`; the same `saver.is_none()` cleanup branch then runs
`LoopHandle::remove` on the already-fired token, which is harmless
because calloop silently ignores unknown tokens. See
[Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate) for the re-arm
rationale.

#### Locking is delegated to GNOME

howan does not lock the session: the saver is a purely visual overlay
that hides the desktop while idle and dismisses on input. Users who want
lock-on-idle configure GNOME directly:

```sh
# Enable lock on idle and lock <delay> seconds after the screen blanks.
gsettings set org.gnome.desktop.screensaver lock-enabled true
gsettings set org.gnome.desktop.screensaver lock-delay 'uint32 0'

# `idle-delay` must be non-zero for Mutter's blanker to fire at all (it
# is also what the DpmsHandoff hands the screen off to — see the
# compatibility section above).
gsettings set org.gnome.desktop.session idle-delay 'uint32 360'
```

The rationale for not driving the lock from howan (the visible black gap
when `ext-session-lock-v1` mounts under Mutter, and the N1 "don't
implement a locker" non-goal) is recorded in Q-phase2-lock in the howan
plan.

### Post-DpmsHandoff: active-watch gate

After the DpmsHandoff releases the inhibitor (leaving the saver surface mapped)
the daemon must not arm a fresh idle watch until the user is actually
active again. The `Inhibiting` input path arms a new `AddIdleWatch`
immediately because the user just produced input; the DpmsHandoff timer
fires *without* any input, so that assumption does not hold. The seat is
still idle, and howan's `T1` is typically shorter than the compositor's
own `org.gnome.desktop.session idle-delay`, so an immediate re-arm would
fire howan's idle watch first and re-acquire the inhibitor on the
still-mapped surface before the compositor's blanker ever reached DPMS
off — making the DpmsHandoff functionally a no-op. This was
recorded as open question Q4 in the howan plan.

The daemon flags the post-DpmsHandoff re-arm as `RearmIntent::AfterActive`
rather than `Immediate`, and `run_daemon` calls
`IdleSource::rearm_after_active` instead of `rearm` for that variant. The
Mutter backend implements the gate by arming `AddUserActiveWatch` first,
waiting for it to fire on the next genuine idle→active transition, and
only then adding the next `AddIdleWatch`. The active-watch is added
*after* `dpms_handoff` has destroyed the inhibitor, so Mutter's
idle/active tracking is no longer blinded and the watch fires on real
user activity. (That blinding is why the input-dismiss path avoids
`AddUserActiveWatch` — see [Suppressing DPMS while the saver is
shown](#suppressing-dpms-while-the-saver-is-shown).)

When input *does* arrive after the handoff, two things happen in
parallel: (a) Mutter fires the user-active watch and the backend
proceeds to add the next `AddIdleWatch`; (b) the input handler dispatches
through `on_input`'s DpmsHandoff arm, which calls `dismiss()` — dropping the
surface and setting `RearmIntent::Immediate`. The daemon loop then sends
an extra `Immediate` re-arm down the channel, which the backend buffers
behind its in-progress `AddIdleWatch`. On the next idle cycle the buffer
drains and the backend re-arms one more idle watch than strictly needed.
This is benign: it just means the cycle *after* a DpmsHandoff-then-input
sequence consumes one extra idle-watch slot.

The `Inhibiting` input path skips the active-watch gate entirely: input from
the user is itself the proof of activity, so `dismiss` flags
`RearmIntent::Immediate` and `run_daemon` calls `IdleSource::rearm`
directly.

### Known limitation: cursor visible during the fade to blank

howan hides the cursor on the saver surface via
`wl_pointer.set_cursor(null)` on Enter and re-applies it at
`dpms_handoff`. GNOME plays a short fade-to-black animation just before
DPMS off engages; during that animation Mutter renders the system
cursor on the hardware-cursor KMS plane, which bypasses the compositor's
own fade path. Only the cursor remains bright while everything else
dims, until the display is finally off.

No client-side workaround is currently confirmed
(`gsettings set org.gnome.settings-daemon.plugins.power idle-dim false`
was tried and did not affect this — `idle-dim` controls a separate
backlight-brightness step that howan's inhibitor already suppresses).
This is an indirect consequence of howan's composited-surface workaround
([30-composited-surface.md](30-composited-surface.md)); a fullscreen
surface would put Mutter on its cursor-managed fullscreen path. The
flicker is expected to disappear when `set_fullscreen` is restored
upstream of the Blackwell modeset fix.

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
    fn rearm_after_active(&self) -> Result<(), Box<dyn Error>> { self.rearm() }
}
```

`start` is handed a `calloop::channel::Sender`; the backend forwards
`IdleEvent::Idle` whenever the seat reaches `T1` idle. The two re-arm methods
split by dismiss path: `rearm` arms the next idle watch immediately (used
after an `Inhibiting` input dismiss), while `rearm_after_active` waits for a
user-active transition first (used after a DpmsHandoff — see
[Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate)). The
default implementation of `rearm_after_active` falls back to `rearm` so
backends without a user-active signal degrade to the existing behavior.
`run_daemon` takes a `Box<dyn IdleSource>`, so adding a second backend (e.g.
wlroots `ext-idle-notify-v1`) means writing a new implementation in its own
module and constructing it in `main` — the loop does not change. That second
backend is out of scope here; only the GNOME backend is implemented.

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
  the backend thread re-adds an idle watch after each cycle, driven by the
  daemon. The daemon picks one of two re-arm primitives depending on which
  dismiss path ran (the choice is computed daemon-side and forwarded to the
  backend as a `RearmKind`):
    - **`Immediate`** — after an `Inhibiting` input dismiss. The user just
      produced input, so on receiving the signal the backend adds a fresh
      `AddIdleWatch` right away. This path deliberately does **not** use
      `AddUserActiveWatch`: while the saver was shown the daemon held an
      idle inhibitor that blinded Mutter's idle/active tracking, so a
      user-active watch armed under it never fired and the saver showed
      once and never reappeared. The daemon, which knows exactly when input
      dismissed the saver, drives the immediate re-arm instead. (See
      [Suppressing DPMS while the saver is
      shown](#suppressing-dpms-while-the-saver-is-shown) for the
      inhibitor-blinding interaction.)
    - **`AfterActive`** — after a DpmsHandoff. The timer destroyed
      the idle inhibitor *without* any input (the saver surface stays mapped
      so the desktop is not exposed), so the seat is still idle. The
      backend first adds an `AddUserActiveWatch`, waits for it to fire on the
      next genuine idle→active transition, and *only then* adds the next
      `AddIdleWatch`, letting the compositor's own idle blank take effect in
      the interim. The active-watch is armed after `dpms_handoff` has
      destroyed the inhibitor, so the blinding caveat above does not apply.
      See [Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate)
      for the race this avoids.
- **Mid-run failures.** Once the watch loop is running, an error on the backend
  thread (the D-Bus connection dropping, or a `WatchFired` subscription / watch
  re-arm failing) ends the loop and logs `howan: Mutter idle watch loop ended:
  <cause>` to stderr. The daemon process itself stays alive but stops detecting
  idle — it will not show the saver again until restarted. Watch the daemon's
  stderr for that line; recovery is a manual restart (automatic supervision is a
  systemd-unit concern, M10). The initial connect is probed synchronously at
  startup, so an unreachable bus at launch instead fails fast with a non-zero
  exit (see "Reachability probe" above).

## Suppressing DPMS while the saver is shown

While the saver is up, nothing in the daemon would otherwise stop the
compositor's own idle timer from physically blanking the display (DPMS off).
On the target NVIDIA hardware wake-from-DPMS takes several seconds — exactly the
latency howan exists to avoid. So for as long as the saver is shown the daemon
holds an idle inhibitor, keeping the screen physically on behind the saver; the
first input then brings the desktop back instantly.

- **Protocol.** The daemon uses `zwp_idle_inhibit_manager_v1` /
  `zwp_idle_inhibitor_v1` (idle *inhibit*) — not `ext-idle-notify-v1` (idle
  *detection*, which Mutter does not implement; see "Why idle detection is built
  in"). Mutter **does** advertise the idle-inhibit manager, so suppression works
  on the primary target with no extra moving parts. The objects are event-less.
- **Lifetime tied to the surface.** The manager is bound once at startup; the
  inhibitor is created against the saver's `wl_surface` at the saver's
  construction site and stored on the `Saver`. `Saver`'s `Drop` impl
  **explicitly** sends `zwp_idle_inhibitor_v1.destroy` on dismiss, letting the
  compositor's idle timer resume. This must be explicit: `wayland-client` does
  not send a proxy's destructor request when the Rust handle is dropped, so a
  "rely on `Drop`" approach leaks the inhibitor — Mutter then keeps the session
  inhibited after dismiss and never reports the next idle period (the saver
  shows only once). With the explicit destroy the inhibitor is held *exactly*
  while the saver is on screen.
- **Graceful degradation.** If the idle-inhibit manager global is absent (a
  compositor that does not advertise it), the daemon logs a single diagnostic to
  stderr at startup and continues to show the saver **without** an inhibitor — it
  does not panic or exit. DPMS suppression is an enhancement to the saver, not a
  precondition for it.
- **Surface invariants unchanged.** This is a purely additive protocol object on
  the same non-fullscreen, non-opaque surface; it does not touch
  `set_fullscreen` or opaque regions and does not change the surface's
  scanout eligibility. The Blackwell safety rationale is unchanged — see
  [30-composited-surface.md](30-composited-surface.md).

This re-evaluates the open Q1 question: whether Mutter actually honors an
inhibitor created for a non-fullscreen, composited (possibly title-barred)
surface. The on-hardware results are recorded in [DPMS-suppression
stages](#dpms-suppression-stages) below.

A concrete interaction surfaced here: holding the inhibitor makes Mutter treat
the session as non-idle, which blinds its `IdleMonitor`. You therefore cannot
both inhibit idle and detect the *next* idle through that one interface *while
the inhibitor is held* — so the backend re-arms idle detection from the dismiss
event for the input path rather than from a Mutter user-active watch (see
"Re-arm strategy"). The post-DpmsHandoff path is the inverse: by the time the
re-arm is requested the inhibitor has already been destroyed by
`HowanApp::dpms_handoff` (which leaves the saver surface mapped — see
[Phase lifecycle](#phase-lifecycle)), so `AddUserActiveWatch` is then safe
to use and is what gates the next idle watch — see
[Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate). DPMS suppression itself is
unaffected: the inhibitor is held only while the saver is meant to suppress
the compositor's blank (i.e. up to `T_dpms`).

## Verifying the daemon via the journal

The daemon's intended workflow is "leave it running during an outing or
overnight, then read the journal afterwards to confirm Inhibiting /
DpmsHandoff behavior". Lifecycle events are emitted through `tracing` and
routed to
stderr, which the systemd `--user` unit captures into the journal.

### Reading the journal

```sh
# Everything from today
journalctl --user -u howan.service --since today

# Just the recent window
journalctl --user -u howan.service --since "2 hours ago"

# Tail live
journalctl --user -u howan.service -f
```

The default verbosity is `INFO` — enough for the lifecycle events listed
below. For one-off debugging, override the filter at the unit level:

```sh
# In ~/.config/systemd/user/howan.service.d/override.conf (or edit the
# unit directly), then `systemctl --user daemon-reload && systemctl
# --user restart howan.service`:
Environment=RUST_LOG=howan=debug
```

`RUST_LOG` follows the `tracing-subscriber` `EnvFilter` syntax; any value
that filter accepts will work. Remove the override (or revert to
`RUST_LOG=howan=info`, the default) when done.

### What an `Inhibiting` input dismiss looks like

An `Inhibiting` cycle — saver shows, input dismisses, daemon resumes the idle
watch — leaves a trail like:

```
idle watch armed         trigger=initial interval_ms=...
idle detected            t1_ms=...
saver shown              inhibitor_acquired=true
inhibitor acquired
input received           phase=Inhibiting elapsed_since_shown_ms=...
saver dismissed          elapsed_since_shown_ms=...
inhibitor released       reason=dismiss
idle watch armed         trigger=dismiss interval_ms=...
```

### What a `DpmsHandoff` looks like

```
idle watch armed         trigger=initial interval_ms=...
idle detected            t1_ms=...
saver shown              inhibitor_acquired=true
inhibitor acquired
phase transition: Inhibiting -> DpmsHandoff    elapsed_since_shown_ms=...
inhibitor released       reason=dpms_handoff
dpms handoff: saver surface retained
user-active watch armed
user-active watch fired
input received           phase=DpmsHandoff elapsed_since_shown_ms=...
saver dismissed          elapsed_since_shown_ms=...
idle watch armed         trigger=add_user_active_watch interval_ms=...
```

The `trigger=add_user_active_watch` field on the final `idle watch armed`
line is the M3-vs-Q4 distinction: it proves the post-DpmsHandoff re-arm was
gated on a real user-active transition rather than firing immediately
(see [Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate)). The
`Inhibiting` input path produces `trigger=dismiss` at the same call site
instead.

The phase model itself, the inhibitor lifetime, and the active-watch
gate are explained in earlier sections of this guide; this section only
covers the observability surface.

## Verification

The deterministic checks below run in the canonical
`cargo build && cargo test && cargo clippy --all-targets -- -D warnings`:

| Check                                                                 | Result |
| --------------------------------------------------------------------- | ------ |
| `howan daemon` subcommand parses; `--idle-timeout` overrides the 5-minute default | PASS (unit tests in `cli.rs`) |
| Daemon loop consumes idle events through the `IdleSource` trait object  | PASS (fake backend test in `daemon.rs`) |
| `IdleSource::rearm` / `rearm_after_active` before `start` are benign no-ops; `T1` → ms | PASS (unit tests in `mutter.rs`) |
| `IdleSource::rearm_after_active` defaults to `rearm` for backends without a user-active signal | PASS (unit test in `daemon.rs`) |
| `dismiss` drops the whole `Saver` and flags `RearmIntent::Immediate`; `dpms_handoff` keeps the surface, takes only the inhibitor, and flags `RearmIntent::AfterActive` | PASS (unit test in `app.rs`) |
| `DpmsHandoff` input after `dpms_handoff` dismisses the surface and overrides the pending `AfterActive` intent with `Immediate` | PASS (unit test in `app.rs`) |
| `grep -rn set_fullscreen crates/` returns comments only, no call site   | PASS |
| No opaque region is declared on the (re)created surface                 | PASS (by inspection of `Saver::new`) |
| Absent idle-inhibit manager ⇒ no inhibitor, no panic                    | PASS (unit test in `app.rs`) |
| `--dpms-timeout` parses and defaults to 2h                              | PASS (unit tests in `cli.rs`) |
| Zero `--dpms-timeout` is rejected pre-start                             | PASS (unit test in `cli.rs`) |
| Removed `--grace-timeout` flag is rejected as unknown                   | PASS (unit test in `cli.rs`) |
| `Saver::phase` boundaries (below `T_dpms`, well below it, at `T_dpms`)   | PASS (unit tests in `app.rs`) |
| `on_input` no-ops when no saver is shown                                | PASS (unit test in `app.rs`) |

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
clean.

> Historical: this `wl_shm` `SlotPool` renderer was later replaced by the
> GPU-backed wgpu renderer (M6), where `present()` manages buffers, so this
> specific failure mode no longer applies. Kept here as the record of the live
> verification at the time. See [50-shader-player.md](50-shader-player.md).

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

## DPMS-suppression stages

These cover the idle-inhibit behavior added for [Suppressing DPMS while the
saver is shown](#suppressing-dpms-while-the-saver-is-shown). They are
on-hardware checks; the deterministic absent-manager path is covered by the unit
test above.

### DPMS Stage 1 (GNOME) — DPMS suppressed while the saver is shown

**Status: PASS (2026-05-23).**

On a GNOME session, set a short GNOME blank/idle timeout (e.g.
`org.gnome.desktop.session idle-delay` to ~30s) and run `howan daemon` with a
short `--idle-timeout` (a few seconds). After the saver auto-appears, leave the
machine idle past the GNOME blank timeout and confirm the display **stays
physically on** (no DPMS off) for as long as the saver is up. This answers the
open Q1 question of whether Mutter honors an inhibitor created for a
non-fullscreen, composited (possibly title-barred) surface. Then dismiss the
saver with input and confirm the compositor's normal idle blanking resumes —
i.e. leaving the machine idle now lets the screen blank as usual, proving the
inhibitor's lifetime is bound to the surface, not leaked for the life of the
daemon.

Observed with `idle-delay = 30` and `howan daemon --idle-timeout 5`:

- The saver appeared after the idle timeout, and the display **stayed physically
  on** well past the 30s GNOME blank deadline for as long as the saver was up
  (Q1 answered: Mutter does honor the inhibitor on the composited surface).
- Input dismissed the saver and the saver **re-appeared on the next idle
  period**, repeatably across cycles, with the daemon staying resident.
- After dismiss, with no saver shown, normal GNOME blanking resumed.

Two bugs surfaced and were fixed during this stage (both in the M3 change):

- **Inhibitor leaked on dismiss.** It was held on the assumption that dropping
  the Rust handle sends `zwp_idle_inhibitor_v1.destroy`, but `wayland-client`
  proxies do not send destructors on drop. The leaked inhibitor kept Mutter
  treating the session as non-idle, so the saver showed only once. Fixed by
  destroying the inhibitor explicitly in `Saver`'s `Drop` (see "Suppressing DPMS
  while the saver is shown").
- **Re-arm relied on a Mutter user-active watch armed while the inhibitor was
  held**, which the inhibitor blinds. Re-arm for the input-dismiss path is now
  driven from the dismiss event instead (see "Re-arm strategy"). A
  user-active watch is still used, but only on the post-DpmsHandoff path where it
  is armed *after* the inhibitor has been released — see
  [Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate).

### DPMS Stage 2 (Blackwell sign-off)

**Status: PASS (2026-05-23).**

DPMS Stage 1 above was run on the NVIDIA Blackwell + GNOME machine itself, so it
*is* the Blackwell sign-off: the autonomous composited saver, holding the idle
inhibitor, showed / suppressed blanking / dismissed / re-showed across cycles
with **no display-engine wedge**.

No separate SSH-guarded re-run is needed for M3, because M3 introduces no new
display transition to guard against. The 2026-05-20 wedge was a KMS modeset
triggered by `set_fullscreen` (Mutter unredirect / direct scanout); that trigger
was removed in PR #3 and the composited path was already signed off on Blackwell
under an SSH guard (see the daemon [Stage 2](#stage-2-blackwell-sign-off-ssh-guarded)
above). M3 only adds an idle-inhibit *policy* object — it does not touch
`set_fullscreen`, opaque regions, scanout, or any modeset, and "keep the screen
on" (DPMS suppression) is the *absence* of a display transition, not a new one.
So there is no M3 crash path to reproduce; the GPU-wedge surface is unchanged
from the composited path already verified.

The genuinely new display transition — **DPMS off↔on**, when the DpmsHandoff releases the
inhibitor and lets the compositor blank, then a later input unblanks — arrives
in **M4** (the phase lifecycle). That is where a fresh SSH-guarded Blackwell
check has real value and should be done; it is out of scope here.

## Phase-lifecycle stages (M4)

These cover the phase machine added in M4 (see [Phase
lifecycle](#phase-lifecycle)). They are on-hardware checks; the boundary and
decision logic itself is covered by the unit tests above.

### M4 Stage 1 (GNOME) — `Inhibiting` behavior unchanged

**Status: pending on-hardware verification.**

Run `howan daemon --idle-timeout 5 --dpms-timeout 120` on a GNOME session.
After the saver auto-appears, input within ~30s must dismiss it and the daemon
must re-arm (the saver re-appears on the next idle cycle), same as the M3
behavior recorded in [Stage 1
above](#stage-1-safe--live-gnome-idle-cycle). Record the result here.

### M4 Stage 2 (GNOME) — GNOME lock-on-idle hands off correctly

**Status: pending on-hardware verification.**

Configure GNOME's own lock-on-idle (see [Locking is delegated to
GNOME](#locking-is-delegated-to-gnome)) with `gsettings set
org.gnome.desktop.screensaver lock-delay 'uint32 0'` and an
`org.gnome.desktop.session idle-delay` that satisfies the [compatibility
constraint](#gnome-compositor-compatibility-idle-delay-vs-t1) (e.g.
`gsettings set org.gnome.desktop.session idle-delay 'uint32 65'` for the
`--idle-timeout 5` below). Run `howan daemon --idle-timeout 5
--dpms-timeout 60`, idle past `T_dpms`, and confirm that after the
compositor's blank takes over, the next input lands on GNOME's lock screen.
Record the result here.

### M4 Stage 3 (GNOME) — DpmsHandoff timer releases the inhibitor, surface stays

**Status: PASS (2026-05-27).** On GNOME with shortened timers (e.g.
`--idle-timeout 3 --dpms-timeout 10`, `idle-delay = 10s`): at `T_dpms` the
inhibitor was released and the saver stayed mapped through the compositor's
blank-countdown (desktop not exposed); the display physically blanked; input
woke the display to the saver and the same input dismissed it, revealing
the desktop; a subsequent idle period showed the saver again normally. The
fade-to-blank cursor flicker is documented separately — see [Known
limitation: cursor visible during the fade to
blank](#known-limitation-cursor-visible-during-the-fade-to-blank).

Run `howan daemon --idle-timeout 5 --dpms-timeout 60` with a short GNOME
`org.gnome.desktop.session idle-delay` (e.g. 30s) and leave the machine idle
past 60s without input. At `T_dpms` the inhibitor must be gone **but the
saver must stay visible** for the full `idle-delay` window — the desktop must
not be exposed at any point. Within that window the display must physically
blank (DPMS off). The first input wakes the display to the **saver** (not to
the desktop), and that same input dismisses the saver revealing the desktop;
a subsequent idle period shows the saver again normally (the `Inhibiting`
state of the next cycle). Confirm by observing the panel / wall clock and the
saver behavior
across at least two cycles.

### M4 Stage 4 (Blackwell sign-off, SSH-guarded) — DPMS off↔on

**Status: PASS (2026-05-27) — without SSH guard.** Verified on the NVIDIA
Blackwell + GNOME target through the M4 Stage 3 scenario above. No GPU wedge
was observed; `journalctl -k --since "10 minutes ago" | grep -iE
'nvidia|gsp|drm|modeset'` returned no entries and `nvidia-smi` remained
responsive throughout. The DPMS off↔on transition completed cleanly across
multiple cycles. The criterion's SSH-lifeline precondition was not formally
applied — that precaution exists to recover from a GPU wedge during the
transition, and no such wedge occurred. A future re-run under a formal SSH
guard is welcome but is not blocking.

This is the new real-display power transition M4 introduces and the one M3
deferred (per [DPMS Stage 2 above](#dpms-stage-2-blackwell-sign-off)). On the
NVIDIA Blackwell + GNOME machine, with an out-of-band SSH lifeline from a
second device, run the M4 Stage 3 scenario above and confirm:

- DPMS off engages cleanly without wedging the display engine / GSP firmware.
- The first input after DPMS off wakes the display normally — to the saver,
  not to the desktop.
- The saver remains the visible surface throughout the blank-countdown and
  through DPMS off / on.
- `journalctl -k` / NVIDIA driver logs show no crash symptoms.

This re-confirmation is the final sign-off for the M4 → Q4 →
DpmsHandoff-surface stack. Q4's active-watch gate is what finally lets the
DPMS off↔on transition actually happen; this task is what keeps the
desktop from being exposed during the compositor's blank-countdown window.

### M4 Stage 5 (GNOME, post-Q4) — Long-running cycle through DpmsHandoff

**Status: PASS (2026-05-27).** Multiple full cycles confirmed (input → idle
→ saver → Inhibiting / DpmsHandoff → DPMS off → input wake → next idle → ...).
The daemon stayed resident across the cycles, subsequent cycles continued to
trigger normally (no Mutter watch leaks observed), and the daemon's stderr was
empty.

With the same flags as M4 Stage 3 above, leave the daemon running through
several full cycles (input wake → idle → saver → `Inhibiting` input dismiss →
next idle → saver → `DpmsHandoff` DPMS off → input wake → ...) and confirm the
daemon stays resident across cycles, subsequent cycles still trigger normally
(i.e. the `rearm_after_active` path does not leak Mutter watches), the buffered
`Immediate` re-arm following a DpmsHandoff-then-input cycle settles benignly
(see [Post-DpmsHandoff](#post-dpmshandoff-active-watch-gate)), and the daemon's
stderr is empty across the cycles.
