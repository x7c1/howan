---
status: completed
pipeline_phase: null
plan: null
base_ref: task/0524-1049-m4-three-phase-lifecycle
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0525-1816-phase3-retain-saver-surface
created_at: 2026-05-25T18:16:14Z
updated_at: 2026-05-25T19:09:00Z
---

# fix(daemon): retain saver surface across Phase 3 to avoid exposing the desktop during compositor blank

## Overview

After M4 + Q4 (PR #6 + #7), Phase 3 already hands the screen off to the
compositor cleanly — `dpms_handoff()` destroys the inhibitor and the
compositor's idle-delay countdown produces a real DPMS off. But the
hand-off has an observable downside: between `T_dpms` and the compositor's
blank, the **desktop behind the saver becomes visible** for the full
`idle-delay` window (verified on GNOME/Mutter, 2026-05-26: saver dismissed
at the 65 s mark, display blanked 30 s later, desktop visible the whole
time). For a screensaver whose stated purpose is to cover the screen
against burn-in and casual snooping, exposing the desktop right at the
moment the user has been idle the longest is wrong.

The current `dpms_handoff()` drops the whole `Saver`, which destroys both
the inhibitor *and* the surface (`Saver::Drop` sends
`zwp_idle_inhibitor_v1.destroy` and the `Window` is destroyed). The fix is
to **drop only the inhibitor, keep the surface alive**:

```
T_dpms reached
  dpms_handoff() called
    destroy the inhibitor (explicit zwp_idle_inhibitor_v1.destroy)
    leave the Saver — surface still mapped, still painted, still covers screen
  pending_rearm = Some(AfterActive)             (unchanged)

Compositor sees no inhibitor → idle-delay counts → DPMS off
  Display physically blanks
  Saver surface is still up but invisible (screen off)
  Desktop is never shown

User input
  Compositor wakes the display → saver visible again (no flash of desktop)
  Input event reaches howan → on_input() → SaverPhase::Phase3 →
    dismiss() (drops Saver, sets pending_rearm = Immediate)
  Desktop visible
```

This was the original intent of "hand off to the compositor's standard
blank" — the saver covers the screen until the compositor takes it black,
and only user activity reveals the desktop. M4's `dpms_handoff()`
over-dropped (surface as well as inhibitor); this task corrects that.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] `HowanApp::dpms_handoff()` (`crates/howan/src/app.rs`) **no longer
      drops the `Saver`**. Instead, it takes and destroys *only* the
      inhibitor (the same explicit `zwp_idle_inhibitor_v1.destroy` that
      `Saver::Drop` performs) and leaves `self.saver` as `Some(..)`. The
      `pending_rearm = Some(RearmIntent::AfterActive)` assignment is
      unchanged; the `dpms_timer_token` clearing is unchanged.
- [x] `Saver` (`crates/howan/src/app.rs`) is reshaped if needed so that
      destroying the inhibitor outside `Drop` is safe — minimally,
      `Saver::inhibitor` already being `Option<ZwpIdleInhibitorV1>` is
      enough: `dpms_handoff` does `if let Some(i) = saver.inhibitor.take()
      { i.destroy() }`. `Saver::Drop` continues to destroy whatever
      inhibitor is still held (it now expects `None` after `dpms_handoff`,
      which is the no-op path of the same code).
- [x] `HowanApp::on_input()` (`crates/howan/src/app.rs`) Phase 3 arm —
      which M4 marked as defensively-unreachable — becomes the **reachable
      path** that input now takes when the user comes back after a Phase 3
      blank. It calls `self.dismiss()` (same as Phase 1) so the surface is
      torn down and `pending_rearm` becomes `Some(Immediate)` (overriding
      the prior `AfterActive`). Update the doc-comment on that arm to
      describe the new flow.
- [x] Unit tests cover the new shape:
      1. After `dpms_handoff()`, `self.saver.is_some()` remains true and
         `self.saver.as_ref().unwrap().inhibitor.is_none()`.
      2. After `dpms_handoff()` + a simulated `on_input()` at a `now`
         past `T_dpms`, `self.saver.is_none()` and `pending_rearm ==
         Some(RearmIntent::Immediate)` (the Phase 3 input-dismiss path).
- [x] `grep -rn set_fullscreen crates/` still returns no call site
      (comments only), no opaque region is declared on the surface, and
      the inhibitor (when present) is still owned by `Saver` and destroyed
      either by `dpms_handoff` or by `Drop`. (Verifiable from the diff.)
- [x] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings`
      passes.

### Manual / on-hardware (verified by a human before merge)

This is the **final-stage verification for the M4 → Q4 → Phase-3-surface
stack**. PR #6's Manual #3 / #4 and PR #7's Blackwell + long-running
criteria were deferred to here because the surface-retention change
touches the same Phase 3 code path; running them on the intermediate
state would have to be repeated. The verifications below cover all of
that combined behavior.

- [ ] **Phase 3 blanks the display without exposing the desktop (GNOME).**
      Re-run the M4/Q4 Phase 3 scenario (`howan daemon --idle-timeout 5
      --grace-timeout 30 --dpms-timeout 60`, `org.gnome.desktop.session
      idle-delay = 30s`). At `T_dpms` the saver **stays visible** (not the
      desktop) for the full `idle-delay` window, then the display physically
      blanks (DPMS off). First input wakes the display to the **saver**
      (not the desktop), and the same input dismisses the saver, revealing
      the desktop. Subsequent idle period shows the saver normally (Phase
      1 cycle resumed).
- [ ] **Blackwell SSH-guarded final sign-off** for the M4 → Q4 →
      Phase-3-surface stack. On the NVIDIA Blackwell + GNOME machine with
      an out-of-band SSH lifeline, run the GNOME scenario above end-to-end.
      Confirm: (a) the DPMS off / on transition does not wedge the display
      engine or GSP firmware; (b) the saver remains the visible surface
      through the blank; (c) no crash symptoms in `journalctl -k` /
      `nvidia-smi`. Record under the existing Phase 3 stage in
      `docs/guides/40-resident-daemon.md`; this is the sign-off PR #6 and
      PR #7 deferred.
- [ ] **Long-running cycle.** Drive the daemon through several full
      cycles (input → idle → saver → Phase 1/2/3 → DPMS off → input → ...)
      and confirm: the daemon stays resident, no watch leaks in Mutter,
      stderr empty, the buffered-`Immediate`-on-`Step 1` race the run log
      anticipated (see Implementation notes) settles benignly.

## Out of scope

- **The wlroots backend.** `IdleSource` trait surface is unchanged.
- **Replacing the static black-pixel saver with shaders (M6+).** The
  surface stays `wl_shm` black; this task is about *when* the surface is
  destroyed, not *what* it shows.
- **TOML config (M11), multi-monitor (M5), systemd `--user` packaging
  (M10).** Unchanged.

## Implementation notes

- **The `Saver::Drop` invariant stays.** `Drop for Saver` continues to
  destroy whatever inhibitor is held — that protects the input-dismiss
  path (Phase 1/2/3 via `on_input` → `dismiss` drops the `Saver` and Drop
  fires). `dpms_handoff` simply performs the inhibitor destroy *earlier
  and outside* a full `Saver` drop, leaving the field `None` so `Drop`'s
  subsequent destroy is a no-op (`Option::take` already returned `None`).
  This is exactly the existing pattern in `Saver::Drop`'s code; just
  invoked from a second site.

- **Backend channel state when input arrives during the Phase 3 wait.**
  After `dpms_handoff` the backend is in Step 3 (waiting on the
  `AddUserActiveWatch` `WatchFired`). When the user inputs:
    1. Mutter fires the user-active `WatchFired` → backend Step 3 ends,
       backend goes to Step 1 (arms `AddIdleWatch`).
    2. In parallel, howan's input handler runs: `on_input` → Phase 3 →
       `dismiss()` → `pending_rearm = Some(Immediate)` → daemon sends
       `RearmKind::Immediate` on the channel.
    3. Backend is in Step 1 blocked on `signals.next()`; the channel
       buffers `Immediate`. After the *next* idle watch fires (T1 later,
       when the user has gone idle again), Step 1 completes, Step 2 reads
       the buffered `Immediate`, and Step 1 re-arms once. This is benign
       — it just means the next Phase-1 cycle uses one extra idle-watch
       arm. The long-running-cycle manual criterion above covers this
       observationally.

- **`on_input` Phase 3 arm becomes reachable.** M4's code said this arm
  was "defensively unreachable, defensive dismiss as a fallback". After
  this change it is the *normal* path for input after Phase 3. Rewrite the
  doc-comment to reflect that. The dispatch (`self.dismiss()`) is exactly
  what M4 already used as the fallback, so no logic change is needed
  beyond the comment.

- **`pending_rearm` precedence.** After `dpms_handoff` sets `AfterActive`,
  a subsequent `dismiss()` (from input) sets `Immediate`. This overwrite
  is correct: once the user is back, we want a normal `rearm()`, not a
  `rearm_after_active()`. The backend may have already armed the
  user-active watch by then; that's fine — the user-active watch fired
  from the same input event that triggered the dismiss, so the backend is
  already on its way to Step 1.

- **No-op when there is no saver.** If `dpms_handoff` is somehow called
  with `self.saver` already `None` (only the input-vs-timer race covered
  by M4's existing idempotency), it must be a benign no-op — `pending_rearm`
  is not overwritten, and no inhibitor destroy is attempted. The current
  code's `if self.saver.take().is_some()` guard should become
  `if let Some(saver) = self.saver.as_mut() { ... }` to preserve the same
  guard while moving from drop-the-whole-thing to mutate-in-place.

- **Docs.** Update `docs/guides/40-resident-daemon.md` "Phase lifecycle"
  → Phase 3 paragraph, the "Post-Phase-3 handoff" subsection, and the
  Surface-vs-process lifecycle bullet for the Phase 3 timer to reflect
  the new "surface stays, inhibitor goes" shape. Record the Stage results
  in the appropriate stage. Keep DRY against the M4/Q4 sections.

- **Conventions.** Run `cargo build && cargo test && cargo clippy
  --all-targets -- -D warnings` and fix issues before done. Documentation,
  code comments, commit messages, and the PR description in English. This
  repository is public — cite only public Wayland / systemd-logind /
  Mutter references; do not reference private repositories or internal
  trackers.
