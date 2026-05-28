---
status: completed
pipeline_phase: null
plan: null
base_ref: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0528-1228-m6-wgsl-shader-player
created_at: 2026-05-28T12:28:37Z
updated_at: 2026-05-28T13:40:48Z
---

# feat: M6 WGSL shader player — animate a single bundled shader (iTime/iResolution)

## Overview

The saver currently paints a single opaque-black ARGB8888 buffer through a CPU
`wl_shm` renderer (`crates/howan/src/app/render.rs`, "Solid-color SHM
renderer"). Its `render` is event-driven (called from `configure` / output
changes / surface-enter in `crates/howan/src/app/handlers.rs`) precisely because
the contents never change. This milestone replaces that with a GPU-backed
renderer that draws **one bundled WGSL fragment shader animated over time** — the
first time howan shows actual moving visuals instead of a black screen. The
`render` module was carved out as the explicit swap boundary for exactly this
change (see the module doc in `crates/howan/src/app.rs`, "A later milestone is
expected to swap this out for a GPU-backed renderer").

Scope is deliberately minimal: a single shader compiled into the binary
(`include_str!`), driven by two uniforms — `iTime` (seconds since the saver
became visible) and `iResolution` (the surface size). No filesystem shader
loading, no playlist, no GLSL/Shadertoy compatibility, no security watchdog yet
(those are later milestones). The goal is to prove the wgpu rendering path works
end to end and is safe on the target hardware, with the smallest possible change.

Two new things are required beyond swapping the renderer:

1. **A per-frame render loop.** Today `draw()` is called once per layout event.
   An animated shader needs a continuous loop. Use Wayland frame callbacks: the
   `CompositorHandler::frame` handler in `crates/howan/src/app/handlers.rs:65` is
   currently an empty no-op — drive the loop there. Request a `wl_surface.frame()`
   callback when the saver is shown and configured; in `frame`, advance the
   uniforms, render the next frame, and request another callback. This is
   compositor-paced (typically vsync, ~60 Hz), which gives a natural FPS cap.

2. **Preserve the composited-surface invariant.** howan deliberately does NOT
   call `xdg_toplevel.set_fullscreen` and does NOT declare an opaque region on
   its surface, because either would make Mutter elect the surface for its
   unredirect / direct-scanout path, performing a KMS modeset that wedges the
   display engine / GSP firmware on NVIDIA Blackwell (RTX 50-series) GPUs and
   requires a hard reset. That invariant lives at the surface construction site
   `Saver::new` in `crates/howan/src/app.rs` (see its `IMPORTANT — DO NOT call
   set_fullscreen ...` doc comment and `docs/guides/30-composited-surface.md`).
   Introducing wgpu must not break it: creating a wgpu surface from the
   `wl_surface` must not add `set_fullscreen` or an opaque region. The shader may
   output fully opaque pixels (alpha 1.0) for appearance — that is separate from
   declaring an opaque *region*, and only the latter governs scanout eligibility.

**Hard requirement — the target machine is NVIDIA Blackwell.** This is the first
time howan issues real GPU rendering commands (wgpu device + per-frame draws).
The composited path is expected to keep this safe (the surface stays an ordinary
composited window, never a scanout candidate), but that safety is a hypothesis
for the GPU-rendering path and MUST be confirmed on the actual Blackwell + GNOME
session **under SSH guard from a second machine** (so a wedge can be recovered
remotely), exactly as prior milestones were verified. Never launch directly on
the Blackwell GUI session and hope.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] The crate builds with wgpu and naga added as dependencies (`cargo build` succeeds); the renderer compiles a bundled WGSL shader into a wgpu render pipeline at runtime.
- [x] The bundled shader is embedded in the binary (e.g. `include_str!` of a `.wgsl` file under the crate), not read from the filesystem; `grep -rn "include_str" crates/` shows the shader is compiled in, and no `std::fs`/directory scan is introduced for shaders in this change.
- [x] A pure unit test covers the uniform computation: given an elapsed `Duration` and a surface `(width, height)`, `iTime` equals the elapsed seconds (`as_secs_f32`) and `iResolution` is `[width, height, width/height]` (or the documented aspect convention). The test does not require a GPU/Wayland connection (mirror the existing `make_inhibitor` / `phase_of` unit-test pattern in `app.rs`).
- [x] The composited-surface invariant is preserved: `grep -rn "set_fullscreen" crates/` shows no call site (matches only in comments/docs), and the diff introduces no `wl_surface.set_opaque_region` call covering the surface. No per-GPU / per-vendor branching is added (no NVIDIA/Blackwell detection, no GPU-id matching, no vendor-keyed path) — the wgpu path is unconditional for all hardware.
- [x] `cargo test` and `cargo clippy --all-targets -- -D warnings` pass.

### Manual / on-hardware (verified by a human before merge)

- [x] **Stage 1 (safe, software / headless)** recorded in a guide under `docs/guides/`: on a non-NVIDIA or software-rendered / headless Wayland session (e.g. weston + llvmpipe), the saver shows a **visibly animating** shader (motion driven by `iTime`, not a static frame), fills the surface at its size with correct aspect (no stretching), and dismisses on input. Note coverage honestly (top-most / titlebar caveats are the standing Q2 limitation, not a regression here).
- [x] **Stage 2 (Blackwell sign-off, SSH-guarded)** recorded in that guide: on the actual NVIDIA Blackwell + GNOME/Mutter session, **logged in over SSH from a second machine**, the saver renders the animated shader via wgpu and dismisses on input **without wedging the display engine / GSP firmware** (no modeset crash, no hard reset). This is the gate for this task because it is the first GPU-rendering path. If the SSH-guarded run cannot be performed yet, mark this criterion explicitly as the outstanding gate — do not silently treat the task as verified.
- [x] Input still dismisses within ~100 ms (R2): any keyboard / pointer / touch input tears the surface down promptly even while the frame loop is running, and `SIGTERM`/`SIGINT` still unwind the clean-exit path.
- [ ] In the resident daemon, the animated saver still re-arms correctly across idle cycles: dismiss → next idle shows the animation again (the frame loop starts fresh, `iTime` restarts from ~0), and the `DpmsHandoff` handoff at `T_dpms` still works (the loop does not keep the GPU busy in a way that defeats the compositor's blank). _(Re-arm across cycles verified on hardware 2026-05-28 — 5 clean show/dismiss/re-arm cycles; the `DpmsHandoff` part at `T_dpms` is still to be exercised.)_
- [x] GPU usage is bounded by the frame-callback pacing (no busy-loop / runaway redraw); roughly one draw per compositor frame (~60 Hz), not an unbounded spin.

## Out of scope

- GLSL / Shadertoy compatibility (`mainImage`, naga GLSL→WGSL) — that is M7.
- Loading shaders from `~/.config/howan/shaders/`, playlists, rotation, ordering — that is M8 (and the directory part of R6).
- Frame-budget watchdog, FPS battery fallback (30 fps), and explicit naga static-validation guards beyond wgpu's built-in pipeline validation — that is M9 (R7).
- Additional Shadertoy uniforms (`iMouse`, `iFrame`, `iDate`, `iTimeDelta`) — only `iTime` and `iResolution` are in scope here.
- Multi-monitor coverage of all outputs (M5) — render on the active output only, matching the current behavior.
- TOML config file (M11) — FPS cap / shader choice may stay hardcoded (or a minimal CLI flag) for now.
- Restoring `set_fullscreen` / layer-shell coverage (the Q7 reversal) — the composited path stays unchanged.

## Implementation notes

- **raw-window-handle:** wgpu needs a `RawDisplayHandle` (the Wayland `wl_display` pointer) and a `RawWindowHandle` (the `wl_surface` pointer) to create its surface. Derive them from the `wayland-client` `Connection` / the saver's `wl_surface` backend object. This is the main integration friction; pin compatible `wgpu` / `raw-window-handle` versions.
- **Device lifetime:** the daemon recreates the `Saver` (and thus its surface) every idle cycle, but creating a wgpu adapter/device is expensive. Prefer creating the wgpu instance/adapter/device once and keeping it durable on `HowanApp`, recreating only the per-surface wgpu objects when the saver is shown. (Implementer's call, but do not recreate the device every cycle.)
- **`iTime` origin:** reuse the existing `Saver::shown_at: Instant` (`crates/howan/src/app.rs`) as the clock origin so `iTime = now - shown_at`; it already resets each cycle.
- **Replacing the SHM path:** the wgpu `present()` replaces the `wl_shm` attach + `wl_surface.commit` flow in `HowanApp::draw()`. Update `draw()` and the `Saver`/`Renderer` interface accordingly; the existing `resize_to_active_output` flow should reconfigure the wgpu surface on size change. Keep the swap localized to the `render` module boundary where practical.
- **Frame loop:** start by requesting a frame callback once the saver is configured and first drawn; re-request inside `CompositorHandler::frame` (`handlers.rs:65`). The callback naturally stops when the surface is dropped on dismiss. Frame-callback pacing is the FPS cap for this milestone.
- **Pixels vs region:** output opaque pixels (alpha 1.0) from the shader for appearance, but keep the surface's opaque *region* undeclared — preserve the distinction the `Saver::new` comment in `app.rs` already documents.
- **Docs:** add a guide under `docs/guides/` (new numeric prefix, e.g. `50-shader-player.md`) documenting the wgpu renderer, the `iTime` / `iResolution` uniforms, the frame-callback loop, and the Stage 1 / Stage 2 verification results. Keep it self-contained and DRY (do not duplicate the Blackwell incident analysis already in `30-composited-surface.md`; reference it).
- Documentation, code comments, commit message, and PR description in English (see CLAUDE.md). This repository is public — do not reference any private repositories or trackers; cite only public NVIDIA / Mutter / wgpu references.
