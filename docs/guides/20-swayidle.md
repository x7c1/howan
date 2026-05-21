# swayidle Integration

## Overview

This guide describes the **original** activation design, in which howan did not
detect idleness itself but relied on an external idle watchdog —
[`swayidle`](https://github.com/swaywm/swayidle), which implements the
`ext-idle-notify-v1` protocol — to decide *when* the screen had been idle and to
invoke howan accordingly. That design is now superseded by the resident daemon
(see the note below). The guide is retained for the exact swayidle invocation,
how the `start`/`stop` lifecycle works, and the result of the manual
verification against a real GNOME / Mutter Wayland session.

> **Superseded:** the swayidle-driven `start`/`stop` activation described here is
> superseded by the resident daemon (`howan daemon`), which builds idle detection
> in instead of delegating to swayidle. See
> [40-resident-daemon.md](40-resident-daemon.md). This document is kept because it
> records the Q1 finding below — that Mutter lacks `ext-idle-notify-v1` — which is
> the reason idle detection had to move in-process. The `start`/`stop` CLI itself
> still works for manual testing.

> **Known limitation:** this swayidle approach does **not** work on GNOME/Mutter
> — Mutter does not implement the `ext-idle-notify-v1` idle-detection protocol
> swayidle needs (it offers only idle *inhibit*). See "Idle detection on
> GNOME/Mutter" below. The `start`/`stop` CLI is verified and works; only the
> swayidle-driven idle trigger is unavailable on the primary target.

Key points:

- Run howan under swayidle with `timeout <N> 'howan start'` and
  `resume 'howan stop'`.
- `howan start` opens the saver; `howan stop` terminates a running saver.
  Running `howan` with no subcommand is equivalent to `howan start`.
- The two processes communicate through a PID file at
  `$XDG_RUNTIME_DIR/howan.pid`; `stop` sends `SIGTERM`, which unwinds the saver
  through its normal clean-exit path.

## CLI

| Command       | Behavior                                                            |
| ------------- | ------------------------------------------------------------------- |
| `howan start` | Launch the saver. Blocks until dismissed or stopped.                |
| `howan stop`  | Terminate a running saver. No-op success if none is running.        |
| `howan`       | No subcommand: defaults to `start`.                                 |

The saver exits on the first keyboard / pointer / touch input, on a
compositor-issued close request, or on `SIGTERM` / `SIGINT`.

## Running under swayidle

```bash
swayidle -w \
  timeout 300 'howan start' \
  resume      'howan stop'
```

- `-w` makes swayidle wait for each command to finish before continuing, which
  keeps the `timeout`/`resume` hooks ordered.
- `timeout 300` fires `howan start` after 300 seconds of inactivity. Adjust the
  number to taste.
- `resume` fires `howan stop` as soon as activity is detected again.

`howan` must be on swayidle's `PATH`. During development, either
`cargo install --path crates/howan` or point the hook at the built binary
(e.g. `'/path/to/target/debug/howan start'`).

## How start and stop find each other

swayidle runs the `resume` hook (`howan stop`) as a process separate from the
`timeout` hook (`howan start`), so `stop` needs a way to reach the
already-running saver:

- On launch, `howan start` writes its PID to `$XDG_RUNTIME_DIR/howan.pid`
  (falling back to the system temp directory when `XDG_RUNTIME_DIR` is unset).
  It removes the file on every exit path.
- `howan stop` reads that PID and sends `SIGTERM`. The saver routes `SIGTERM`
  (and `SIGINT`) through its calloop event loop, setting the same exit flag the
  input handlers use, so shutdown follows the normal clean-exit path and the
  process exits with status `0`.
- `stop` is a clean no-op when there is nothing to stop: a missing PID file, an
  unparseable file, or a stale PID (the owning process is already gone) all exit
  `0` without an alarming message. A stale file is removed so a subsequent
  `stop` stays a no-op.

`start` does not enforce singleton behavior (swayidle will not fire `timeout`
twice without an intervening `resume`), but it does refuse to start if a *live*
instance already owns the PID file, rather than stranding the existing one.

## Manual verification

The swayidle-driven behavior cannot be reproduced from the diff or the canonical
`cargo build && cargo test && cargo clippy` run; it needs a real Wayland session.
Record the outcome here.

Target session: GNOME / Mutter on Wayland (Ubuntu 26.04).

| Check                                                       | Result (2026-05-21)                                              |
| ----------------------------------------------------------- | ---------------------------------------------------------------- |
| swayidle drives `howan start` after the idle timeout        | **BLOCKED** — see "Idle detection on GNOME/Mutter" below         |
| Saver disappears on resume (`howan stop`)                   | depends on the idle hook above; `stop` itself verified           |
| `start` exits status 0 after `stop`; PID file removed       | PASS — exercised directly on a headless `weston`                 |
| `stop` is a clean no-op on a missing/stale PID file         | PASS                                                             |
| `start` commits no buffer before the first xdg `configure`  | PASS — confirmed on a strict (`weston`) compositor               |
| `howan start` is safe on real NVIDIA Blackwell              | PASS — see `30-composited-surface.md` Stage 2 (no display wedge) |

### Idle detection on GNOME/Mutter — not available

On the target GNOME / Mutter session, swayidle exits immediately with
`Compositor doesn't support idle protocol`. Mutter advertises
`zwp_idle_inhibit_manager_v1` (idle *inhibit*) but **not** `ext-idle-notify-v1`
(idle *notify*) nor `org_kde_kwin_idle`, so no Wayland client — swayidle
included — can detect idle on this compositor. (swayidle itself does support
`ext-idle-notify-v1`; the gap is on Mutter's side.)

Consequence: the swayidle-driven idle → saver → resume flow cannot run on
GNOME/Mutter as designed. The `howan start` / `stop` CLI and lifecycle are
correct and verified independently (above). Driving the saver on GNOME needs a
different idle source — e.g. the `org.gnome.Mutter.IdleMonitor` or
`org.freedesktop.ScreenSaver` D-Bus interfaces — which is an architecture
question beyond this milestone. Top-most coverage on Mutter (no
`wlr-layer-shell`) is discussed in `30-composited-surface.md`.

The CLI-level lifecycle (start writes the PID file, stop signals it, no-op on a
missing/stale file, PID file cleaned up) is verified directly by exercising the
binary.

### ⚠️ Incident: full system lockup on NVIDIA Blackwell (2026-05-20)

Launching `howan start` to manually check the on-screen behavior caused an
**unrecoverable display freeze that required a hard reboot.**

- **Environment:** GeForce RTX 5060 Ti (GB206, Blackwell) · NVIDIA open kernel
  module 595.58.03 · Linux 7.0 · GNOME Shell / Mutter 50.1 on Wayland.
- **Trigger:** `howan start` mapped its fullscreen surface while Chrome was
  playing a fullscreen YouTube video. The saver covered the screen, then the
  session became unrecoverable.
- **Crash boot journal:** `gnome-shell` crashed with SIGSEGV, after which the
  NVIDIA display engine wedged (`nvidia-modeset: Failed to query display engine
  channel state`), the GSP firmware crashed (`GSP-CrashCat` / `RC_TRIGGERED`)
  with IOMMU `IO_PAGE_FAULT`s, and the compositor could not be restarted
  (EGL/KMS init failed: "not supported by EGL"). Only a hard reboot recovered.
- **`howan`'s role:** the saver process itself had already exited cleanly
  (status 0, PID file removed) ~11 s *before* the compositor crash. The fault
  is not a `howan` logic defect — it is a known **Blackwell (RTX 50-series)
  NVIDIA driver / GSP-firmware bug**: a display-engine / atomic-modeset
  operation collapses the GSP firmware and kills the display engine until a
  hard reset (reported across nvidia-open 580.x / 595.x). `howan`'s fullscreen
  surface forced the Mutter modeset that triggered it, concurrent with Chrome's
  GPU video-decode load.

**Resolution:** howan no longer calls `set_fullscreen`. It now covers the output
with an ordinary composited (non-fullscreen, non-opaque) surface, which stays
off Mutter's unredirect / direct-scanout modeset path and so does not trigger
this crash. See [30-composited-surface.md](30-composited-surface.md), where a
Stage 2 run on the actual Blackwell session confirmed no display-engine wedge.
The description above is the historical fullscreen build at the time of the
incident.

**Caution for any future fullscreen experiment:** do **not** run a
`set_fullscreen` build directly on an NVIDIA Blackwell + Wayland desktop session.
Verify in a software-rendered / headless Wayland session, or under an SSH guard,
so a GPU-firmware crash cannot take down the host.

References: NVIDIA RTX 5060 Ti / Blackwell GSP firmware crash reports on the
[NVIDIA developer forums](https://forums.developer.nvidia.com/t/regression-nvidia-modeset-kernel-panic-kwin-wayland-crash-on-5060-ti-blackwell-under-high-vram-load/351517).
