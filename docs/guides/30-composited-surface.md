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

### Stage 1 — safe, protocol-level (software-rendered / headless)

Goal: confirm howan no longer issues `set_fullscreen` and stays on the
composited path, on a compositor that cannot trigger the dangerous modeset.

The safety requirement is **software-rendered / headless**, not non-NVIDIA
hardware. A headless compositor with a software (pixman/llvmpipe) renderer
drives no real KMS output and initializes no GPU, so it does **not** touch the
NVIDIA display engine — it is therefore safe to run **on the Blackwell machine
itself** (a separate non-NVIDIA box is not required). Do not confuse this with a
*nested GPU* compositor (`mutter --nested`, `sway` on the DRM/GPU backend),
which shares the GPU and is not safe; see Stage 2.

Run a headless software compositor, e.g. `weston` with the pixman renderer (on
Debian/Ubuntu: `sudo apt install weston`):

```bash
# Start a headless, software-rendered weston. It creates its own wayland socket
# (e.g. wayland-1) under $XDG_RUNTIME_DIR and never drives the real display.
weston --backend=headless-backend.so --renderer=pixman --width=1920 --height=1080 \
  --socket=wayland-stage1 &

# Run howan against THAT socket — never the real session's wayland-0.
# Headless has no input device, so the saver will not dismiss on its own; cap the
# run with a timeout and inspect the protocol it spoke.
WAYLAND_DEBUG=1 WAYLAND_DISPLAY=wayland-stage1 timeout 5 cargo run 2>wire.log; \
  grep -i 'set_fullscreen' wire.log && echo "UNEXPECTED set_fullscreen" \
    || echo "ok: no set_fullscreen on the wire"
```

What a headless run can and cannot confirm:

- **Can (protocol level):** howan sends no `xdg_toplevel.set_fullscreen`, creates a
  normal `xdg_toplevel`, and commits a buffer sized to the advertised output —
  visible in `wire.log` (`xdg_toplevel`, `set_min_size`/`set_max_size`,
  `wl_surface.commit`, no `set_fullscreen`, no `set_opaque_region`).
- **Cannot (visual/input):** that the saver is *visible*, *dismisses on input*,
  and *covers the output / stays on top*. Headless has no display or input
  device, so these require a real display and are exercised in Stage 2 (and the
  top-most question remains open regardless — see above).

**Status: PASSED (protocol level), 2026-05-21.** Verified against a headless
`weston` 14 with the pixman (CPU) renderer, advertising a 1920×1080 output. The
`WAYLAND_DEBUG` wire trace showed howan send **no** `xdg_toplevel.set_fullscreen`
and **no** `wl_surface.set_opaque_region`; it created an ordinary `xdg_toplevel`
and pinned `set_min_size`/`set_max_size` to `1920×1080`, matching the advertised
`wl_output.mode`, then committed a buffer. The visual/input aspects (visible,
dismiss-on-input, coverage/top-most) were not exercised — they require a real
display and fall to Stage 2.

### Stage 2 — Blackwell sign-off (SSH-guarded)

Goal: since the target machine is NVIDIA Blackwell and running there is a hard
requirement, confirm the saver displays and dismisses on the **actual Blackwell
+ GNOME/Mutter** session without wedging the display engine.

**Never launch howan directly on the Blackwell GUI session without an SSH
guard.** A nested compositor (`mutter --nested` etc.) shares the same GPU, so a
GSP firmware crash would still take down the host — nesting is not a safe
substitute.

Procedure: log in to the Blackwell machine **over SSH from a second device**
first, so that if the GPU wedges you can still capture logs, kill the process,
and reboot remotely. Even when the display engine dies, the network stack and
`sshd` keep running (in the original incident the machine still logged for ~2
minutes), so an SSH session remains usable for recovery.

The "second device" does not have to be another computer:

- **A phone or tablet works.** Enable `sshd` on the Blackwell host
  (`sudo systemctl enable --now ssh`) and SSH in from a phone on the same LAN
  (Android: Termux; iOS: Blink/Termius). This is the recommended option when no
  second computer is available.
- **No second device at all → pre-arm an automatic reboot** as a weaker safety
  net. Save all work and `sync` first, then schedule an unconditional reboot
  before launching, and cancel it if howan behaves:

  ```bash
  sudo systemd-run --on-active=120 systemctl reboot   # auto-reboot in 2 min
  # ... launch howan (below); if it is fine, cancel with:
  # sudo systemctl stop run-r<unit>.timer   (the unit name systemd-run printed)
  ```

  This trades the session (and any unsaved work) for avoiding a hard power-cycle
  and possible filesystem damage if the display wedges.
- **A local virtual terminal (Ctrl+Alt+F3) is NOT a recovery channel.** A
  display-engine / GSP wedge takes the text console down with it (same display
  hardware), so it cannot be relied on.

```bash
# From the second device, over SSH into the Blackwell host:

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

**Status: PASSED, 2026-05-21.** Run on the actual NVIDIA Blackwell + GNOME/Mutter
Wayland session (open kernel module 595.58.03, a 5120×2160 output), launched and
monitored from a phone SSH session per the procedure above.

- **No display-engine wedge.** The saver displayed and dismissed on input (mouse)
  with no freeze. The captured kernel log showed none of the crash signatures
  from the original incident — no `RC_TRIGGERED`, `GSP-CrashCat`, `IO_PAGE_FAULT`,
  "Failed to query display engine", NVRM `Xid`, or `gnome-shell` SIGSEGV. This is
  the gate, and it is cleared: the composited-surface approach runs safely on
  Blackwell.
- **Wire trace** confirmed no `set_fullscreen` and no `set_opaque_region`; the
  toplevel was sized to the output's advertised mode (`set_min/max_size`
  5120×2160 matching `wl_output.mode`).
- **Coverage (expected limitation):** the GNOME top panel remained visible — the
  composited toplevel does not cover the panel or guarantee top-most stacking.
  This is the open top-most-coverage question above, not a regression; full
  coverage would require `set_fullscreen`/layer-shell, which is exactly what this
  workaround avoids (and what the Restoration path would later restore).
