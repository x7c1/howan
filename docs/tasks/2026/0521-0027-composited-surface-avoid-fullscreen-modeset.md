---
status: completed
pipeline_phase: null
plan: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0521-0027-composited-surface-avoid-fullscreen-modeset
created_at: 2026-05-20T15:27:24Z
updated_at: 2026-05-20T16:14:13Z
---

# refactor: cover the output via a composited surface instead of set_fullscreen

## Overview

The saver currently makes its window fullscreen by calling
`window.set_fullscreen(None)` in `crates/howan/src/app.rs` (around line 115).
On a real GNOME/Mutter Wayland session this is dangerous: a fullscreen surface
becomes a candidate for Mutter's *fullscreen unredirect / direct scanout*
optimization, which performs a KMS plane/mode reconfiguration when the surface
maps. On NVIDIA Blackwell (RTX 50-series) that modeset wedges the GPU display
engine / GSP firmware and requires a hard reset. This was hit during the M2
manual verification and is recorded in `docs/guides/20-swayidle.md` (the
"Incident: full system lockup on NVIDIA Blackwell" section). It is an NVIDIA
driver/GSP bug, not a howan logic defect — but howan's fullscreen request is the
trigger, so howan must stop pulling that trigger.

Mutter's documented rule is that only **opaque** surfaces, or the **transparent
surface of a fullscreen window**, are eligible for unredirect / direct scanout
(see GNOME/mutter commit history and merge request !798 for the
`window-actor/wayland` scanout gating). The corollary: a surface that is
**neither fullscreen nor declared opaque** is never elected for that path, so it
stays on the normal composited path and no risky modeset happens.

This task switches the saver to that safe path:

1. Stop calling `set_fullscreen`. Instead size the surface to cover the active
   output's current mode, as an ordinary `xdg_toplevel`.
2. Do **not** declare an opaque region on the surface (`wl_surface.set_opaque_region`).
   Leaving the surface non-opaque is what keeps Mutter from electing it for
   unredirect / scanout. Document this deliberate choice in a code comment so a
   future contributor does not "optimize" it back by adding an opaque region.

The visual goal (does it actually cover the screen / stay on top) is **not**
fully guaranteed by this approach — Mutter has no `wlr-layer-shell`, so top-most
coverage is a separate open question. Verifying real coverage is a manual check
(see acceptance criteria).

**Hard requirement — the target machine is NVIDIA Blackwell.** The whole point
of this change is that the saver must actually run on a Blackwell + GNOME/Mutter
Wayland desktop without wedging the GPU. "Refuse to run on Blackwell" or
"silently fall back to a blank screen on Blackwell" is **not** an acceptable end
state. The composited-surface approach is the mechanism that makes it safe to
run there (it reduces the saver to an ordinary composited window, which does not
trigger the fullscreen modeset that crashed the GPU). Because that safety is a
hypothesis, the real Blackwell verification must be done under SSH guard (see the
final acceptance criterion), never by launching directly on the Blackwell GUI
session and hoping.

**Design principle — no per-GPU / per-vendor branching.** Do **not** detect
whether the GPU is NVIDIA / Blackwell and switch behavior on it. There must be
no `if blackwell { … }`-style code, no GPU/vendor detection utility, and no
environment-specific fallback selection. The composited-surface path is correct
and safe on every GPU, so it is the single, unconditional drawing path for all
hardware. Keeping one code path (not a matrix of hardware-specific behaviors) is
an explicit goal of this task.

**This is a reversible workaround, not the ideal design.** The ideal design uses
`set_fullscreen` (or a layer-shell overlay) to *guarantee* full coverage and
top-most stacking. The composited path sacrifices that guarantee to dodge the
Blackwell modeset crash; it is a temporary workaround, not the permanent answer.
A future maintainer must be able to trace *why* `set_fullscreen` was removed and
*when* to restore it. Capture this in the durable, self-contained records (the
code comment at the former `set_fullscreen` site and the guide) — not only in
this one-shot task. **Restoration condition:** restore the `set_fullscreen`-based
design once (1) an upstream NVIDIA driver / GSP-firmware release fixes the
Blackwell modeset crash, **and** (2) the SSH-guarded Blackwell run (Stage 2
below) re-confirms a fullscreen surface no longer wedges the GPU.

## Acceptance criteria

- [ ] `set_fullscreen` is no longer called anywhere in the crate (`grep -rn set_fullscreen crates/` returns no call site; the only matches allowed are in comments/docs explaining why it is avoided)
- [ ] The saver surface is sized to the active output's current mode dimensions (queried via the bound `OutputState` / `wl_output` geometry) rather than left at the hardcoded `INITIAL_WIDTH`/`INITIAL_HEIGHT` fallback as its final size; the fallback constants may remain only as the pre-configure starting allocation
- [ ] The code does not call `wl_surface.set_opaque_region` with a region covering the surface, and a comment at the surface-setup site explains that the surface is intentionally left non-opaque to avoid Mutter's unredirect / direct-scanout modeset (cite the NVIDIA Blackwell incident)
- [ ] No per-GPU / per-vendor branching is introduced: there is no NVIDIA/Blackwell detection, no `if`-on-GPU drawing path, and no hardware-specific fallback selection. The composited-surface path is unconditional for all hardware (verifiable by inspecting the diff — no `/sys/class/drm` vendor probing, no GPU-id matching, no vendor-keyed config switch)
- [ ] The M1 dismiss behavior is preserved: any keyboard, pointer, or touch input still exits the saver with status 0, and `SIGTERM`/`SIGINT` (the `howan stop` path) still unwind through the same clean-exit path
- [ ] A guide under `docs/guides/` (numeric-prefix convention, distinct from the existing `10-` guide) documents the avoidance design: why `set_fullscreen` is not used, why the surface is left non-opaque, and the manual safe-hardware verification result
- [ ] The guide is **self-contained on `main`**: it does not hard-link to `20-swayidle.md` as if that file exists (it is the unmerged M2 deliverable and is absent from `main`; this composited-surface change is expected to land first so that M2's manual verification becomes safe). State the few incident facts the guide actually needs inline (GPU model, the one-line crash cascade, "NVIDIA GSP-firmware bug, hard reset required"); any pointer to `20-swayidle.md` must be a soft "see also (added by the swayidle integration work)" that does not break when the file is absent. Do not duplicate the full incident analysis (DRY) — only the minimum the guide needs to stand alone
- [ ] **Reversibility is recorded in durable, self-contained places.** Both (a) the code comment at the former `set_fullscreen` site in `crates/howan/src/app.rs` and (b) the guide state plainly that this is a **temporary workaround**, that the **ideal design uses `set_fullscreen`** (for guaranteed coverage / top-most), and the **restoration condition**: restore `set_fullscreen` once an upstream NVIDIA/GSP fix for the Blackwell modeset crash ships **and** the SSH-guarded Blackwell run (Stage 2) re-confirms a fullscreen surface is safe. The guide has a dedicated section for this (e.g. "Restoration path — when to return to set_fullscreen")
- [ ] **Stage 1 (safe, protocol-level)** recorded in that guide: on a **non-NVIDIA or software-rendered / headless Wayland** session (e.g. virtio-gpu, llvmpipe, weston pixman), confirm the saver no longer issues `set_fullscreen`, stays on the composited path, appears, and dismisses on input. Note honestly whether it covered the output / stayed on top (connects to the unresolved top-most question)
- [ ] **Stage 2 (Blackwell sign-off, SSH-guarded)** recorded in that guide: since the target machine is NVIDIA Blackwell and running there is a hard requirement, the real verification is to run the saver on the actual Blackwell + GNOME/Mutter session **while logged in over SSH from a second machine** (so the GPU wedging seen previously can be recovered via remote log capture / kill / reboot). Record whether the saver displayed and dismissed without wedging the display engine. **Never** launch it directly on the Blackwell GUI session without that SSH guard. If the SSH-guarded run cannot be performed yet, mark this criterion explicitly as the outstanding gate (do not silently treat the task as fully verified)

## Out of scope

- DPMS-delegation as part of the Phase 3 lifecycle (releasing `idle-inhibit` after `T_dpms`) — that is M3/M4 lifecycle work, unrelated to this change
- A guaranteed top-most / full-coverage mechanism (no `wlr-layer-shell` on Mutter; this is a standing open question, not solvable here)
- Multi-monitor coverage of all outputs (M5) — this task targets the active output only, matching the current M1/M2 behavior
- Any GPU/shader rendering (M6+); the renderer stays on `wl_shm`
- Compositor-side unredirect-disable workarounds (e.g. GNOME extensions) — those belong in setup documentation, not in howan's runtime behavior

## Implementation notes

- The active output's mode size is available through SCTK's `OutputState`; pick the output the surface is shown on (active output, matching M1). If output info is not yet available at startup, keep the existing initial allocation and resize on the first `configure`/output event rather than blocking.
- A normal `xdg_toplevel` may get server-side decorations (titlebar) from the compositor since it is no longer fullscreen. Decide and document how the saver suppresses chrome (e.g. request no/zero decorations) so it does not show a titlebar; keep this minimal.
- Keep the existing `wl_shm` ARGB8888 renderer. Note that the buffer is filled with `0xFF` alpha (opaque black) for appearance, but **no opaque region is declared on the surface** — these are different things, and only the latter governs Mutter's scanout eligibility. Make sure a comment makes this distinction clear.
- Documentation, code comments, commit messages, and PR description in English (see CLAUDE.md). This repository is public; do not reference any private repositories or external/internal trackers other than the public NVIDIA/Mutter references already cited in `20-swayidle.md`.
