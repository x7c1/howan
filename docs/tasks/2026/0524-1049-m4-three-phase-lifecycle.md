---
status: completed
pipeline_phase: null
plan: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0524-1049-m4-three-phase-lifecycle
created_at: 2026-05-24T10:49:16Z
updated_at: 2026-05-25T00:12:00Z
---

# feat: M4 three-phase saver lifecycle (immediate / lock / DPMS handoff)

## Overview

The resident daemon (`howan daemon`, M2.5 / PR #4) currently has a single saver
state: shown on `T1` idle, dismissed on the first input — Phase 1 of the
designed lifecycle. **M4 layers the elapsed-time-driven phase machine on top of
that state**, so the saver behaves differently the longer the user stays away:

- **Phase 1 (immediate return)** — from saver show to `T_grace` (default 60
  minutes from saver-shown). Input dismisses the saver and the daemon re-arms.
  This is today's behavior.
- **Phase 2 (lock handoff)** — from `T_grace` to `T_dpms` (default 120 minutes
  from saver-shown). Input invokes systemd-logind's `Lock` method on the
  current session (the D-Bus equivalent of `loginctl lock-session`), *then*
  dismisses the saver. The compositor's lock screen takes over from there.
- **Phase 3 (DPMS handoff)** — from `T_dpms` onward, fired by a timer (no input
  needed). The daemon releases the idle inhibitor and destroys the saver
  surface, letting the compositor's standard idle blank take over.

The plumbing required is small but touches three seams:

1. **Phase state on the saver.** `Saver` (`crates/howan/src/app.rs:266`) gains a
   `shown_at: Instant` field set in `Saver::new` (`app.rs:504`); a new
   `Saver::phase(now, t_grace, t_dpms)` method returns a `SaverPhase` enum
   (`Phase1` / `Phase2` / `Phase3`) by comparing `now - shown_at` against the
   two thresholds. The "saver was shown when?" instant is the single source of
   truth.
2. **A Phase 3 timer in the daemon loop.** `HowanApp::show_saver`
   (`app.rs:342`) currently has no notion of elapsed time. The daemon
   (`run_daemon`, `app.rs:148`) needs a calloop `Timer` armed for `T_dpms`
   when a saver is shown, cancelled if the saver is dismissed first by input,
   and on fire it calls `HowanApp::dpms_handoff()` to dismiss the surface (the
   inhibitor is released by `Saver`'s existing `Drop`). Use the
   `calloop::timer::Timer` source already available — no new crate.
3. **Per-phase input behavior.** The input handlers (every place that today
   calls `HowanApp::dismiss()`, see `crates/howan/src/app/handlers.rs:387`) now
   ask the app which phase the saver is in and either dismiss (Phase 1), call
   `lock_session()` then dismiss (Phase 2), or — in Phase 3 — the surface is
   already gone so input doesn't reach a saver handler. Concretely, `dismiss()`
   stays as the "drop surface + flag re-arm" primitive; the dispatch lives in a
   new `HowanApp::on_input()` that the four input handlers call, picking the
   action by `Saver::phase(Instant::now(), …)`.

Use `zbus` (already in the dependency tree for the Mutter IdleMonitor) for
`org.freedesktop.login1.Session.Lock` rather than shelling out to `loginctl`.
The session object path can be obtained from `XDG_SESSION_ID` or, more
robustly, from `org.freedesktop.login1.Manager.GetSession("auto")` — the work
subagent should pick whichever is cleaner with the existing `zbus` blocking
API style.

This task does **not** restructure idle-source re-arm semantics. After a Phase
3 timer-driven dismiss the daemon re-arms the idle source the same way it does
on an input dismiss today; the resulting interaction with the compositor's
just-released idle timer is left to plan Q4 (see "Out of scope").

The composited-surface invariants from PR #3 and the idle-inhibit lifetime
from PR #5 are unchanged and must stay unchanged: M4 adds no surface flags, no
opaque region, and the inhibitor is still owned by `Saver` and destroyed by
its `Drop` — Phase 3 dismiss just drops the `Saver` earlier than input would
have. See `docs/guides/30-composited-surface.md` and
`docs/guides/40-resident-daemon.md`.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] `Saver` (`crates/howan/src/app.rs:266`) gains a `shown_at: Instant` set
      in `Saver::new` (`app.rs:504`), and a `Saver::phase(now: Instant,
      t_grace: Duration, t_dpms: Duration) -> SaverPhase` method returning a
      `SaverPhase` enum (`Phase1` / `Phase2` / `Phase3`). Unit tests cover all
      three branches by feeding synthetic `now` values relative to `shown_at`
      (no Wayland involvement): boundary at exactly `t_grace` is Phase 2,
      boundary at exactly `t_dpms` is Phase 3, well-below-`t_grace` is Phase 1.
- [x] The CLI gains `--grace-timeout <SECONDS>` (default 3600) and
      `--dpms-timeout <SECONDS>` (default 7200) alongside the existing
      `--idle-timeout` (`crates/howan/src/cli.rs:47`), accessible via
      `DaemonArgs::grace_timeout()` / `DaemonArgs::dpms_timeout()` returning
      `Duration`. New unit tests in `cli.rs` cover the defaults and an explicit
      override for each, mirroring the existing `--idle-timeout` tests.
- [x] `DaemonArgs` rejects `--dpms-timeout` ≤ `--grace-timeout` (returning a
      non-zero exit / clap-style error before the daemon starts) so the phase
      windows cannot be misconfigured into a degenerate state. Unit test
      asserts the error.
- [x] The daemon loop (`run_daemon`, `app.rs:148`) arms a `calloop::timer::Timer`
      for `T_dpms` when the saver is shown and cancels it on dismiss. On fire,
      the timer callback invokes a new `HowanApp::dpms_handoff()` that drops
      the `Saver` (so `Drop` destroys the inhibitor) and re-arms the idle
      source the same way an input dismiss does today. The timer source
      registration lives in `run_daemon`, not in any handler.
- [x] On input, every existing call to `HowanApp::dismiss()` in
      `crates/howan/src/app/handlers.rs` is routed through a new
      `HowanApp::on_input()` that picks behavior by `Saver::phase(now, t_grace,
      t_dpms)`: Phase 1 → `dismiss()`; Phase 2 → `lock_session()` then
      `dismiss()`; Phase 3 path cannot occur via input (the surface is gone)
      and a unit test asserts `on_input()` is a no-op when `self.saver` is
      `None`. `t_grace` / `t_dpms` are threaded into `HowanApp` at `new` time
      from the CLI args.
- [x] `HowanApp::lock_session()` calls `org.freedesktop.login1.Session.Lock`
      via `zbus` (blocking API, on the main thread — the call is fire-and-
      forget and not expected to take more than the typical D-Bus round trip).
      On failure it logs a single `howan: lock-session failed: <cause>` line
      to stderr **and still proceeds to dismiss the saver** (matching the
      plan: stderr + proceed-to-dismiss on D-Bus failure). A unit test covers
      the "lock failure → dismiss still runs" decision path by injecting a
      stub locker (see implementation notes for the seam).
- [x] `grep -rn set_fullscreen crates/` still returns no call site (comments
      only), no opaque region is declared on the saver surface, and the
      inhibitor is still created at `Saver::new` and destroyed in `Saver`'s
      `Drop` — M4 must not weaken the PR #3 / PR #5 invariants. (Verifiable
      from the diff.)
- [x] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings`
      passes.

### Manual / on-hardware (verified by a human before merge)

- [ ] **Phase 1 behavior is unchanged (GNOME).** Run `howan daemon
      --idle-timeout 5 --grace-timeout 30 --dpms-timeout 120` on a GNOME
      session. After the saver auto-appears, input within ~30s dismisses it
      and the daemon re-arms (saver re-appears on the next idle cycle), same
      as today's M3 behavior. Record in
      `docs/guides/40-resident-daemon.md` Stage notes.
- [ ] **Phase 2 input invokes the GNOME lock screen.** With the same flags,
      let the saver stay up past 30s (`T_grace`) but well under 120s, then
      input. The GNOME lock screen appears (i.e. `Session.Lock` was honored)
      and the saver is dismissed. Record the result.
- [ ] **Phase 3 timer releases the inhibitor and lets the compositor blank.**
      With `howan daemon --idle-timeout 5 --grace-timeout 30 --dpms-timeout
      60` and a short GNOME `org.gnome.desktop.session idle-delay` (e.g. 30s),
      leave the machine idle past 60s without input. At `T_dpms` the saver
      surface disappears, the inhibitor is gone, and within the compositor's
      own idle timer the display physically blanks (DPMS off). Confirm by
      observing the panel / wall clock.
- [ ] **Blackwell sign-off (SSH-guarded) for the Phase 3 DPMS off↔on
      transition.** This is the new real-display power transition M4
      introduces and the one M3 deferred (per
      `docs/guides/40-resident-daemon.md` "DPMS Stage 2"). On the NVIDIA
      Blackwell + GNOME machine, with an out-of-band SSH lifeline from a
      second device, run the Phase 3 scenario above and confirm: (a) DPMS
      off engages cleanly without wedging the display engine / GSP firmware,
      (b) the first input wakes the display normally, (c) no crash symptoms
      in `journalctl -k` / NVIDIA driver logs. Record the result in
      `docs/guides/40-resident-daemon.md` as a new stage; this answers the
      sign-off the M3 task deferred to M4.
- [ ] **Lock-failure fallback (manual injection).** Temporarily mask the
      `org.freedesktop.login1` interface (e.g. by stopping `systemd-logind`
      in a throwaway VM, or by running with `DBUS_SESSION_BUS_ADDRESS` pointed
      at an empty stub) and confirm Phase 2 input logs the stderr line and
      *still* dismisses the saver instead of getting stuck with the saver up.
      This proves the "log + proceed" branch in `lock_session()` is exercised
      end-to-end, not just under unit test.

## Out of scope

- **Post-Phase-3 idle re-arm semantics (plan Q4).** After Phase 3 dismiss the
  daemon re-arms the idle source the same way it does on input dismiss. The
  resulting interaction with the compositor's freshly-released idle timer —
  whether the saver could pop back up before the compositor's blank lands, or
  whether a follow-up `AddUserActiveWatch`-style "wait for active" seam is
  needed — is open question Q4 in the howan plan and is left to a follow-up.
  The Manual / on-hardware criterion above only requires that DPMS off lands
  at some point under `T_dpms` idle; refining the handoff is not in M4.
- **TOML config and duration-string parsing (M11).** `T_grace` / `T_dpms` are
  exposed only as CLI flags in seconds, mirroring the existing
  `--idle-timeout` shape. The TOML schema in the plan (`"60min"` /
  `"120min"`) is M11.
- **Multi-monitor coverage (M5)**, **GPU / shader rendering (M6+)**,
  **systemd `--user` packaging (M10)**. The saver stays `wl_shm` on the
  active output only; the daemon stays foreground-runnable.
- **Lock screen UI / authentication.** howan never draws an auth surface —
  the actual lock is the compositor's own screen, reached via
  `Session.Lock`. This is part of the design (non-goal: no own locker) and
  M4 must preserve it.
- **A wlroots `ext-idle-notify-v1` backend.** The `IdleSource` seam is
  unchanged; only the GNOME (Mutter) backend exists.

## Implementation notes

- **Phase enum and the saver instant.** Put `SaverPhase` next to `Saver` in
  `crates/howan/src/app.rs`. `Saver::new` takes `Instant::now()` and stores
  it; `Saver::phase(now, t_grace, t_dpms) -> SaverPhase` is pure — that is
  what the unit tests exercise. Do not thread `Instant` through more than
  necessary; the input handlers and the timer callback call
  `Instant::now()` at their own call sites.

- **Threading `t_grace` / `t_dpms` into `HowanApp`.** The cleanest place is
  `HowanApp::new`: add two `Duration` fields (`t_grace`, `t_dpms`) and pass
  them from `main.rs` via the `DaemonArgs`. `run` (one-shot `howan start`)
  can pass defaults; `run_daemon` passes the user-supplied values. The
  one-shot path keeps Phase 1 only — its loop exits on `saver.is_none()`
  before any timer fires.

- **The Phase 3 timer source.** Use `calloop::timer::Timer::from_duration`
  registered on the `LoopHandle` inside `run_daemon`. The timer is armed at
  the same point the saver becomes shown — easiest seam: when
  `HowanApp::show_saver` succeeds, return a "saver was just shown" boolean
  (or expose `app.saver.is_some()` + a one-shot "armed" flag) so the loop
  can `insert_source` the timer. Cancel by holding the `RegistrationToken`
  and removing it on dismiss / re-show. The callback calls
  `HowanApp::dpms_handoff()` which is equivalent to `dismiss()` but exists
  as a named method so the call site documents the intent and a future
  refinement (post-Phase-3 behavior, Q4) can diverge if needed without
  rewiring the timer plumbing.

- **`org.freedesktop.login1.Session.Lock`.** Define a `zbus::proxy` in a
  new module under `crates/howan/src/` (the work subagent picks the
  location consistent with the existing layout — e.g.
  `crates/howan/src/lock.rs` or `crates/howan/src/app/lock.rs`). Interface
  `org.freedesktop.login1.Session`, method `Lock()`, no args, no return.
  Obtain the session path with `GetSession("auto")` on the
  `org.freedesktop.login1.Manager` interface — `XDG_SESSION_ID` is the
  fallback but is environment-dependent. Use the **blocking** API
  (consistent with `mutter.rs`) on the main thread; the call is a single
  D-Bus round trip and not expected to block long enough to matter for
  the calloop dispatch.

- **Test seam for lock failure.** The unit-test criterion ("lock failure
  → dismiss still runs") needs a way to inject a failing locker without
  hitting D-Bus in CI. Define a small `trait SessionLocker { fn lock(&self)
  -> Result<(), Box<dyn Error>>; }`, with `LogindLocker` as the production
  impl that calls the zbus proxy, and a `FailingLocker` test double.
  `HowanApp` holds `Box<dyn SessionLocker>`. This is the same shape as
  `Box<dyn IdleSource>` already used for the idle backend — keep the seam
  consistent.

- **`on_input()` and the four handlers.** Today, the keyboard / pointer /
  touch handlers (`crates/howan/src/app/handlers.rs:387` and the impls
  above it for `WlKeyboard`, `WlPointer`, `WlTouch` and the `Closed` event
  on the window) each call `self.dismiss()` directly. Replace those call
  sites with `self.on_input()`. `on_input` reads `self.saver.as_ref()`;
  if `None`, returns. Otherwise computes `phase` and dispatches: Phase 1
  → `self.dismiss()`, Phase 2 → `self.lock_session(); self.dismiss();`,
  Phase 3 → unreachable from input (assert/log + dismiss as a defensive
  fallback). The window-closed path (compositor close) should always
  dismiss without lock.

- **Docs.** Extend `docs/guides/40-resident-daemon.md` with a short "Phase
  lifecycle" section that documents: the three phases, the timer-driven
  Phase 3 transition, the input-driven Phase 2 lock, the `--grace-timeout`
  / `--dpms-timeout` flags, the rejection of `T_dpms ≤ T_grace`, and the
  Stage results from the manual criteria above (including the Blackwell
  DPMS off↔on sign-off). Keep it DRY — soft-reference
  `30-composited-surface.md` for the surface invariants and the existing
  "Suppressing DPMS while the saver is shown" section for the inhibitor
  lifetime; do not restate them. Markdown files over ~100 lines need an
  Overview section (CLAUDE.md), which this guide already has — extend it
  rather than starting a new file.

- **Conventions.** Run `cargo build && cargo test && cargo clippy
  --all-targets -- -D warnings` and fix issues before done. Documentation,
  code comments, commit messages, and the PR description in English. This
  repository is public — cite only public Wayland / systemd-logind /
  Mutter references; do not reference private repositories or internal
  trackers.
