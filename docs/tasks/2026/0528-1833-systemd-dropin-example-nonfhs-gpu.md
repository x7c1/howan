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
branch: task/0528-1833-systemd-dropin-example-nonfhs-gpu
created_at: 2026-05-28T18:33:07Z
updated_at: 2026-05-29T13:11:00Z
---

# docs(packaging): ship an opt-in systemd drop-in example for non-FHS GPU setups

## Overview

howan's GPU renderer (added in M6) loads the Vulkan loader (`libvulkan.so.1`)
and the GPU driver (via the Vulkan ICD) at runtime. On a normal
system-toolchain build this just works: the dynamic loader finds the libraries
through the FHS `ld.so` cache, and the Vulkan loader finds the GPU's ICD
manifest in its default search dir (`/usr/share/vulkan/icd.d`). No environment
variables are needed.

But a binary built with a **non-FHS toolchain on an FHS distro** — the concrete
case being a Nix-toolchain build on Ubuntu — uses a dynamic loader that does not
search the FHS paths. Such a binary cannot find `libvulkan.so.1` / the GPU
driver, so the daemon fails to find a wgpu adapter unless it is launched with
`LD_LIBRARY_PATH` (pointing at the FHS lib dir) and `VK_ICD_FILENAMES` (pointing
at the GPU's ICD manifest). This is a build-environment mismatch, not a defect
in howan, and it affects only that unusual setup.

Provide an **opt-in** way for those users to make the installed `--user` daemon
work, without polluting the shipped unit or breaking the majority of users:

1. Add a systemd drop-in **example** at
   `packaging/systemd/howan.service.d/override.conf.example`. It must be named
   `*.example` (NOT `*.conf`) so systemd never auto-loads it — it only takes
   effect after the user copies it to
   `~/.config/systemd/user/howan.service.d/override.conf`. Its comments must
   explain, generically: when it is needed (a non-FHS-toolchain build such as
   Nix on an FHS distro, plus the relevant GPU vendor), that it is NOT needed
   for a normal system-toolchain build, and how to discover the correct values
   for the local machine (e.g. `ldconfig -p | grep libvulkan` for the lib dir,
   `ls /usr/share/vulkan/icd.d/` for the ICD manifest). The example may use
   concrete Ubuntu+NVIDIA values (`/usr/lib/x86_64-linux-gnu`,
   `/usr/share/vulkan/icd.d/nvidia_icd.json`) as a worked example, framed as a
   sample to adapt — not as a universal default.

2. Document the opt-in pattern in user-facing docs (the README command section,
   and/or a packaging-oriented guide under `docs/guides/`): when to use the
   drop-in, where to copy it, and that it is unnecessary for a normal build.

**Do NOT** make `make install` / `packaging/install.sh` write `Environment=`
lines into the active unit by default. The paths are distro- and vendor-specific
(multiarch dir names differ across distros; pinning an NVIDIA ICD breaks
AMD/Intel users), so baking them into the shipped, public installer would break
most environments. The fix belongs in an opt-in, user-copied drop-in, not the
default install path.

This is a follow-up to M6 (the wgpu renderer is what introduced the runtime GPU
dependency). It is packaging/documentation only — no renderer or shader code
changes.

## Acceptance criteria

### Automated (pipeline-verified)

- [x] An example drop-in exists at `packaging/systemd/howan.service.d/override.conf.example`, and it is the only file added under that directory — no active `override.conf` (or any `*.conf`) is shipped: `find packaging -path '*systemd*service.d*' -name '*.conf'` returns nothing.
- [x] The example file is a valid systemd drop-in fragment: it contains a `[Service]` section with `Environment=LD_LIBRARY_PATH=...` and `Environment=VK_ICD_FILENAMES=...` lines, and comments that (a) state it is only for a non-FHS-toolchain build (e.g. Nix on an FHS distro) and unnecessary otherwise, and (b) show how to find the correct local values.
- [x] `packaging/install.sh` and the shipped `packaging/systemd/howan.service` contain no active `Environment=` injection: `grep -rn 'Environment=' packaging/install.sh packaging/systemd/howan.service` returns no match (the only `Environment=` lines live in the `.example` file).
- [x] User-facing documentation (README and/or a `docs/guides/` page) describes the opt-in drop-in: when it is needed, that it is unnecessary for a normal system-toolchain build, and the copy-to-`~/.config/systemd/user/howan.service.d/override.conf` step.
- [x] `cargo build && cargo test && cargo clippy --all-targets -- -D warnings` pass (no code change is expected; this guards against accidental breakage).

### Manual / on-hardware (verified by a human before merge)

- [ ] On a non-FHS-toolchain build (e.g. a Nix-built binary) on an FHS distro with an NVIDIA GPU, copying the example to `~/.config/systemd/user/howan.service.d/override.conf` (with the machine's correct paths) and running `systemctl --user daemon-reload && systemctl --user restart howan.service` lets the daemon find the GPU and render the saver on idle, with no command-line `LD_LIBRARY_PATH` / `VK_ICD_FILENAMES`.

## Out of scope

- Making the default `make install` inject `Environment=` into the active unit (explicitly rejected above).
- Switching howan's build to a system/FHS toolchain, or adding nixGL — those are separate environment/toolchain decisions, not this packaging example.
- Any renderer / shader / wgpu code change (that is M6 and its follow-ups).
- Auto-detecting the host's Vulkan lib/ICD paths in the installer.
