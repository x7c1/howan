---
status: completed
pipeline_phase: null
plan: null
blocked_by: []
subagent_type: general-purpose
retries_remaining: 1
check_command: "cargo build && cargo test && cargo clippy --all-targets -- -D warnings"
assignee: null
branch: task/0519-2217-m1-fullscreen-window
run_log: /home/x7c1/repos/haco-studio/atelier/.tmp/task-loop/0519-2217-m1-fullscreen-window-20260519_2238.md
created_at: 2026-05-19T13:17:59Z
updated_at: 2026-05-19T14:24:44Z
---

# feat: M1 fullscreen window with input-dismiss

## Overview

Implement the first milestone of howan: a Wayland client that opens a fullscreen window, renders a solid black background, and exits when it receives any keyboard, pointer, or touch input.

This is the foundation that subsequent milestones build on:

- Idle-inhibit attaches to the surface created here
- The 3-phase lifecycle wraps the dismiss handler
- The shader player will replace the black rendering with WGSL output

Use the `smithay-client-toolkit` (SCTK) crate to bind Wayland protocols. For rendering, either `softbuffer` (CPU) or a minimal `wgpu` setup is acceptable — pick whichever keeps the diff smaller. Do not introduce a dependency that will not be carried forward to later milestones.

Target compositor: GNOME / Mutter on Wayland. Other compositors (KWin, Sway, Hyprland) should work but are not required to verify.

## Acceptance criteria

- [ ] `cargo run` from the workspace root opens a fullscreen window on the active output
- [ ] The window content is solid black
- [ ] Pressing any keyboard key dismisses the window and exits the process
- [ ] Clicking any pointer button dismisses the window and exits the process
- [ ] Tapping the touchscreen dismisses the window and exits the process (wire up the handler; physical touch testing is optional when no touchscreen is available)
- [ ] Dismiss response is under 100 ms by inspection (no need to measure precisely, but no obvious lag)
- [ ] The process exits with status 0 on dismiss
- [ ] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings` succeeds with no warnings

## Implementation notes

- Add `smithay-client-toolkit` to `crates/howan/Cargo.toml`. Use the latest published version
- Use `xdg-shell` to create the surface and request `set_fullscreen` to enter fullscreen
- Bind `wl_seat` and handle `wl_keyboard.key`, `wl_pointer.button`, and `wl_touch.down` events; any one of these triggers exit
- Black rendering can be done by drawing a single solid-color buffer once; no per-frame redraw is needed for M1
- No tests are required for M1, but `cargo test` must still succeed (it will pass trivially with no tests defined)

## Out of scope

- Idle-inhibit handling
- 3-phase lifecycle, lock-screen handoff, DPMS-handoff
- Multi-monitor support (use the active output only)
- Shader rendering (WGSL or GLSL)
- Security guards (frame budget, FPS cap)
- systemd unit integration
- Configuration file / TOML schema
