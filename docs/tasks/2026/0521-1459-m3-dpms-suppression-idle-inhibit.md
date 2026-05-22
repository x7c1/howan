---
status: completed
pipeline_phase: null
plan: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0521-1459-m3-dpms-suppression-idle-inhibit
created_at: 2026-05-21T14:59:41Z
updated_at: 2026-05-22T15:15:33Z
---

# feat: M3 suppress DPMS while the saver is shown (idle-inhibit-unstable-v1)

## Overview

The resident daemon (`howan daemon`, added in M2.5 / PR #4) now shows the saver
surface when the seat goes idle for `T1` and tears it down on input, staying
resident across cycles (`crates/howan/src/app.rs:144` `run_daemon`). But while
the saver is up, nothing stops the compositor's own idle timer from physically
blanking the display (DPMS off). On NVIDIA the wake-from-DPMS takes several
seconds — exactly the latency howan exists to avoid. **M3 makes the daemon hold
a `zwp_idle_inhibit_manager_v1` inhibitor on the saver surface for as long as the
saver is shown**, so the screen stays physically on behind the saver and input
brings the desktop back instantly.

This is the natural fit for the inhibit protocol: Mutter does **not** implement
`ext-idle-notify-v1` (idle *detection*) but it **does** advertise
`zwp_idle_inhibit_manager_v1` (idle *inhibit*) — see the Q1 finding recorded in
`docs/guides/40-resident-daemon.md`. So idle-inhibit is available on the primary
target without any extra moving parts. The inhibitor protocol (`zwp_idle_inhibit_manager_v1`
and `zwp_idle_inhibitor_v1`) is not in the `wayland-client` core crate; it lives in the
`wayland-protocols` crate under `wp::idle_inhibit::zv1::client`, which this task
adds as a dependency. Both objects are event-less, so they need only no-op
`Dispatch` impls (`wayland_client::delegate_noop!`).

The lifecycle must be tied to the surface, not the process. The saver is the
recreatable `Saver` (`crates/howan/src/app.rs:256`), created on demand in
`HowanApp::show_saver` (`app.rs:301`) and dropped in `HowanApp::dismiss`
(`app.rs:391`). The inhibitor should be created when the saver surface is created
and stored **on `Saver`** so that dropping the saver on dismiss also drops the
inhibitor — which sends `zwp_idle_inhibitor_v1.destroy` and lets the compositor's
idle timer resume. That keeps the show → dismiss → show cycle correct for free:
inhibit is on exactly while the saver is on screen, with no separate state to
keep in sync.

The composited-surface invariants from PR #3 are unchanged and must stay
unchanged: this is a purely additive protocol object on the same non-fullscreen,
non-opaque surface (`docs/guides/30-composited-surface.md`). M3 does not touch
`set_fullscreen` or opaque regions, and does not change the scanout-eligibility
of the surface, so it carries no new Blackwell modeset risk.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] `wayland-protocols` is added to `crates/howan/Cargo.toml` with the
      `client` feature enabled (and the unstable feature gate the
      `idle-inhibit` zv1 protocol requires), and the crate builds:
      `cargo build` succeeds.
- [x] `HowanApp` binds `zwp_idle_inhibit_manager_v1` from the registry at
      startup (in `HowanApp::new`, `crates/howan/src/app.rs:270`) and stores it
      as an `Option` (so a session without the global degrades gracefully rather
      than failing). The manager is bound through the existing `GlobalList`
      (`globals.bind(...)`), not via a new ad-hoc registry.
- [x] The saver surface holds an inhibitor for its lifetime: `Saver`
      (`crates/howan/src/app.rs:256`) gains a field that owns the
      `zwp_idle_inhibitor_v1`, created against the saver's `wl_surface` when the
      saver is created, so that dropping `Saver` on `dismiss` destroys the
      inhibitor. (Verifiable from the diff: the inhibitor field lives on `Saver`,
      is populated at the surface (re)creation site, and there is no explicit
      `destroy` call needed because `Drop` handles it.)
- [x] When the idle-inhibit manager global is **absent** (e.g. a compositor that
      does not advertise it), the daemon logs a clear diagnostic to stderr once
      and continues to show the saver **without** an inhibitor — it does not
      panic and does not exit. (A unit test covers the "manager is `None` ⇒ no
      inhibitor, no panic" path, since the Wayland round-trip itself can't run in
      CI.)
- [x] Event-less dispatch: `zwp_idle_inhibit_manager_v1` and
      `zwp_idle_inhibitor_v1` are wired with `delegate_noop!` (or equivalent
      empty `Dispatch` impls) alongside the existing `delegate_*!` block
      (`crates/howan/src/app/handlers.rs:387`).
- [x] The composited-surface invariants are preserved: `grep -rn set_fullscreen
      crates/` still returns no call site (comments only), and no opaque region
      is declared on the surface. M3 adds only the inhibitor object.
- [x] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings`
      passes.

### Manual / on-hardware (verified by a human before merge)

- [ ] **DPMS is suppressed while the saver is shown (GNOME).** On a GNOME
      session, set a short GNOME blank/idle timeout
      (e.g. `org.gnome.desktop.session idle-delay` to ~30s) and run `howan daemon`
      with a short `--idle-timeout` (a few seconds). After the saver auto-appears,
      leave the machine idle past the GNOME blank timeout and confirm the display
      **stays physically on** (no DPMS off) for as long as the saver is up. This
      re-evaluates the original Q1 concern: that Mutter actually honors an
      inhibitor created for a non-fullscreen, composited (possibly
      title-barred) surface. Record the result in `40-resident-daemon.md`.
- [ ] **Inhibitor is released on dismiss.** After dismissing the saver with
      input (so no saver is shown and no inhibitor is held), confirm the
      compositor's normal idle blanking resumes — i.e. leaving the machine idle
      now lets the screen blank as usual. This proves the inhibitor's lifetime is
      bound to the surface, not leaked for the life of the daemon.
- [ ] **Blackwell sign-off (SSH-guarded).** Re-run the idle-triggered
      show / suppress-blank / input-dismiss cycle on the actual NVIDIA Blackwell +
      GNOME session **while logged in over SSH from a second machine** (same
      out-of-band guard as M2.5 Stage 2). The change is additive and does not
      alter the surface's scanout eligibility, so the modeset-wedge risk is
      unchanged from PR #3/#4 — but holding DPMS off indefinitely is new
      system-level behavior, so verify on the real machine and record in
      `40-resident-daemon.md` that the display engine was not wedged and the
      screen stayed on as expected. Never launch the first run directly on the
      Blackwell GUI session without the SSH guard.

## Out of scope

- **Releasing the inhibitor for Phase 3 DPMS delegation (M4).** In M3 the
  inhibitor is held for the *entire* time the saver is shown. The 3-phase
  lifecycle — releasing `idle-inhibit` after `T_dpms` so the compositor's
  standard blank takes over (Phase 3), and `loginctl lock-session` on Phase 2
  input — is M4. M3 only proves "screen does not blank while the saver is up".
- **Elapsed-time / phase state tracking** (`T_grace`, `T_dpms`). Not introduced
  here; M3 has a single binary state (saver shown ⇒ inhibited).
- **TOML config and duration-string schema** (M11). No new user-facing knob is
  needed: the inhibitor is held automatically whenever the saver is shown.
- **systemd `--user` unit packaging** (`howan.service`, M10). The daemon stays
  runnable in the foreground for verification.
- **Multi-monitor coverage** (M5) and **GPU / shader rendering** (M6+). The saver
  stays on `wl_shm` and targets the active output only.

## Implementation notes

- **Where the protocol lives.** `zwp_idle_inhibit_manager_v1` /
  `zwp_idle_inhibitor_v1` are in the `wayland-protocols` crate
  (`wayland_protocols::wp::idle_inhibit::zv1::client::{zwp_idle_inhibit_manager_v1,
  zwp_idle_inhibitor_v1}`), not in `wayland-client`. Add `wayland-protocols` with
  the `client` feature; the zv1 protocols sit behind the crate's
  unstable-protocols feature gate (commonly `unstable` / `wp` — check the version
  resolved alongside `wayland-client = "0.31"`). SCTK 0.20 does not provide an
  idle-inhibit helper, so bind and drive the objects directly with
  `wayland-client`.
- **Binding the manager.** Bind it in `HowanApp::new` from the existing
  `GlobalList` (`globals.bind::<ZwpIdleInhibitManagerV1, _, _>(qh, 1..=1, ())`).
  Store `Option<ZwpIdleInhibitManagerV1>` on `HowanApp`; on bind failure (global
  absent) log a single clear stderr line and keep `None` — do **not** fail the
  daemon, since DPMS suppression is best-effort relative to the saver actually
  appearing. The manager object itself has no events.
- **Creating / owning the inhibitor.** Create the inhibitor at the saver
  construction site so its lifetime matches the surface. The cleanest seam:
  give `Saver` an `Option<ZwpIdleInhibitorV1>` field and pass the (optional)
  manager into `Saver::new` (`crates/howan/src/app.rs:450`), calling
  `manager.create_inhibitor(saver.wl_surface(), qh, ())`. `show_saver`
  (`app.rs:301`) already owns the manager via `&self`, so thread it through.
  Because `dismiss` (`app.rs:391`) drops the whole `Saver`, the inhibitor's
  `Drop` sends `destroy` automatically — no explicit teardown, and the
  show→dismiss→show cycle stays correct with no extra state. Per the protocol,
  the inhibitor takes effect once the surface is visible; creating it at surface
  creation (before the first configure) is fine — it simply becomes effective
  when the surface maps.
- **Dispatch.** Both objects are event-less; add
  `delegate_noop!(HowanApp: ignore ZwpIdleInhibitManagerV1);` and
  `delegate_noop!(HowanApp: ZwpIdleInhibitorV1);` next to the existing
  `delegate_*!` block in `crates/howan/src/app/handlers.rs:387`.
- **Docs.** Update `docs/guides/40-resident-daemon.md` with a short section
  documenting that the daemon holds an idle-inhibit lock for the saver's
  lifetime, why (avoid the multi-second DPMS wake on the target hardware), the
  graceful-degradation behavior when the manager global is absent, and the M3
  Stage results from the manual criteria above. Keep it DRY — soft-reference
  `30-composited-surface.md` for the surface invariants rather than restating
  them. Markdown files over ~100 lines need an Overview section (CLAUDE.md).
- **Conventions.** Run `cargo build && cargo test && cargo clippy --all-targets
  -- -D warnings` and fix issues before done. Documentation, code comments,
  commit messages, and the PR description in English. This repository is public —
  cite only public Wayland / Mutter references; do not reference private
  repositories or internal trackers.
