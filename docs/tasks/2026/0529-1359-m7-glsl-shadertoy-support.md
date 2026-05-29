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
branch: task/0529-1359-m7-glsl-shadertoy-support
created_at: 2026-05-29T13:59:36Z
updated_at: 2026-05-29T14:44:30Z
---

# feat: M7 GLSL (Shadertoy-compatible) shader support

## Overview

M6 plays a single WGSL fragment shader: the renderer embeds `shader.wgsl`
(`crates/howan/src/app/render.rs`, `include_str!` at `SHADER_SOURCE`) and builds
it into a wgpu pipeline via `wgpu::ShaderSource::Wgsl(...)` (`render.rs:161`).
This milestone adds a **second input language — GLSL written to the Shadertoy
convention** — so a shader copy-pasted from Shadertoy runs in howan. The WGSL
path and the bundled WGSL shader stay as-is; M7 only *adds* a GLSL entry point.

How GLSL reaches the GPU (stated here so this task is self-contained): shader
text never goes to the GPU directly. naga
(wgpu's bundled translator, already a direct dependency — see
`crates/howan/Cargo.toml`, `naga = "22"`, declared "so later milestones can
parse/validate shaders") parses the source into its own IR, validates it, and a
backend emits the GPU form (SPIR-V under Vulkan). WGSL and GLSL both pass through
that same naga IR + validation, which is why routing GLSL through naga is safer
than handing raw GLSL to a driver. Two viable wirings — pick one:

1. Enable wgpu's `glsl` feature and use `wgpu::ShaderSource::Glsl { shader,
   stage, defines }`, or
2. Parse with `naga::front::glsl` to a `naga::Module` yourself and feed
   `wgpu::ShaderSource::Naga(...)`.

Option 2 keeps naga explicit (it is already the direct dep) and gives direct
access to the validation result, which M9 will build on; either is acceptable.

Three things this milestone must do:

1. **Accept the Shadertoy `mainImage` convention.** Shadertoy shaders define
   `void mainImage(out vec4 fragColor, in vec2 fragCoord)`, not a GLSL `main`.
   Wrap it: synthesize the real fragment entry point that calls `mainImage` with
   the pixel coordinate and writes its `fragColor` to the output. `fragCoord` is
   in pixels with the Shadertoy origin convention (bottom-left); map howan's
   surface coordinates accordingly so a pasted shader is not vertically flipped.

2. **Provide the Shadertoy uniforms.** Extend the M6 uniform set (`iTime`,
   `iResolution`) with the commonly-referenced ones so a pasted shader that uses
   them links and runs: `iTimeDelta` (seconds since last frame), `iFrame` (i32
   frame counter), `iMouse` (vec4, **always 0** — howan is idle, no pointer
   tracking), and `iDate` (vec4 year/month/day/seconds-in-day). Keep the
   std140-style alignment/padding discipline the M6 `Uniforms` struct already
   follows (`render.rs`); document the field order so the WGSL struct and the
   Rust struct stay in lockstep.

3. **A minimal way to select a GLSL shader.** M8 owns directory scanning,
   playlists, rotation, ordering, and the "empty dir → bundled fallback"
   behavior — none of that is in scope here. To exercise GLSL now, add a single
   CLI flag `--shader <path>` (on the `daemon` and `start` subcommands in
   `crates/howan/src/cli.rs`) that loads one shader file from an explicit path,
   choosing the WGSL or GLSL pipeline by extension (`.wgsl` → WGSL, `.glsl`/
   `.frag` → GLSL). When the flag is absent, behavior is unchanged: the bundled
   WGSL shader plays. This is the smallest seam that proves "Shadertoy paste
   works"; M8 will generalize it to a directory.

Scope guard — **single pass only.** Shadertoy multi-buffer shaders (Buffer
A/B/C/D) and texture/audio channels (`iChannel0..3`) are out of scope; only a
single-pass `mainImage` is supported. A shader that needs channels should fail
to load with a clear error, not crash.

**Hardware note.** This still drives the existing wgpu rendering path on NVIDIA
Blackwell, but it does **not** change the surface / scanout invariant that M6
already cleared on hardware (no `set_fullscreen`, no opaque region — the surface
stays an ordinary composited window). The Blackwell modeset-wedge risk is the
same path M6 verified, so the on-hardware risk here is low; still confirm the
GLSL path renders on the real Blackwell + GNOME session under SSH guard, as a
sanity check rather than a first-ever GPU bring-up.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] The crate builds with the GLSL frontend wired (either wgpu's `glsl` feature or `naga::front::glsl`): `cargo build` succeeds, and `grep -rn "Glsl\|front::glsl\|glsl" crates/howan/src crates/howan/Cargo.toml` shows the GLSL path is actually present (not just a comment).
- [x] A CPU-only test compiles a minimal Shadertoy-style GLSL source — containing `void mainImage(out vec4 fragColor, in vec2 fragCoord)` — through the **same** parse+validate path the renderer uses (naga GLSL frontend + `naga::valid::Validator`), and asserts it validates with no error. The test needs no GPU or Wayland connection (mirror the existing pure unit-test pattern in `app.rs`). This is the gate that proves GLSL is accepted.
- [x] A CPU-only test asserts a GLSL source that references a multi-pass channel (`iChannel0`) is rejected with a clear, typed error (single-pass-only guard), rather than panicking.
- [x] A pure unit test covers the extended uniform computation: given an elapsed `Duration`, a previous-frame delta, a frame index, and a surface `(width, height)`, the uniform struct has `iTime`/`iTimeDelta` as the documented seconds values, `iFrame` equal to the frame index, `iResolution = [w, h, w/h]`, and `iMouse` all-zero. The struct's size/field order matches the WGSL/std140 layout (extend the M6 uniform test).
- [x] The bundled WGSL default path is unchanged when `--shader` is not given: the existing M6 uniform/render-path unit tests still pass, and `grep -rn "include_str" crates/` still shows the bundled WGSL shader compiled in as the default.
- [x] The composited-surface invariant is preserved: `grep -rn "set_fullscreen" crates/` shows no call site (matches only in comments/docs), the diff adds no `wl_surface.set_opaque_region` covering the surface, and no per-GPU / per-vendor branching is introduced (the wgpu path stays unconditional for all hardware).
- [x] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings` pass.

### Manual / on-hardware (verified by a human before merge)

- [ ] **Stage 1 (safe, software / headless)** recorded in `docs/guides/50-shader-player.md`: a real single-pass shader copy-pasted from Shadertoy (GLSL, `mainImage`), saved to a file and loaded via `--shader <path>`, renders and **visibly animates** on a software/headless Wayland session (e.g. weston + llvmpipe), with correct orientation (not vertically flipped) and aspect, and dismisses on input. Include the exact shader used (or a link) so the result is reproducible.
- [ ] **Stage 2 (Blackwell sanity check, SSH-guarded)** recorded in that guide: the same GLSL shader renders on the actual NVIDIA Blackwell + GNOME/Mutter session, **logged in over SSH from a second machine**, via wgpu without wedging the display engine / GSP firmware. Lower-risk than M6 (the surface/scanout path is unchanged), but confirm rather than assume; if the SSH-guarded run cannot be performed yet, mark this as the outstanding gate instead of treating the task as verified.
- [ ] Input still dismisses within ~100 ms (R2) while a GLSL shader's frame loop runs, and `SIGTERM`/`SIGINT` still unwind the clean-exit path.
- [ ] A genuinely Shadertoy-incompatible or malformed GLSL file (e.g. references `iChannel0`, or has a syntax error) is rejected with a readable error and the daemon falls back to a usable state (bundled shader or clean exit) rather than crashing — observed once by hand.

## Out of scope

- Directory scanning of `~/.config/howan/shaders/`, playlists, rotation, ordering, and the "empty/missing dir → bundled fallback" behavior — that is M8 (the directory part of R6). M7 adds only the single `--shader <path>` flag.
- Multi-pass Shadertoy shaders (Buffer A/B/C/D) and `iChannel0..3` texture/audio inputs (R6 explicitly limits MVP to single-pass `mainImage`).
- Frame-budget watchdog, FPS battery fallback (30 fps), and explicit naga static-analysis guards (loop bounds, etc.) beyond naga's built-in validation — that is M9 (R7). M7 may surface the validation *result*, but does not add the watchdog/limits.
- TOML config file (M11) — the shader path stays a CLI flag for now.
- Multi-monitor coverage of all outputs (M5) — render on the active output only.
- Restoring `set_fullscreen` / layer-shell coverage (the Q7 reversal) — the composited path stays unchanged.

## Implementation notes

- **mainImage wrapper:** synthesize the fragment entry point around the user's `mainImage`. Prefer doing this at the GLSL source level (prepend Shadertoy uniform declarations + a `main` that calls `mainImage(fragColor, gl_FragCoord.xy)` with the y-flip applied) so naga only ever sees standard GLSL. Keep the wrapper text in one place and documented.
- **y-flip:** Shadertoy's `fragCoord` origin is bottom-left; wgpu/Vulkan framebuffer coordinates are top-left. Flip y when computing the coordinate passed to `mainImage`, or a pasted shader appears upside-down — make this explicit and test the orientation in Stage 1.
- **Uniform struct:** extend the existing `Uniforms` in `render.rs` carefully — adding fields changes the buffer size and std140 padding. Keep the WGSL struct and the Rust struct field order identical and re-check 16-byte alignment (the M6 comment already explains the padding rule). `iMouse` is constant zero; `iFrame` comes from the frame-callback counter; `iDate` from the wall clock at frame time.
- **Shader selection:** `--shader <path>` reads one file; detect language by extension. On read/parse/validate failure, log a clear error and fall back to the bundled WGSL shader (do not crash the daemon). This keeps the daemon resilient and previews the M8 fallback behavior without implementing the directory scan.
- **Reuse the M6 frame loop:** the per-frame callback loop, device lifetime, and present() flow are unchanged; only the shader-module construction and the uniform buffer grow. Keep the change localized to the `render` module boundary where practical.
- **Docs:** extend `docs/guides/50-shader-player.md` (do not create a new guide) with a GLSL/Shadertoy section: the `mainImage` convention, the supported uniforms and their howan values (note `iMouse` is always 0), the single-pass limitation, the `--shader` flag, and the Stage 1 / Stage 2 results. Keep it DRY — reference, don't duplicate, the M6 wgpu/frame-loop description already in that file.
- Documentation, code comments, commit message, and PR description in English (see CLAUDE.md). This repository is public — do not reference any private repositories or trackers; cite only public Shadertoy / naga / wgpu references.
