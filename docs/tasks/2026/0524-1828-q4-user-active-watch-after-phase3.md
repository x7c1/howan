---
status: completed
pipeline_phase: null
plan: null
base_ref: task/0524-1049-m4-three-phase-lifecycle
blocked_by: [0524-1049-m4-three-phase-lifecycle.md]
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0524-1828-q4-user-active-watch-after-phase3
created_at: 2026-05-24T18:28:45Z
updated_at: 2026-05-25T06:19:00Z
---

# fix(daemon): defer idle re-arm after Phase 3 until the user is active again (plan Q4)

## Overview

After M4 (PR #6) the resident daemon transitions to Phase 3 at `T_dpms`:
`dpms_handoff()` drops the `Saver`, which destroys the inhibitor and the
surface, and the daemon then re-arms its idle source the same way it does
after an input dismiss. On-hardware verification surfaced that **this
immediate re-arm makes the Phase 3 handoff non-functional**:

```
T_dpms reached         saver dismissed, inhibitor released
  ↓ idle_source.rearm() called from the dismiss path
howan AddIdleWatch     fires at T1 (default 5 s)
GNOME idle-delay       e.g. 30 s
  ↓ howan wins the race
saver re-appears, new inhibitor acquired, compositor never blanks
```

Because the howan idle watch is shorter than the compositor's idle-delay, the
inhibitor is re-acquired before the compositor's blanker can fire — the
display never reaches DPMS off. This was logged as open question Q4 in the
howan plan and explicitly left out of M4's scope. M4's manual Phase 3 stage
on real hardware confirmed the failure mode, so the follow-up is now
necessary.

**Fix shape.** After Phase 3 dismiss, the daemon must not arm a new
`AddIdleWatch` until the user is actually active again. Mutter exposes
`AddUserActiveWatch` (`org.gnome.Mutter.IdleMonitor`) for exactly this
purpose: it fires once when the seat transitions from idle to active. The
backend uses that signal as the gate before adding the next idle watch. The
Phase 1/2 (input dismiss) path is unchanged — the user is by definition
present, so re-arming immediately is still correct there.

This is **a daemon-side change only**. The 3-phase machine (M4), the
composited-surface invariants (PR #3), and the idle-inhibit lifetime (PR #5)
are untouched.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] The `IdleSource` trait (`crates/howan/src/daemon.rs`) gains a new
      method (e.g. `rearm_after_active`) that arms the next idle watch
      *only* after the seat has been observed active again. The existing
      `rearm` keeps its current "arm immediately" semantics. Doc-comment on
      both methods explains the split.
- [x] `MutterIdleSource` (`crates/howan/src/daemon/mutter.rs`) implements
      `rearm_after_active` by signaling the watch thread to add an
      `AddUserActiveWatch` first, wait for it to fire, then add the regular
      `AddIdleWatch`. The two-phase loop in `run_watch_loop` is extended
      (cleanly — a new `RearmKind { Immediate, AfterActive }` enum sent
      through the existing rearm channel is the obvious shape) without
      duplicating the idle-watch arming code.
- [x] `HowanApp::dpms_handoff()` (`crates/howan/src/app.rs`) flags a
      distinct "re-arm after active" intent (not the existing
      `pending_rearm`), and `run_daemon` calls `rearm_after_active` for it
      while continuing to call `rearm` for ordinary input dismisses. Phase 1
      / Phase 2 input behavior is unchanged.
- [x] Unit tests in `daemon.rs` (the existing `FakeIdleSource` pattern)
      cover the new trait method: a fake backend records `rearm_after_active`
      calls separately from `rearm`, and a test asserts `dpms_handoff`
      results in `rearm_after_active` being called once while
      `dismiss`-from-input results in `rearm` being called once.
- [x] Unit test in `mutter.rs` (analogous to the existing
      `rearm_before_start_is_ok`) covers `rearm_after_active` as a benign
      no-op before `start` has run.
- [x] The composited-surface and idle-inhibit invariants are unchanged:
      `grep -rn set_fullscreen crates/` returns no call site, no opaque
      region is declared on the surface, and the inhibitor is still owned
      by `Saver` and destroyed in `Drop`. (Verifiable from the diff.)
- [x] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings`
      passes.

### Manual / on-hardware (verified by a human before merge)

- [ ] **Phase 3 hands off to the compositor's blank (GNOME).** Re-run the
      M4 Phase 3 manual scenario: `howan daemon --idle-timeout 5
      --grace-timeout 30 --dpms-timeout 60` with `org.gnome.desktop.session
      idle-delay = 30s`. After `T_dpms` the saver disappears and the saver
      **does not re-appear**. Within the compositor's idle-delay, the
      display physically blanks (DPMS off). First input wakes the display
      to the desktop (not to a re-shown saver), and a subsequent idle
      period shows the saver again normally (Phase 1). Record the result
      in `docs/guides/40-resident-daemon.md` under the existing Phase 3
      stage. This was the criterion M4 was unable to satisfy.
- [ ] **Blackwell SSH-guarded re-confirmation.** Repeat the above on the
      NVIDIA Blackwell + GNOME machine with the out-of-band SSH lifeline,
      to confirm the genuine DPMS off↔on transition still does not wedge
      the display engine / GSP firmware (the M4 task carried this same
      sign-off; it is repeated here because the new code path is what
      finally lets the transition actually happen).
- [ ] **Long-running cycle.** Leave the daemon running through several
      Phase 3 cycles (input wake → idle → saver → Phase 1/2/3 → DPMS off
      → input wake → ...) and confirm the daemon stays resident, the
      `rearm_after_active` path does not leak watches in Mutter, and the
      stderr log is empty across cycles.

## Out of scope

- **The wlroots backend.** Only the GNOME (Mutter) backend exists today;
  the `IdleSource` seam is preserved but no wlroots impl is written here.
- **Configuration knobs for the active-watch threshold.** Mutter's
  `AddUserActiveWatch` fires on the next idle → active transition with no
  caller-tunable threshold; expose nothing new on the CLI / TOML.
- **TOML config (M11), multi-monitor (M5), GPU/shader rendering (M6+),
  systemd `--user` packaging (M10).** Unchanged.
- **Lock screen UI / authentication.** Unchanged.

## Implementation notes

- **Trait shape.** Add `fn rearm_after_active(&self) -> Result<(), Box<dyn
  Error>>` next to the existing `rearm`. Default it to a fallback that
  calls `rearm` so a backend without active-watch support degrades to
  today's behavior; the Mutter impl overrides with the proper semantics.

- **Mutter backend.** The cleanest extension of
  `crates/howan/src/daemon/mutter.rs` is to change the mpsc rearm signal
  from `()` to a small enum:

  ```rust
  enum RearmKind { Immediate, AfterActive }
  ```

  The watch loop currently has Phase 1 (arm `AddIdleWatch`, wait for fire,
  emit `Idle`) and Phase 2 (block on `rearm_rx.recv()`). After receiving
  `RearmKind::AfterActive`, arm an `AddUserActiveWatch`, block on its
  `WatchFired` (the same `receive_watch_fired` stream — match by the
  returned watch id), then proceed back to Phase 1's `AddIdleWatch`. The
  cleanup `idle_watch` tracking already in place extends naturally to also
  track the in-flight active watch for the same best-effort `RemoveWatch`
  on exit.

- **Daemon side.** `HowanApp` currently exposes `take_pending_rearm() ->
  bool`. Replace with `take_pending_rearm() -> Option<RearmKind>` (or a
  pair of flags — pick what reads cleaner). `dismiss()` (input path) sets
  `Immediate`; `dpms_handoff()` sets `AfterActive`. `run_daemon` matches
  on the returned kind and calls the corresponding trait method. The
  one-shot `run` (`howan start`) path never reaches Phase 3 (its loop
  exits on `saver.is_none()`), so it does not need to handle
  `AfterActive`.

- **`Mutter user-active during inhibit` already covered.** The original
  M2.5 module docs (`mutter.rs`) deliberately avoided
  `AddUserActiveWatch` because the inhibitor held during saver display
  blinds Mutter's idle/active tracking. That concern is **not** relevant
  here: the active-watch added in `rearm_after_active` is armed *after*
  Phase 3 has already destroyed the saver and released the inhibitor, so
  the watch is fired by genuine user activity, not by the inhibitor
  flipping state. Update the `mutter.rs` "Re-arm strategy" docs to record
  this distinction.

- **Docs.** Extend `docs/guides/40-resident-daemon.md` "Phase lifecycle"
  with a short subsection on the post-Phase-3 handoff: the active-watch
  gate before the next idle watch, why it's needed (Q4 race), and the
  observable behavior (compositor blanks; first input wakes the desktop,
  not the saver). Record the Phase 3 stage result here. Keep DRY —
  soft-reference the "Suppressing DPMS while the saver is shown" section
  for the inhibitor lifetime; do not restate it.

- **Conventions.** Run `cargo build && cargo test && cargo clippy
  --all-targets -- -D warnings` and fix issues before done. Documentation,
  code comments, commit messages, and the PR description in English. This
  repository is public — cite only public Wayland / systemd-logind /
  Mutter references; do not reference private repositories or internal
  trackers.
