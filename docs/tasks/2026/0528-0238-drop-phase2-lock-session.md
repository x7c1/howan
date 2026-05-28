---
status: completed
pipeline_phase: null
plan: null
base_ref: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings && ! grep -rqn 'Phase2\\|LockSurveillance\\|SessionLocker\\|LogindLocker\\|lock_session\\|LockSession\\|locked hint\\|LockedHint\\|grace_timeout\\|t_grace\\|T_grace' crates/howan/src/ && ! grep -rqn 'org.freedesktop.login1' crates/howan/"
assignee: null
branch: task/0528-0238-drop-phase2-lock-session
created_at: 2026-05-27T17:38:34Z
updated_at: 2026-05-27T18:46:22Z
---

# feat(daemon): drop Phase 2 lock-session call and delegate locking to GNOME

## Overview

The howan plan's open question **Q-phase2-lock** is decided in favor of
**removing Phase 2 entirely**: howan no longer issues
`loginctl lock-session` on input after `T_grace`. The saver becomes a
purely visual screensaver that hides the desktop while idle and dismisses
on input; the responsibility for actually locking the session moves
entirely to GNOME's own configuration (`org.gnome.desktop.screensaver
lock-enabled` / `lock-delay` / `org.gnome.desktop.session idle-delay`).

### Why

During M10 (PR #9) real-machine verification it became clear that the
Phase 2 input-to-lock-screen transition produces a multi-second black
screen on a real GNOME / Mutter / Ubuntu 26.04 session. Three findings
made the lock path itself the wrong abstraction to keep:

1. The black gap is structurally unavoidable from howan. All D-Bus
   lock entry points (`loginctl lock-session`,
   `org.gnome.ScreenSaver.Lock()`, `SetActive(true)`) converge on
   GNOME Shell's `ext-session-lock-v1` client. The protocol allows the
   compositor to wait for the lock surface to be rendered before
   signalling `locked`, but Mutter does not implement that optimization
   — it blanks all outputs first and renders the lock UI second. The
   PR #9 LockedHint-wait (Q3) reduced howan's own contribution to 35 ms;
   the rest is entirely downstream and unfixable from howan.

2. Self-implementing `ext-session-lock-v1` (so howan's saver becomes
   the actual lock surface) is explicitly excluded by the plan's N1
   ("自分でロッカーを実装しない"), to avoid taking on PAM
   authentication, lockout-risk, and the security boundary of a
   real lock screen.

3. The auto-idle-lock path through GNOME does not exhibit a visible
   black gap because the screen is already DPMS-off when the lock
   fires, so the blanking step is hidden in the natural "screen off
   → key press → lock UI" transition. By dropping the manual lock
   from howan and letting the user configure GNOME's own lock-on-idle
   path, the visible black gap goes away for the common workflow.

### What changes

- **R3 in the howan plan is revised** by Q-phase2-lock to remove
  Phase 2. After this PR, the lifecycle is:
  - Phase 1 (immediate dismiss; `SaverPhase::Inhibiting` in code):
    from `T1` (saver shown) up to `T_dpms`. Any input dismisses the
    saver and the daemon re-arms.
  - Phase 3 (DPMS handoff; `SaverPhase::DpmsHandoff` in code): at
    `T_dpms`, howan releases its idle inhibitor and retains the saver
    surface so the compositor's fade-to-blank does not expose the
    desktop; the compositor then DPMS-offs the outputs according to
    its own `idle-delay`.

  The two surviving states are named after what they do (rather than
  the old `Phase1`/`Phase3` numbering, which left a confusing gap
  where Phase 2 was removed): `Inhibiting` holds the idle inhibitor,
  `DpmsHandoff` has released it to the compositor.
- **The `T_grace` boundary and the entire Phase 2 state disappear.**
  There is no longer any time-based phase transition until `T_dpms`.
- **The `--grace-timeout` CLI flag is removed.** `--idle-timeout` and
  `--dpms-timeout` remain.
- **All lock-session infrastructure is deleted**: the
  `LockSurveillance` trait, `SessionLocker` / `LogindLocker` impls,
  the `org.freedesktop.login1.Session` zbus proxy, the LockedHint
  wait logic, the timeout helper thread, the unit tests for the
  Phase 2 lock path, and the related tracing events
  (`lock-session issued, waiting for LockedHint`,
  `locked hint observed`, `locked hint not observed within timeout`,
  `lock-session failed`).
- **The cli.rs validation `T_dpms > T_grace` is removed.** The
  validation that needs to remain is `T_dpms > T1` (Phase 3 must be
  reachable after the saver has shown for at least an instant).

### How

The phase machine in `crates/howan/src/app.rs` currently has three
arms; collapse to two. Files to touch (verify by reading first; this
list may be incomplete):

- `crates/howan/src/cli.rs` — drop `grace_timeout_secs`,
  `--grace-timeout`, `DEFAULT_T_GRACE_SECS`, `grace_timeout()`, the
  `T_dpms <= T_grace` validation; rename or remove the `validate()`
  body as appropriate. Update the related tests
  (`daemon_grace_timeout_*`).
- `crates/howan/src/app.rs` — find every reference to `Phase2`,
  `t_grace`, `grace_timeout`, and the `T_grace` constant; remove the
  state, the elapsed-time check that transitions into Phase 2, and
  the Phase 2 input-handler arm. Phase 1's input-handler arm becomes
  the sole input handler until Phase 3. Update Phase enum
  (or remove if a boolean suffices) and the `phase transition`
  tracing events accordingly.
- `crates/howan/src/app/lock.rs` — delete the entire file
  (LockSurveillance, SessionLocker, LogindLocker, NoopLocker,
  zbus proxy, helper-thread plumbing, the Phase 2 unit tests).
- `crates/howan/src/app.rs` / `handlers.rs` / `render.rs` /
  `daemon.rs` / `daemon/mutter.rs` — strip out the lock-related
  wiring (constructor arguments, trait imports, plumbing).
- `crates/howan/Cargo.toml` — review whether zbus is still needed
  by the Mutter IdleMonitor side (it is — IdleMonitor uses zbus).
  Keep zbus, but its surface area shrinks. Do not remove the
  dependency.
- `crates/howan/src/daemon.rs` and `crates/howan/src/main.rs` — drop
  arguments to `run_daemon()` related to the locker/surveillance, the
  CLI plumbing of `--grace-timeout`, and any related logging fields.
- `docs/guides/40-resident-daemon.md` — rewrite the Phase 2
  description out of existence. Replace with a short subsection
  saying lock-on-idle is delegated to GNOME's
  `org.gnome.desktop.screensaver.lock-enabled` and `lock-delay`, and
  what the journal trail looks like in the simplified 2-phase
  machine. Update the "Verifying the daemon via the journal" section
  to drop the Phase 2 example and update the Phase 3 example to
  show the immediate "Phase 1 → Phase 3" transition.
- `README.md` — search for "Phase 2", "lock-session",
  "`--grace-timeout`", "`T_grace`" and update / remove.
- The previous task file (`0527-0527-m10-systemd-user-unit.md`)
  must not be modified — it is the historical record of M10.

### Unit-test seam

The phase decision logic in `app.rs` already has unit tests around
phase transitions (`crates/howan/src/app.rs` lines ~967, ~976, ~985,
etc.). These tests must be updated to reflect the 2-phase machine,
not deleted wholesale — keep the existing time-based decision
coverage but with Phase 2 removed:
- A saver-shown elapsed time of 30 s (well before `T_dpms`) → dismiss
  on input, no lock-session call.
- A saver-shown elapsed time past `T_dpms` → Phase 3 handoff fires.
- The original "Phase 2 dismisses with lock-session" tests are
  deleted (they assert behavior that no longer exists).

### Out of scope

- **The corresponding upstream plan update** (Q-phase2-lock status:
  resolved / decided, R3 wording, state-transition diagram, milestone
  M4 Done definition). That lives in a separate private tracker and
  ships as a separate PR synchronized with this one. Do not link to
  or reference private-tracker paths from this PR.
- **`install.sh`'s GNOME compatibility check.** The `idle-delay > T1`
  recommendation still applies for Phase 3 handoff (the screen needs
  GNOME's idle timer to fire DPMS off after howan releases its
  inhibitor), so the existing warnings stay. Do not change the
  check logic. Optionally clarify the message text if it currently
  mentions Phase 2 — it does not.
- **Adding new functionality.** No multi-monitor (M5), no shader
  player (M6+), no TOML config (M11). Only the deletion / collapse
  described above.
- **Changing the default `T_dpms`.** The default 2-hour `T_dpms`
  stays. Users wanting lock behavior set their own GNOME lock-delay.

## Acceptance criteria

### Automated (pipeline-verified)

The `check_command` runs `cargo build && cargo test && cargo clippy
--all-targets -- -D warnings` plus a grep gate that asserts the
Phase 2 / lock infrastructure is removed from `crates/howan/src/`.

- [x] `cargo build` succeeds with no warnings beyond clippy's
- [x] `cargo test` passes with all phase-decision unit tests updated
      to the 2-phase machine
- [x] `cargo clippy --all-targets -- -D warnings` is clean
- [x] `grep -rn 'Phase2\|LockSurveillance\|SessionLocker\|LogindLocker\|lock_session\|LockSession\|locked hint\|LockedHint\|grace_timeout\|t_grace\|T_grace' crates/howan/src/` returns no matches
- [x] `grep -rn 'org.freedesktop.login1' crates/howan/` returns no
      matches (the entire login1 D-Bus surface, including any
      generated proxy code, is removed)

### Manual / on-hardware (verified by a human before merge)

- [x] **Phase 2 boundary has no effect in the journal.** With the
      daemon running under defaults (`T1=300s`, `T_dpms=7200s`),
      stay idle past the historical `T_grace` boundary (1 h) but
      before `T_dpms` (2 h); confirm the journal shows no `phase
      transition`, no `lock-session` events, only the initial
      `idle detected` and `saver shown`.
- [x] **Input dismiss after one hour still works as Phase 1.**
      Drive an input after staying idle for more than one hour (so
      historical Phase 2 territory); the saver dismisses with the
      same journal trail as a short Phase 1 dismiss — no
      `lock-session issued`, no `LockedHint`, no GNOME lock screen
      appears. The desktop is visible immediately on dismiss.
- [x] **Phase 3 still works.** Using a short-timer drop-in
      (`--idle-timeout 30 --dpms-timeout 90`), confirm the Phase 3
      journal trail: `idle detected` → `saver shown` → `phase
      transition: Inhibiting -> DpmsHandoff` →
      `inhibitor released reason="dpms_handoff"` → `dpms handoff:
      saver surface retained` → user-active watch armed → input →
      `saver dismissed` → `idle watch armed
      trigger="add_user_active_watch"`.
- [x] **GNOME-driven lock still works under user gsettings.** Set
      `gsettings set org.gnome.desktop.screensaver lock-delay 0` and
      keep `lock-enabled true`; idle past `T_dpms` and confirm that
      after the compositor's blank takes over, the next input lands
      on GNOME's lock screen (not the desktop). This verifies that
      lock responsibility moves cleanly to GNOME without howan
      participating.

## Out of scope

See `### Out of scope` in the Overview section above.

## Implementation notes

- The deleted `LockSurveillance` orphaned-thread caveat from PR #9
  becomes moot when the whole file is removed; no follow-up needed.
- After this change, howan calls into D-Bus only for the Mutter
  IdleMonitor (via zbus). Other D-Bus surface (login1) is gone.
- The `Phase` enum likely collapses to either an enum with two
  variants (`Active`, `DpmsHandoff` — or whatever reads well) or
  even a single boolean state. Pick whichever the test code reads
  more clearly with; do not invent a richer abstraction.
