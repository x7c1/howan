# Shader Player

## Overview

This guide explains how howan renders its saver: a fragment shader animated over
time on the GPU through [wgpu], replacing the earlier solid-black `wl_shm`
renderer. The **default** is a single WGSL shader compiled into the binary
(milestone M6 — the first time howan shows moving visuals). M7 adds a second
input language — **GLSL written to the Shadertoy convention** — so a shader
copy-pasted from [Shadertoy] runs in howan via `--shader <path>` (see
[GLSL / Shadertoy shaders](#glsl--shadertoy-shaders)).

Key points:

- The default shader is **compiled into the binary** (`include_str!` of
  `crates/howan/src/app/shader.wgsl`) and built into a wgpu render pipeline at
  runtime. When `--shader` is not given, nothing is read from the filesystem.
  There is no shader directory, playlist, or rotation yet (that is a later
  milestone); `--shader <path>` loads exactly one file.
- The full **Shadertoy-style uniform set** drives it — `iResolution`, `iTime`,
  `iTimeDelta`, `iFrame`, `iMouse`, `iDate` — mirroring the well-known Shadertoy
  names so a pasted shader links. `iMouse` is **always zero** (the saver is
  idle and does not track the pointer). See
  [Uniforms](#uniforms) for each field's howan value.
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
- The GPU path is loaded at runtime, so it adds a runtime dependency on the
  Vulkan loader and the GPU driver's ICD. A normal system-toolchain build finds
  both through the FHS defaults; a non-FHS-toolchain build (e.g. Nix on an FHS
  distro) needs an opt-in systemd drop-in — see
  [GPU runtime libraries on a non-FHS build](#gpu-runtime-libraries-on-a-non-fhs-build).

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

`render::uniforms(...)` computes the uniform block; it is pure and unit-tested
without a GPU or Wayland connection (the wall-clock `iDate` is the one field
supplied by the caller, `render::i_date_now`, since it is not a pure function of
the inputs). The block is uploaded to the uniform buffer once per frame before
the draw, and is shared by the WGSL and GLSL paths — the same buffer, the same
field order. The fields and their howan values:

- `iResolution` (`vec3`) = `[width, height, width / height]`. The `.z` component
  is the aspect ratio (width over height); the shader uses it to avoid
  stretching the pattern on non-square surfaces. A zero height degrades to a
  `0.0` aspect rather than a non-finite value.
- `iTime` (`float`) = `elapsed.as_secs_f32()`, where `elapsed = now -
  Saver::shown_at`. `shown_at` resets every idle cycle, so the animation
  restarts from ~0 each time the saver appears.
- `iTimeDelta` (`float`) = seconds since the previous presented frame (`0` on
  the first frame of a cycle).
- `iFrame` (`int`) = frame counter since the saver became visible (`0` on the
  first frame of a cycle).
- `iMouse` (`vec4`) = **always `[0, 0, 0, 0]`**. The saver is a passive idle
  overlay and does not track the pointer; the uniform is provided only so a
  pasted Shadertoy shader that reads it still links.
- `iDate` (`vec4`) = `(year, month, day, seconds-since-midnight)` from the local
  wall clock at frame time.

### Layout discipline

The Rust `Uniforms` struct (`render.rs`), the WGSL `struct Uniforms`
(`shader.wgsl`), and the Shadertoy GLSL uniform block (the prelude in
`shader.rs`) must stay field-for-field identical, including std140 padding. The
byte offsets (vec3+float | float+int+2 floats of padding | vec4 | vec4 = 64
bytes) are documented on `render::Uniforms`; a `uniforms_struct_matches_std140_layout`
unit test pins the size. Adding or reordering a field means editing all three.

## GLSL / Shadertoy shaders

[Shadertoy] shaders are GLSL fragment shaders that define

```glsl
void mainImage(out vec4 fragColor, in vec2 fragCoord) { ... }
```

rather than a GLSL `main`. howan accepts one via `--shader <path>` on both the
`daemon` and `start` subcommands, choosing the pipeline by file extension:
`.wgsl` → WGSL, `.glsl` / `.frag` → GLSL. Without `--shader`, the bundled WGSL
shader plays (unchanged from M6).

How a GLSL file reaches the GPU (in `crates/howan/src/app/shader.rs`):

1. **Single-pass guard.** A source that references a texture/audio channel
   (`iChannel0..3`) is rejected with a clear, typed error. howan supports only a
   single-pass `mainImage`; Shadertoy multi-buffer shaders (Buffer A/B/C/D) and
   channel inputs are out of scope.
2. **mainImage wrapper.** howan prepends the Shadertoy uniform block + a fragment
   output, and appends a synthesized `main` that calls `mainImage` with the pixel
   coordinate, then writes its `fragColor` to the output. naga only ever sees
   standard GLSL with a real `main`.
3. **y-flip.** Shadertoy's `fragCoord` origin is bottom-left; wgpu/Vulkan
   framebuffer coordinates are top-left, so the wrapper flips y
   (`iResolution.y - gl_FragCoord.y`) before calling `mainImage`. Without this a
   pasted shader would render upside-down.
4. **Parse + validate.** The wrapped source is parsed by `naga::front::glsl` to a
   `naga::Module` and validated with `naga::valid::Validator` — the **same** naga
   IR + validation the WGSL path goes through, so GLSL never reaches the driver as
   raw text. The validated module is handed to wgpu via `ShaderSource::Naga`. The
   vertex stage stays the bundled WGSL full-screen triangle; only the fragment
   stage comes from the GLSL file.

This shares the M6 frame-callback loop, device lifetime, and uniform buffer
unchanged; see [The frame-callback loop](#the-frame-callback-loop).

### Resilient fallback

A `--shader` file that fails to read, parse, or validate (a syntax error, an
unrecognized extension, or an `iChannel0` reference) does **not** crash the
daemon: the failure is logged with a clear error and the daemon falls back to the
bundled WGSL shader. This previews the M8 "empty/missing directory → bundled
fallback" behavior without implementing the directory scan.

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

**Status: PASSED — Blackwell sign-off complete (2026-05-28).** Verified on an
NVIDIA GeForce RTX 5060 Ti (Blackwell) + GNOME/Mutter session, under SSH guard
from a second device. The selected adapter was logged as `device_type:
DiscreteGpu, backend: Vulkan, NVIDIA` (the real GPU, not a software fallback).

- `howan start`: the animated shader rendered full-screen and dismissed on
  input, with no display-engine wedge and no `nvidia-modeset` / GSP crash.
- `howan daemon --idle-timeout 10`: five idle -> show -> input-dismiss -> re-arm
  cycles ran back to back; the animation re-rendered on every cycle (the
  per-surface wgpu objects are rebuilt each idle cycle) with no errors, and
  `SIGTERM` shut the daemon down cleanly (logged `daemon shutting down`, exit 0).
- Two defects surfaced only on the real display and were fixed in this change:
  the device requested the downlevel limits preset (2048 max texture dimension),
  too small for the 5120x2160 output, so it now adopts the adapter's own limits;
  and the `wl_surface.frame` callback was requested after `present()` (after the
  commit), so the loop stalled after a single frame — it is now requested before
  `present()`.

Not yet exercised on hardware: the `DpmsHandoff` transition at `T_dpms` (it
reuses the M3/M4 inhibitor + handoff logic plus the M6 surface retention).
Verify when convenient; it is independent of the GPU-rendering path proven above.

(The dev binary was built with a non-FHS toolchain, so the run set
`LD_LIBRARY_PATH` + `VK_ICD_FILENAMES` to reach the system Vulkan loader and
NVIDIA ICD. That is a build-environment detail, not part of the saver — see the
next section for the opt-in way to make this stick for the installed daemon.)

### GLSL / Shadertoy verification (M7)

The two stages above proved the M6 WGSL bring-up. M7 adds the GLSL path; it
reuses the *same* surface / scanout path (no `set_fullscreen`, no opaque region),
so the Blackwell modeset-wedge risk is unchanged — Stage 2 here is a sanity
check, not a first-ever GPU bring-up. Both stages use a real single-pass
Shadertoy shader. A ready one ships at
[`examples/shaders/shadertoy-rainbow.glsl`](../../examples/shaders/shadertoy-rainbow.glsl)
(it runs unchanged on shadertoy.com too); it has a vertical brightness gradient
that is dark toward the **bottom** of the screen, so the orientation (no
vertical flip) is verifiable by eye.

**Stage 1 (safe, software / headless).** Run the GLSL shader on a software
(pixman) weston with the llvmpipe Vulkan ICD forced, exactly as in Stage 1
above, but pass `--shader`:

```bash
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
WAYLAND_DISPLAY=wayland-stage1 \
  timeout 6 cargo run -- start --shader examples/shaders/shadertoy-rainbow.glsl
```

Confirm: the shader renders and **visibly animates** (the rainbow drifts with
`iTime`), the gradient is darker at the bottom (correct orientation, not flipped)
and the aspect is not stretched, and input dismisses it.

**Status: NOT YET RUN.** Outstanding gate — record the observed result, the exact
shader used, and the weston/ICD versions here once performed.

**Stage 2 (Blackwell sanity check, SSH-guarded).** Run the same GLSL shader on
the actual NVIDIA Blackwell + GNOME/Mutter session, **logged in over SSH from a
second machine** (follow the SSH-guard procedure referenced in Stage 2 above; do
not force the llvmpipe ICD here). Confirm it renders via wgpu without wedging the
display engine / GSP firmware, dismisses on input within ~100 ms, and that
`SIGTERM`/`SIGINT` still unwind the clean-exit path. Also confirm once by hand
that a malformed or channel-using GLSL file (e.g. one referencing `iChannel0`) is
rejected with a readable error and the daemon falls back to the bundled shader
rather than crashing.

**Status: NOT YET RUN.** Outstanding gate — lower-risk than the M6 Stage 2
(surface/scanout path unchanged), but confirm rather than assume; record the
result here once performed.

## GPU runtime libraries on a non-FHS build

The wgpu renderer loads the Vulkan loader (`libvulkan.so.1`) and the GPU driver
(via the Vulkan ICD) at runtime, not at build time. How those are found depends
on the toolchain the binary was built with. "FHS" below is the Filesystem
Hierarchy Standard — the conventional `/usr/lib`, `/usr/share`, … layout that
mainstream distros (Ubuntu, Fedora, …) follow and that the system loader
searches by default; Nix deliberately does not follow it.

- **Normal system-toolchain build** (the binary `cargo install` produces with
  your distro's toolchain): no configuration is needed. The dynamic loader
  finds `libvulkan.so.1` through the FHS `ld.so` cache, and the Vulkan loader
  finds the GPU's ICD manifest in its default search dir
  (`/usr/share/vulkan/icd.d`). The shipped unit and `make install` set no GPU
  environment variables, and none are required.

- **Non-FHS-toolchain build on an FHS distro** (the concrete case being a
  Nix-toolchain build on Ubuntu): the binary uses a dynamic loader that does
  not search the FHS paths, so it cannot find `libvulkan.so.1` or the GPU
  driver's ICD. The daemon then fails to find a wgpu adapter unless it is told
  where to look via `LD_LIBRARY_PATH` (the FHS lib dir) and `VK_ICD_FILENAMES`
  (the GPU's ICD manifest). This is a build-environment mismatch, not a defect
  in howan, and it affects only that unusual setup.

The fix for the installed `--user` daemon is an **opt-in systemd drop-in**, not
a change to the shipped unit. The values are distro- and vendor-specific (the
multiarch lib dir name differs across distros; an NVIDIA ICD path breaks
AMD/Intel users), so baking them into `make install` would break most
environments. Instead, copy the example fragment and adapt it:

```bash
mkdir -p ~/.config/systemd/user/howan.service.d
cp packaging/systemd/howan.service.d/override.conf.example \
   ~/.config/systemd/user/howan.service.d/override.conf
# edit the copy with your machine's paths, then:
systemctl --user daemon-reload && systemctl --user restart howan.service
```

The example
([`packaging/systemd/howan.service.d/override.conf.example`](../../packaging/systemd/howan.service.d/override.conf.example))
is `*.example`, not `*.conf`, so systemd never auto-loads it; it takes effect
only once copied to `override.conf` as above. Its comments show how to discover
the correct `LD_LIBRARY_PATH` and `VK_ICD_FILENAMES` values for your machine.

To confirm the override took effect, watch the daemon's journal as it next
renders the saver (trigger an idle cycle or run `howan start`):

```bash
journalctl --user -u howan.service -f
```

A working override logs `wgpu adapter selected` with `device_type: DiscreteGpu`
(your real GPU, e.g. `backend: Vulkan, NVIDIA`). `no suitable wgpu adapter
found` means the dynamic loader still cannot reach the Vulkan loader or ICD — re-check
the two paths in your `override.conf`. A `device_type: Cpu` line means it fell
back to a software rasterizer (llvmpipe): rendering works but on the CPU, so the
GPU paths are still not being found.

[wgpu]: https://wgpu.rs/
[Shadertoy]: https://www.shadertoy.com/
