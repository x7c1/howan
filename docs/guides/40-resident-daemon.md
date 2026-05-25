# Resident Daemon

## Overview

howan runs as a single **resident daemon** (`howan daemon`) that owns idle
detection, surface display, and the elapsed-time phased lifecycle (Phase 1
immediate return / Phase 2 lock-session handoff / Phase 3 DPMS handoff). It
connects to Wayland once, stays alive with no surface, and shows the saver
autonomously when the seat has been idle for `T1`. Input or the Phase 3 timer
destroys the *surface* — not the process — and the daemon re-arms for the next
idle period. `SIGTERM`/`SIGINT` terminate the whole daemon cleanly.

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
- Input is dispatched by the elapsed-time **three-phase lifecycle**: from saver
  show until `T_grace` input dismisses the saver (Phase 1); from `T_grace` to
  `T_dpms` input first asks logind to lock the session, then dismisses (Phase
  2); at `T_dpms` a calloop timer drops the saver and releases the inhibitor
  so the compositor's own idle blank takes over (Phase 3). See [Phase
  lifecycle](#phase-lifecycle).

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
4. On the first keyboard / pointer / touch input, dispatch by the three-phase
   lifecycle (see [Phase lifecycle](#phase-lifecycle)): drop the saver surface
   (Phase 1), or lock the session via logind and then drop it (Phase 2). The
   durable Wayland state persists. The daemon re-arms the idle source and stays
   resident.
5. If the saver stays up past `T_dpms` without input, a calloop timer fires
   `dpms_handoff()`, which drops the saver surface so the inhibitor is released
   and the compositor's standard idle blank can take over (Phase 3).
6. `SIGTERM`/`SIGINT` set a process-exit flag, releasing the seat input handles
   on the way out.

### Surface lifecycle vs. process lifecycle

`HowanApp` holds the durable Wayland state plus an `Option<Saver>`. The `Saver`
(window + `wl_shm` renderer) is created on demand and dropped on dismiss, so the
show → hide → show cycle is repeatable within one process. Two dismiss paths
deliberately diverge:

- **Input** calls `HowanApp::on_input()`, which dispatches by phase (see
  [Phase lifecycle](#phase-lifecycle)) and eventually calls `dismiss()` —
  dropping only the `Saver` and setting a `pending_rearm` intent of
  `Immediate`. It does **not** set the process-exit flag. The daemon loop
  observes the pending intent, asks the idle source to re-arm via `rearm()`,
  and continues.
- **The Phase 3 timer** calls `HowanApp::dpms_handoff()`, which also drops
  the `Saver` (releasing the inhibitor) but sets the re-arm intent to
  `AfterActive` so the daemon loop calls `rearm_after_active()` instead.
  This gates the next idle watch on a user-active transition — see
  [Post-Phase-3 handoff](#post-phase-3-handoff-active-watch-gate). The
  process stays resident.
- **`SIGTERM`/`SIGINT`** call `HowanApp::request_exit()`, which sets the
  process-exit flag the loop checks. Signals always terminate the whole daemon.

Keeping these on separate flags is what lets input mean "stay resident" while a
signal means "shut down". The one-shot `howan start` path reuses the same
`dismiss()` but, having no idle source, simply notices the surface is gone and
exits.

### Phase lifecycle

The saver has three behavioral phases driven by **how long it has been
shown** (not how long the seat has been idle — those are different clocks).
The single source of truth is `Saver::shown_at` (an `Instant` set in
`Saver::new`); `Saver::phase(now, t_grace, t_dpms)` compares `now -
shown_at` against the two thresholds and returns `Phase1` / `Phase2` /
`Phase3`. Boundaries are inclusive on the lower side: exactly at `T_grace`
we are already in Phase 2, exactly at `T_dpms` we are already in Phase 3
(matching the `Timer::from_duration(t_dpms)` fire semantics).

- **Phase 1 — immediate return.** From show to `T_grace`. Input dismisses
  the saver and the daemon re-arms the idle source, same as the M3
  behavior. This is the common case.
- **Phase 2 — lock handoff.** From `T_grace` to `T_dpms`. Input calls
  `org.freedesktop.login1.Session.Lock` on the current session (the D-Bus
  equivalent of `loginctl lock-session`), then dismisses the saver. The
  compositor's lock screen takes over from there — howan never draws an
  auth surface itself (non-goal: no own locker). The lock call is
  fire-and-forget; if it fails the daemon logs a single `howan: lock-session
  failed: <cause>` line to stderr and **still proceeds to dismiss** so the
  user is never left staring at a saver they cannot get out of.
- **Phase 3 — DPMS handoff.** At `T_dpms`. A calloop `Timer` armed when
  the saver was shown fires and drops the `Saver`. `Saver`'s `Drop`
  releases the inhibitor; the compositor's standard idle timer then
  blanks the display normally. No input is needed; if input *does* arrive
  the surface is already gone and the daemon's input handlers no-op.
  After the handoff the daemon does **not** arm a fresh idle watch right
  away — see [Post-Phase-3
  handoff](#post-phase-3-handoff-active-watch-gate) below for why.

The two thresholds are exposed as CLI flags in seconds: `--grace-timeout
<SECONDS>` (default 3600, 1 hour) and `--dpms-timeout <SECONDS>` (default
7200, 2 hours). The daemon rejects `--dpms-timeout` ≤ `--grace-timeout`
with a non-zero exit before starting, because collapsing the windows would
silently make Phase 2 unreachable. Full duration-string / TOML
configuration (e.g. `"60min"`) is a later milestone.

The Phase 3 timer registration lives in `run_daemon`, not in any handler:
when the saver becomes shown, `run_daemon` calls
`LoopHandle::insert_source(Timer::from_duration(t_dpms), …)` and keeps the
`RegistrationToken`; an input dismiss cancels the timer via
`LoopHandle::remove(token)`, and a fired timer drops itself with
`TimeoutAction::Drop`. The timer callback invokes
`HowanApp::dpms_handoff()`, which drops the `Saver` the same way
`dismiss()` does but flags `RearmIntent::AfterActive` so the daemon's
post-Phase-3 re-arm is gated on a user-active transition rather than
firing immediately — see [Post-Phase-3
handoff](#post-phase-3-handoff-active-watch-gate) for why.

The session lock call uses `zbus`'s blocking API on the main thread; it is
a single D-Bus round trip and short enough not to perturb calloop
dispatch. The session object path is resolved once at daemon startup via
`org.freedesktop.login1.Manager.GetSession("auto")`, which returns the
caller's current session without depending on `XDG_SESSION_ID`. If logind
is unreachable at startup (a non-systemd session), the daemon falls back
to a no-op locker so it still runs — Phase 2 then behaves like Phase 1,
which is strictly safer than refusing to start.

The composited-surface invariants from [30-composited-surface.md](30-composited-surface.md)
and the inhibitor lifetime from [Suppressing DPMS while the saver is
shown](#suppressing-dpms-while-the-saver-is-shown) are **unchanged** by
the phase machine: M4 adds no surface flags, no opaque region, and the
inhibitor is still owned by `Saver` and destroyed by its `Drop` — Phase 3
just drops the `Saver` earlier than input would have.

### Post-Phase-3 handoff: active-watch gate

After Phase 3 drops the saver the daemon must not arm a fresh idle watch
until the user is actually active again. The Phase 1 / Phase 2 input path
arms a new `AddIdleWatch` immediately because the user just produced input;
the Phase 3 timer fires *without* any input, so that assumption does not
hold. The seat is still idle, and howan's `T1` is typically shorter than
the compositor's own `org.gnome.desktop.session idle-delay`, so an
immediate re-arm would fire howan's idle watch first, re-show the saver,
and re-acquire the inhibitor before the compositor's blanker ever reached
DPMS off — making the Phase 3 handoff functionally a no-op. This was
recorded as open question Q4 in the howan plan.

The daemon flags the post-Phase-3 re-arm as `RearmIntent::AfterActive`
rather than `Immediate`, and `run_daemon` calls
`IdleSource::rearm_after_active` instead of `rearm` for that variant. The
Mutter backend implements the gate by arming `AddUserActiveWatch` first,
waiting for it to fire on the next genuine idle→active transition, and
only then adding the next `AddIdleWatch`. The active-watch is added
*after* `Saver`'s `Drop` has released the inhibitor, so Mutter's
idle/active tracking is no longer blinded and the watch fires on real
user activity. (That blinding is why the input-dismiss path avoids
`AddUserActiveWatch` — see [Suppressing DPMS while the saver is
shown](#suppressing-dpms-while-the-saver-is-shown).)

The Phase 1 / Phase 2 input path is unchanged — see
`IdleSource::rearm`. On-hardware behavior is verified in
[M4 Stage 3](#m4-stage-3-gnome--phase-3-timer-releases-the-inhibitor).

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
after Phase 1 / Phase 2 input dismiss), while `rearm_after_active` waits for
a user-active transition first (used after a Phase 3 DPMS handoff — see
[Post-Phase-3 handoff](#post-phase-3-handoff-active-watch-gate)). The
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
    - **`Immediate`** — after a Phase 1 / Phase 2 input dismiss. The user just
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
    - **`AfterActive`** — after a Phase 3 DPMS handoff. The saver was dropped
      by the timer *without* any input, so the seat is still idle. The
      backend first adds an `AddUserActiveWatch`, waits for it to fire on the
      next genuine idle→active transition, and *only then* adds the next
      `AddIdleWatch`, letting the compositor's own idle blank take effect in
      the interim. The active-watch is armed after `Saver`'s `Drop` has
      released the inhibitor, so the blinding caveat above does not apply.
      See [Post-Phase-3 handoff](#post-phase-3-handoff-active-watch-gate)
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
"Re-arm strategy"). The post-Phase-3 path is the inverse: by the time the
re-arm is requested the inhibitor has already been released by `Saver`'s
`Drop`, so `AddUserActiveWatch` is then safe to use and is what gates the next
idle watch — see [Post-Phase-3
handoff](#post-phase-3-handoff-active-watch-gate). DPMS suppression itself is
unaffected: the inhibitor is held only while the saver is shown.

## Verification

The deterministic checks below run in the canonical
`cargo build && cargo test && cargo clippy --all-targets -- -D warnings`:

| Check                                                                 | Result |
| --------------------------------------------------------------------- | ------ |
| `howan daemon` subcommand parses; `--idle-timeout` overrides the 5-minute default | PASS (unit tests in `cli.rs`) |
| Daemon loop consumes idle events through the `IdleSource` trait object  | PASS (fake backend test in `daemon.rs`) |
| `IdleSource::rearm` / `rearm_after_active` before `start` are benign no-ops; `T1` → ms | PASS (unit tests in `mutter.rs`) |
| `IdleSource::rearm_after_active` defaults to `rearm` for backends without a user-active signal | PASS (unit test in `daemon.rs`) |
| `dismiss` flags `RearmIntent::Immediate`; `dpms_handoff` flags `RearmIntent::AfterActive` | PASS (unit test in `app.rs`) |
| `grep -rn set_fullscreen crates/` returns comments only, no call site   | PASS |
| No opaque region is declared on the (re)created surface                 | PASS (by inspection of `Saver::new`) |
| Absent idle-inhibit manager ⇒ no inhibitor, no panic                    | PASS (unit test in `app.rs`) |
| `--grace-timeout` / `--dpms-timeout` parse and default to 1h / 2h       | PASS (unit tests in `cli.rs`) |
| Degenerate `--dpms-timeout ≤ --grace-timeout` is rejected pre-start     | PASS (unit test in `cli.rs`) |
| `Saver::phase` boundaries (below `T_grace`, at `T_grace`, at `T_dpms`)   | PASS (unit tests in `app.rs`) |
| `on_input` no-ops when no saver is shown                                | PASS (unit test in `app.rs`) |
| Phase 2 dismisses even when the locker fails (log + proceed contract)   | PASS (unit test in `app.rs` with a `FailingLocker` stub) |

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
  user-active watch is still used, but only on the post-Phase-3 path where it
  is armed *after* the inhibitor has been released — see [Post-Phase-3
  handoff](#post-phase-3-handoff-active-watch-gate).

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

The genuinely new display transition — **DPMS off↔on**, when Phase 3 releases the
inhibitor and lets the compositor blank, then a later input unblanks — arrives
in **M4** (the 3-phase lifecycle). That is where a fresh SSH-guarded Blackwell
check has real value and should be done; it is out of scope here.

## Phase-lifecycle stages (M4)

These cover the three-phase machine added in M4 (see [Phase
lifecycle](#phase-lifecycle)). They are on-hardware checks; the boundary and
decision logic itself is covered by the unit tests above.

### M4 Stage 1 (GNOME) — Phase 1 behavior unchanged

**Status: pending on-hardware verification.**

Run `howan daemon --idle-timeout 5 --grace-timeout 30 --dpms-timeout 120` on a
GNOME session. After the saver auto-appears, input within ~30s must dismiss it
and the daemon must re-arm (the saver re-appears on the next idle cycle), same
as the M3 behavior recorded in [Stage 1
above](#stage-1-safe--live-gnome-idle-cycle). Record the result here.

### M4 Stage 2 (GNOME) — Phase 2 input invokes the lock screen

**Status: pending on-hardware verification.**

With the same flags as M4 Stage 1, let the saver stay up past 30s (`T_grace`)
but well under 120s, then input. The GNOME lock screen must appear (proving
`org.freedesktop.login1.Session.Lock` was honored) and the saver must be
dismissed. Record the result here.

### M4 Stage 3 (GNOME) — Phase 3 timer releases the inhibitor

**Status: pending on-hardware verification (post-Q4 fix).**

Run `howan daemon --idle-timeout 5 --grace-timeout 30 --dpms-timeout 60` with
a short GNOME `org.gnome.desktop.session idle-delay` (e.g. 30s) and leave the
machine idle past 60s without input. At `T_dpms` the saver surface must
disappear, the inhibitor must be gone, the saver must **not** re-appear, and
within the compositor's own idle-delay the display must physically blank
(DPMS off). The first input wakes the display to the desktop (not to a
re-shown saver); a subsequent idle period shows the saver again normally
(Phase 1 of the next cycle). Confirm by observing the panel / wall clock and
the saver behavior across at least two cycles. Record the result here.

The initial M4 attempt observed the saver re-appearing immediately after
`T_dpms` because the daemon re-armed its idle watch on the same path as an
input dismiss — howan's `T1` (5s) won the race against the compositor's
30s idle-delay, so the inhibitor was re-acquired before the blanker could
fire. The Q4 follow-up (this task) gates the post-Phase-3 re-arm on a
user-active transition; see [Post-Phase-3
handoff](#post-phase-3-handoff-active-watch-gate).

### M4 Stage 4 (Blackwell sign-off, SSH-guarded) — DPMS off↔on

**Status: pending on-hardware verification (post-Q4 fix).**

This is the new real-display power transition M4 introduces and the one M3
deferred (per [DPMS Stage 2 above](#dpms-stage-2-blackwell-sign-off)). On the
NVIDIA Blackwell + GNOME machine, with an out-of-band SSH lifeline from a
second device, run the M4 Stage 3 scenario above and confirm:

- DPMS off engages cleanly without wedging the display engine / GSP firmware.
- The first input after DPMS off wakes the display normally.
- `journalctl -k` / NVIDIA driver logs show no crash symptoms.

This re-confirmation is repeated post-Q4 because the active-watch gate is
what finally lets the DPMS off↔on transition actually happen — the M4-only
implementation never reached DPMS off in practice (see Stage 3 above).
Record the result here; this answers the sign-off the M3 task deferred to
M4 (and that M4 in turn was unable to satisfy until Q4 landed).

### M4 Stage 5 — Lock-failure fallback (manual injection)

**Status: pending on-hardware verification.**

Two distinct failure paths exist; pick the one that matches what you want to
verify, because each emits a different diagnostic:

- **Per-call `Lock` failure (the "log + proceed" branch in `on_input`).**
  Start `howan daemon` while `systemd-logind` is reachable so `build_locker()`
  picks `LogindLocker`, then break logind out from under the running daemon
  (e.g. `sudo systemctl stop systemd-logind` after the saver has appeared, or
  use a polkit / firewall trick to deny the next `Session.Lock` call). Drive
  Phase 2 input and confirm the daemon logs `howan: lock-session failed:
  <cause>` to stderr and **still** dismisses the saver — i.e. the user is not
  stuck. This is the path the unit test with `FailingLocker` exercises in CI.
- **Startup fallback (`LogindLocker::new()` failure).** Stop `systemd-logind`
  *before* launching `howan daemon`, or point `DBUS_SYSTEM_BUS_ADDRESS` at an
  empty stub, so the production locker cannot be constructed. Confirm that at
  startup the daemon logs `howan: systemd-logind session lock unavailable
  (<cause>); Phase 2 input will dismiss the saver without locking` and from
  then on Phase 2 input dismisses silently (no per-call `lock-session failed`
  line, because the in-process locker is the `NoopLocker` and never errors).
  This proves the degradation path documented in the Phase lifecycle section
  above — daemon-still-runs, Phase 2 collapses to Phase 1 behavior.

### M4 Stage 6 (GNOME, post-Q4) — Long-running cycle through Phase 3

**Status: pending on-hardware verification (post-Q4 fix).**

With the same flags as M4 Stage 3 above, leave the daemon running through
several full cycles (input wake → idle → saver → Phase 1 input dismiss → next
idle → saver → Phase 2 lock+dismiss → next idle → saver → Phase 3 DPMS off
→ input wake → ...) and confirm the daemon stays resident across cycles,
subsequent cycles still trigger normally (i.e. the `rearm_after_active`
path does not leak Mutter watches), and the daemon's stderr is empty
across the cycles.
