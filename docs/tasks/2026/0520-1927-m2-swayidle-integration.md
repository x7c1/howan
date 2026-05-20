---
status: completed
pipeline_phase: null
plan: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0520-1927-m2-swayidle-integration
created_at: 2026-05-20T10:27:49Z
updated_at: 2026-05-20T11:32:54Z
---

# feat: M2 swayidle integration (start/stop lifecycle)

## Overview

Implement the second milestone of howan: drive the fullscreen saver from
`swayidle` so it appears automatically after the system has been idle and
disappears on resume.

`swayidle` is the external idle watchdog (it implements `ext-idle-notify-v1`);
howan does not implement idle detection itself. The integration contract is the
pair of commands swayidle invokes:

```
swayidle -w \
  timeout 300 'howan start' \
  resume     'howan stop'
```

So howan must grow a small CLI: `howan start` launches the fullscreen saver
(the M1 behavior), and `howan stop` terminates the running saver. Because the
`resume` hook runs `howan stop` as a *separate* process, `stop` needs a way to
find and signal the already-running `start` instance. This milestone is mostly
about that process lifecycle and IPC, plus verifying the behavior against a
real swayidle invocation.

Target compositor: GNOME / Mutter on Wayland (Ubuntu 26.04). Other compositors
should work but are not required to verify.

## Acceptance criteria

- [ ] A CLI is in place: `howan start` runs the fullscreen saver (the M1 behavior), and `howan stop` terminates the running saver
- [ ] Running `howan` with no subcommand keeps a sensible default (either prints usage, or behaves as `start`); decide and document which
- [ ] `howan stop` terminates a running `howan start` instance cleanly, and that instance exits with status 0
- [ ] `howan stop` with no running instance exits without error (a no-op) and prints no alarming error message
- [ ] After a clean exit of the saver (e.g. a `start`→`stop` cycle), no stale runtime state remains: a second `howan stop` is still a clean no-op, and the runtime artifact the IPC relies on (e.g. the `$XDG_RUNTIME_DIR/howan.pid` file) has been removed
- [ ] On `SIGTERM`, the running saver shuts down gracefully through the Wayland event loop rather than aborting
- [ ] Driven by `swayidle -w timeout <N> 'howan start' resume 'howan stop'`, the saver appears after the idle timeout and disappears on resume. This is a **manual** check: it needs a real GNOME/Mutter Wayland session and is not reproducible from the diff or the canonical build/test/clippy run. Record the outcome in the guide below — in particular, whether the saver's surface actually appeared on top, since Mutter lacks `wlr-layer-shell` and top-most is therefore not guaranteed
- [ ] The exact swayidle invocation and the manual verification result above are documented in a guide under `docs/guides/`, following the repo's numeric-prefix convention (e.g. `docs/guides/20-swayidle.md`) and distinct from `10-documentation-structure.md`

## Implementation notes

- Add a CLI argument parser. `clap` (derive) is the conventional choice and will be carried forward by later milestones (config-file overrides in M11, `--t1` etc.). Keep the surface minimal for now: just `start` and `stop`.
- For `stop` → running-instance IPC, the simplest carry-forward design is a PID file under `$XDG_RUNTIME_DIR/howan.pid` (fall back sensibly if `XDG_RUNTIME_DIR` is unset): `start` writes its PID on launch and removes it on exit; `stop` reads it and sends `SIGTERM`. A Unix-domain socket is also acceptable if the implementer prefers it, but do not over-build — M2 needs only "tell the running saver to quit".
- Handle `SIGTERM` (and `SIGINT`) by setting the same exit flag the input handlers already toggle, so the calloop event loop unwinds through the existing M1 clean-exit path rather than terminating abruptly. Do not duplicate teardown logic.
- Guard against a stale PID file (process no longer alive): `stop` should treat a stale/nonexistent target as a no-op success rather than failing.
- `start` does not need to defend against a second concurrent `start` for M2 (swayidle won't fire `timeout` twice without an intervening `resume`), but if it's cheap to detect an existing live PID and refuse/replace, a brief note in the code is welcome. Do not add heavy singleton machinery.
- Documentation, code comments, commit messages, and PR description in English (see CLAUDE.md).

## Out of scope

- `idle-inhibit-unstable-v1` / DPMS suppression (M3)
- The 3-phase lifecycle, `loginctl lock-session` handoff, DPMS handoff (M4)
- Multi-monitor coverage (M5)
- Shader rendering — WGSL or GLSL (M6+)
- Security guards: frame-budget watchdog, FPS cap, naga validation (M9)
- systemd `--user` unit packaging (M10) — only the manual swayidle invocation is documented here
- Configuration file / TOML schema and CLI overrides beyond `start`/`stop` (M11)
