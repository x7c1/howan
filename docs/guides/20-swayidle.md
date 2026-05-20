# swayidle Integration

## Overview

howan does not detect idleness itself. It relies on an external idle watchdog —
[`swayidle`](https://github.com/swaywm/swayidle), which implements the
`ext-idle-notify-v1` protocol — to decide *when* the screen has been idle and to
invoke howan accordingly. This guide documents the exact swayidle invocation
that drives howan, how the `start`/`stop` lifecycle works, and the result of the
manual verification against a real GNOME / Mutter Wayland session.

Key points:

- Run howan under swayidle with `timeout <N> 'howan start'` and
  `resume 'howan stop'`.
- `howan start` opens the fullscreen saver; `howan stop` terminates a running
  saver. Running `howan` with no subcommand is equivalent to `howan start`.
- The two processes communicate through a PID file at
  `$XDG_RUNTIME_DIR/howan.pid`; `stop` sends `SIGTERM`, which unwinds the saver
  through its normal clean-exit path.

## CLI

| Command       | Behavior                                                            |
| ------------- | ------------------------------------------------------------------- |
| `howan start` | Launch the fullscreen saver. Blocks until dismissed or stopped.     |
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

| Check                                                      | Result        |
| ---------------------------------------------------------- | ------------- |
| Saver appears after the idle timeout                       | _pending_     |
| Saver disappears on resume (`howan stop` via SIGTERM)      | _pending_     |
| `start` instance exits with status 0 after `stop`          | _pending_     |
| PID file removed after the cycle                           | _pending_     |
| Saver surface actually rendered **on top**                 | _pending_     |

> Note on top-most: Mutter does not implement `wlr-layer-shell`, so an
> xdg-shell fullscreen window is not guaranteed to sit above every other
> surface. The "rendered on top" row above records what was actually observed;
> a guaranteed always-on-top overlay is out of scope for this milestone.

The CLI-level lifecycle (start writes the PID file, stop signals it, no-op on a
missing/stale file, PID file cleaned up) was verified directly by exercising the
binary; only the on-screen swayidle behavior remains a manual desktop check.
