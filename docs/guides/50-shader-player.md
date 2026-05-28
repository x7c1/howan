# WGSL Shader Player

## Overview

This guide explains how howan renders its saver: a single bundled WGSL fragment
shader, animated over time on the GPU through [wgpu], replacing the earlier
solid-black `wl_shm` renderer (milestone M6 — the first time howan shows moving
visuals).

Key points:

- The shader is **compiled into the binary** (`include_str!` of
  `crates/howan/src/app/shader.wgsl`) and built into a wgpu render pipeline at
  runtime. Nothing is read from the filesystem; there is no shader directory,
  playlist, or GLSL/Shadertoy support yet (those are later milestones).
- Two uniforms drive it: `iTime` (seconds since the saver became visible) and
  `iResolution` (`vec3(width, height, width / height)`), mirroring the
  well-known Shadertoy names.
- A **Wayland frame-callback loop** paces the animation: each `wl_surface.frame`
  callback paints the next frame and requests another, so the FPS is capped by
  the compositor (typically vsync) with no busy-loop.
- The GPU path **preserves the composited-surface invariant** — creating the
  wgpu surface adds no `set_fullscreen` and declares no opaque region. The
  shader outputs opaque pixels (alpha 1.0) for appearance, which is a different
  thing from declaring an opaque *region*. See
  [30-composited-surface.md](30-composited-surface.md) for why that distinction
  is safety-critical on NVIDIA Blackwell; that rationale is not repeated here.
- There is **no per-GPU / per-vendor branching**: the wgpu path is the single,
  unconditional renderer for all hardware.

## Architecture

The renderer lives in `crates/howan/src/app/render.rs`, split into two layers
because creating a wgpu device is expensive and the daemon recreates the saver
surface on every idle cycle:

- **`Gpu`** — durable, process-lifetime state held on `HowanApp` behind an `Rc`:
  the wgpu instance / adapter / device / queue, the compiled render pipeline, and
  the single uniform buffer + bind group. Created once in `HowanApp::new`.
- **`Renderer`** — per-surface state, rebuilt each time the saver is shown: the
  wgpu `Surface` wrapping the saver's `wl_surface`, its current size, and a
  shared handle to the durable `Gpu`. Dropped on dismiss.

The three things that can fail do so at three different severities:

- **Building `Gpu`** (the adapter/device request) is **fatal**: if it fails the
  daemon exits non-zero at startup rather than running without a renderer.
- **Building a per-cycle `Renderer`** is **non-fatal**: if surface creation
  fails, `show_saver` logs the error and leaves the saver absent for that cycle.
  The daemon stays resident and re-arms for the next idle period — it does not
  crash or hang.
- **A transient `SurfaceError`** once a frame is running (e.g. `Lost`/`Outdated`
  after a resize) reconfigures the swapchain and skips a single frame; the next
  frame callback paints again, so the loop never stalls.

### Surface creation from the Wayland handles

wgpu needs a `RawDisplayHandle` (the `wl_display`) and a `RawWindowHandle` (the
`wl_surface`) to create its surface. howan derives them from:

- `Connection::backend().display_ptr()` → the `wl_display` pointer, and
- `wl_surface.id().as_ptr()` → the `wl_surface` (`wl_proxy`) pointer.

These accessors require `wayland-client`'s **`system`** feature (which backs
proxy IDs with the real libwayland-client pointers via `wayland-sys`). The
**`dlopen`** feature is also enabled so libwayland is loaded at runtime rather
than linked at build time — building therefore needs only the runtime
`libwayland-client.so.0`, not a `libwayland-dev` / pkg-config setup.

The raw handles are wrapped with `wgpu::Instance::create_surface_unsafe`. The
safety contract (the handles must outlive the surface) is met two ways: the
`Connection` is durable on `HowanApp`, and in `Saver` the `renderer` field is
declared **before** `window`, so the wgpu surface drops before the `wl_surface`
it points at (Rust drops fields in declaration order). That field ordering is
load-bearing — do not reorder it.

## Uniforms

`render::uniforms(elapsed, width, height)` is a pure function (unit-tested
without a GPU or Wayland connection) that computes the uniform block:

- `iTime` = `elapsed.as_secs_f32()`, where `elapsed = now - Saver::shown_at`.
  `shown_at` resets every idle cycle, so the animation restarts from ~0 each
  time the saver appears.
- `iResolution` = `[width, height, width / height]`. The `.z` component is the
  aspect ratio (width over height); the shader uses it to avoid stretching the
  pattern on non-square surfaces. A zero height degrades to a `0.0` aspect
  rather than a non-finite value.

The block is uploaded to the uniform buffer once per frame before the draw.

## The frame-callback loop

Today's saver animates, so it needs a continuous render loop rather than the
old event-driven single paint. howan drives it with Wayland frame callbacks:

1. When the saver is first configured, `HowanApp::draw` paints frame 0 and
   requests a `wl_surface.frame` callback (via `Saver::request_frame_if_idle`).
2. When the callback fires, `CompositorHandler::frame` calls `HowanApp::on_frame`,
   which paints the next frame (advancing `iTime`) and requests another callback.

A `Saver::frame_pending` flag ensures only one callback is outstanding at a time,
so layout events (configure / output changes) that also paint do not stack extra
callbacks and over-drive the loop. The loop is compositor-paced — the compositor
schedules the next callback at its repaint cadence (typically vsync, ~60 Hz) —
which is the FPS cap for this milestone; there is no timer and no busy-loop. The
loop stops on its own when the surface is dropped on dismiss (no more callbacks
are requested). wgpu's `present()` commits the `wl_surface`, replacing the old
`wl_shm` attach + `wl_surface.commit`.

## Manual verification

As with the composited surface, real visual behavior cannot be validated from a
headless build alone. Two stages are defined, mirroring
[30-composited-surface.md](30-composited-surface.md). Because this is the first
GPU-rendering path, **Stage 2 (Blackwell, SSH-guarded) is the gate.**

### Stage 1 — software-rendered / headless

Goal: confirm the wgpu path renders, the frame loop advances `iTime`, and the
composited-surface invariant still holds, on a compositor that cannot trigger
the dangerous Blackwell modeset.

Safety: run against a **software (pixman) weston** and force the **llvmpipe
(lavapipe) Vulkan ICD** so wgpu never touches the real GPU:

```bash
# A software weston on its own socket (headless, or nested with the wayland
# backend for a real visible window). Never the real session's wayland-0 as
# howan's target.
weston --backend=headless-backend.so --renderer=pixman \
  --width=1920 --height=1080 --socket=wayland-stage1 &

# Force the software Vulkan ICD so wgpu uses llvmpipe, not the NVIDIA GPU.
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
WAYLAND_DISPLAY=wayland-stage1 WAYLAND_DEBUG=1 \
  timeout 6 cargo run 2>wire.log
grep -iE 'set_fullscreen|set_opaque_region' wire.log \
  && echo "UNEXPECTED" || echo "ok: neither on the wire"
```

**Status: PASSED (protocol + loop mechanism), 2026-05-28.** Verified on
`weston` 14 (pixman / CPU renderer) with wgpu pinned to the llvmpipe Vulkan ICD
(`lvp_icd.json`), so no NVIDIA code path was exercised. Observed:

- wgpu created its Vulkan surface over the saver's `wl_surface` (Mesa Vulkan WSI
  engaged) and rendered; the shader pipeline compiled and presented.
- The frame-callback loop advanced: a continuous sequence of distinct
  `wl_surface.frame` → `wl_callback.done` ticks, with `iTime` increasing across
  frames (e.g. `0.0002` → `0.015` → …) — i.e. the animation moves, it is not a
  static frame.
- The wire trace showed **no** `xdg_toplevel.set_fullscreen` and **no**
  `wl_surface.set_opaque_region`; the toplevel was pinned to the advertised
  output mode (`set_min_size` / `set_max_size` 1920×1080). `SIGTERM` unwound the
  clean-exit path.
- Caveat: a *nested* weston window that GNOME occludes gets its frame callbacks
  throttled (the compositor stops scheduling repaints for a hidden window), so a
  sustained ~60 Hz cadence and the on-screen visual are best confirmed on a
  foreground/visible surface — which is part of Stage 2. The top-most / titlebar
  coverage limitation is the standing Q2 limitation from
  [30-composited-surface.md](30-composited-surface.md), not a regression here.

### Stage 2 — Blackwell sign-off (SSH-guarded)

Goal: since the target machine is NVIDIA Blackwell and this is the first time
howan issues real GPU rendering commands (wgpu device + per-frame draws), confirm
the animated saver renders and dismisses on input on the **actual Blackwell +
GNOME/Mutter** session **without wedging the display engine / GSP firmware**.

Follow the SSH-guard procedure in
[30-composited-surface.md](30-composited-surface.md#stage-2--blackwell-sign-off-ssh-guarded)
exactly (log in over SSH from a second device first; never launch directly on the
Blackwell GUI session). This is the same recovery channel, and a nested
compositor is **not** a safe substitute because it shares the GPU. Run the
animated saver (do **not** force the llvmpipe ICD here — Stage 2 exercises the
real NVIDIA path under the SSH guard), then record:

- Whether the saver rendered the animated shader and dismissed on input within
  ~100 ms, with the per-frame loop running.
- Whether the daemon re-arms across idle cycles (dismiss → next idle re-shows
  the animation with `iTime` restarting), and the `DpmsHandoff` blank still works.
- Any `nvidia-modeset` / `GSP` errors in the captured kernel log (there must be
  none, as in the M-series composited-surface run).

**Status: OUTSTANDING — this is the gate for M6.** The SSH-guarded Blackwell run
has not been performed in this change. Do not treat M6 as fully verified until it
is done and recorded here. The composited path is expected to keep the GPU
rendering safe (the surface stays an ordinary composited window, never a scanout
candidate), but that remains a hypothesis for the GPU-rendering path until this
run confirms it.

[wgpu]: https://wgpu.rs/
