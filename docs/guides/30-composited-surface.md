# Composited Surface (avoiding the fullscreen modeset)

## Overview

This guide explains why howan covers the screen with an ordinary **composited**
surface instead of a fullscreen one, and why that surface is deliberately left
**non-opaque**. Both choices exist to keep howan off the compositor code path
that triggered a GPU lockup on NVIDIA Blackwell hardware. It also records the
manual safe-hardware verification procedure (two stages) and its current
status.

Key points:

- howan does **not** call `xdg_toplevel.set_fullscreen`. It sizes an ordinary
  `xdg_toplevel` to the active output's current mode instead.
- howan does **not** declare an opaque region on its surface
  (`wl_surface.set_opaque_region`). The shm buffer is still filled with
  opaque-black pixels, which is a different thing.
- There is **no per-GPU / per-vendor branching**. The composited path is the
  single, unconditional drawing path for all hardware.
- This is a **temporary workaround**, not the preferred design. The
  "Restoration path" section below gives the two conditions under which
  `set_fullscreen` should be brought back.
- The two manual verification stages (Stage 1 software/non-NVIDIA, Stage 2
  Blackwell over SSH) are **not yet performed** and remain the outstanding
  gate.

## Background: the incident

On a GNOME/Mutter Wayland session running on an NVIDIA Blackwell (RTX 50-series,
e.g. RTX 5060 Ti) GPU, mapping a fullscreen surface led to a full system lockup
that required a hard reset. The minimum facts needed to understand this design:

- **Hardware:** NVIDIA Blackwell, RTX 50-series (observed on an RTX 5060 Ti).
- **Crash cascade (one line):** mapping a fullscreen surface → Mutter
  unredirect / direct-scanout KMS modeset → NVIDIA GSP-firmware crash → display
  engine wedged → hard reset required.
- **Root cause:** this is an NVIDIA driver / GSP-firmware bug, **not** a howan
  logic defect — but howan's fullscreen request was the trigger, so howan must
  stop pulling that trigger. Public NVIDIA developer-forum reports of
  GSP-firmware crashes on Blackwell corroborate the upstream nature of the bug.

See also: operational swayidle context (how howan is launched as a screen saver,
and the full incident timeline) is added by the swayidle integration work; it is
not required to understand this guide, which stands on its own.

## Why no `set_fullscreen`, why non-opaque

Mutter only elects a surface for its **unredirect / direct-scanout**
optimization when the surface is either:

- **opaque** (an opaque region covering the surface has been declared), or
- the **transparent surface of a fullscreen window**.

That optimization performs a KMS plane/mode reconfiguration when the surface
maps. On Blackwell that modeset is what wedges the display engine / GSP
firmware. (See the GNOME/mutter `window-actor/wayland` scanout-gating history,
e.g. merge request !798.)

The corollary is the safety mechanism howan relies on: a surface that is
**neither fullscreen nor opaque** is never elected for that path, so it stays on
the normal composited path and no risky modeset happens. howan therefore:

1. Does not call `set_fullscreen` — it is an ordinary `xdg_toplevel`.
2. Does not declare an opaque region on the surface.

### Opaque pixels vs. opaque region

These are two different things and only the second one matters for scanout
eligibility:

- The `wl_shm` ARGB8888 buffer is filled with fully opaque black
  (alpha `0xFF`). This is purely about appearance (a solid black screen).
- `wl_surface.set_opaque_region` would tell the compositor the surface has no
  see-through parts, making it an unredirect/scanout candidate. howan never
  calls this.

A future contributor might be tempted to "optimize" rendering by declaring an
opaque region (it can let the compositor skip painting what is behind the
surface). **Do not do this** — it re-arms exactly the modeset path this design
avoids. The code comment at the surface-setup site in `crates/howan/src/app.rs`
states this explicitly.

## How the screen is covered now

Instead of fullscreen, howan sizes the surface to the **active output's current
mode**:

- The active output is the one the surface enters (`wl_surface.enter`), matching
  the existing "active output only" behavior. Until a surface-enter event
  arrives, howan falls back to the first advertised output.
- The size comes from the active output's `wl_output` mode flagged `current`
  (via SCTK's `OutputState` / `OutputInfo`), falling back to the
  compositor-reported logical size.
- The toplevel's min and max size are pinned to that size so the compositor does
  not offer a smaller interactive size.
- If output geometry is not yet available at startup, howan keeps the small
  initial allocation (`INITIAL_WIDTH` / `INITIAL_HEIGHT`) and resizes on the
  first output / configure event rather than blocking startup.

### Open question: top-most coverage

Sizing the surface to the output is **not** a guarantee that the saver visually
covers the screen and stays on top of other windows. Mutter has no
`wlr-layer-shell`, so there is no standard protocol to force a top-most overlay,
and a non-fullscreen toplevel may even receive server-side decorations (a
titlebar). Achieving guaranteed top-most full coverage is a standing open
question, deliberately out of scope for this change; it is recorded here so the
limitation is explicit.

## No per-GPU branching

The composited-surface path is correct and safe on every GPU, so it is the
single unconditional drawing path. There is intentionally:

- no NVIDIA / Blackwell detection,
- no `/sys/class/drm` vendor probing or GPU-id matching,
- no vendor-keyed configuration switch,
- no `if blackwell { … }`-style fallback.

## Restoration path — when to return to `set_fullscreen`

The composited-surface approach is a **temporary workaround**, not the ideal
design. The ideal design uses `set_fullscreen` (or a `wlr-layer-shell` overlay):
it *guarantees* full screen coverage and top-most stacking, which the composited
path cannot guarantee. This is exactly the unresolved limitation described under
"Open question: top-most coverage" above — accepting it is the price we pay to
avoid the Blackwell modeset crash, not a choice we would make otherwise.

Restore the `set_fullscreen`-based design once **both** of the following hold:

1. An upstream **NVIDIA driver / GSP-firmware release fixes the Blackwell
   modeset crash** (the cascade described under "Background: the incident").
2. The **SSH-guarded Blackwell run (Stage 2 below) re-confirms** that a
   fullscreen surface no longer wedges the GPU.

Until both are true, keep the composited path. When restoring, re-introduce the
fullscreen request at the surface-setup site in `crates/howan/src/app.rs` (the
code comment there points back to this section) and re-run both verification
stages.

## Manual verification

You cannot validate real screen coverage from CI or a headless build; it
requires a running Wayland GUI session. Two stages are defined. **Both are
currently outstanding (not yet performed in this change.)** Do not treat the
task as fully verified until they are done and recorded here.

### Stage 1 — safe, protocol-level (software-rendered / non-NVIDIA)

Goal: confirm howan no longer issues `set_fullscreen`, stays on the composited
path, appears, and dismisses on input — on hardware where a modeset cannot wedge
a Blackwell GPU.

Run on a **non-NVIDIA or software-rendered / headless Wayland** session, e.g. a
VM with virtio-gpu, llvmpipe, or `weston` with the pixman renderer:

```bash
# Example: a nested weston with the software (pixman) renderer.
weston --backend=headless --renderer=pixman --width=1920 --height=1080 &
# Point howan at that compositor.
WAYLAND_DISPLAY=wayland-1 cargo run

# In another terminal, confirm no fullscreen request is sent on the wire:
WAYLAND_DEBUG=1 WAYLAND_DISPLAY=wayland-1 cargo run 2>&1 \
  | grep -i 'set_fullscreen' && echo "UNEXPECTED set_fullscreen" || echo "ok: no set_fullscreen"
```

Record:

- Whether the saver appears.
- Whether any keyboard / pointer / touch input dismisses it with exit status 0.
- Whether it actually covered the output and stayed on top (this connects to
  the unresolved top-most question above — report honestly, do not assume).

**Status: outstanding — not yet performed.**

### Stage 2 — Blackwell sign-off (SSH-guarded)

Goal: since the target machine is NVIDIA Blackwell and running there is a hard
requirement, confirm the saver displays and dismisses on the **actual Blackwell
+ GNOME/Mutter** session without wedging the display engine.

**Never launch howan directly on the Blackwell GUI session without an SSH
guard.** A nested compositor (`mutter --nested` etc.) shares the same GPU, so a
GSP firmware crash would still take down the host — nesting is not a safe
substitute.

Procedure: log in to the Blackwell machine **over SSH from a second machine**
first, so that if the GPU wedges you can still capture logs, kill the process,
and reboot remotely.

```bash
# On the second machine, over SSH into the Blackwell host:

# 1. Start capturing the kernel/compositor log in case of a wedge.
journalctl -f -k -u gdm > /tmp/howan-blackwell.log &

# 2. Launch howan against the live session (adjust WAYLAND_DISPLAY/XDG_RUNTIME_DIR
#    to the GUI user's session).
WAYLAND_DISPLAY=wayland-0 cargo run

# 3. If anything wedges, kill it and (if needed) reboot from this SSH session.
pkill -INT howan   # clean dismiss path
# reboot            # only if the display engine is unrecoverable
```

Record:

- Whether the saver displayed and dismissed without wedging the display engine.
- Any `nvidia-modeset` / `GSP` errors observed in the captured log.

**Status: outstanding — this is the gate that must be cleared before declaring
howan safe to run on Blackwell.**
