# Howan

A lightweight Wayland screensaver written in Rust.

## Status

Pre-1.0. Under active development.

## Build

```bash
cargo build
```

## Install

Install Howan as a systemd `--user` service:

```bash
git clone https://github.com/x7c1/howan.git
cd howan
make install
```

This runs [`packaging/install.sh`](packaging/install.sh), which builds
the binary into `~/.cargo/bin` via `cargo install --path crates/howan`,
copies [`packaging/systemd/howan.service`](packaging/systemd/howan.service)
to `~/.config/systemd/user/howan.service`, then reloads, enables, and
restarts the unit.

To remove:

```bash
make uninstall
```

This runs [`packaging/uninstall.sh`](packaging/uninstall.sh), which is
idempotent.

### Inspecting the service

```bash
systemctl --user status howan.service          # current state
journalctl --user -u howan.service             # logs
journalctl --user -u howan.service -f          # follow logs live
```

### Overriding CLI flags

The unit launches `howan daemon` with no flags, so it uses the built-in
defaults (`T1=5min`, `T_dpms=120min`). To override, edit
`~/.config/systemd/user/howan.service`, then reload and restart:

```bash
systemctl --user daemon-reload
systemctl --user restart howan.service
```

Note: a subsequent `make install` overwrites
`~/.config/systemd/user/howan.service` with the copy from this repo,
discarding local edits. Apply the same edit to
[`packaging/systemd/howan.service`](packaging/systemd/howan.service) if
you want it to survive re-install.

### GPU library paths for a non-FHS build (opt-in)

A normal system-toolchain build needs nothing here. But a binary built with a
non-FHS toolchain (e.g. Nix) on an FHS distro cannot find the Vulkan loader /
GPU driver at runtime, so the daemon fails to find a GPU. For that case, copy
the example drop-in and adapt it to your machine:

```bash
mkdir -p ~/.config/systemd/user/howan.service.d
cp packaging/systemd/howan.service.d/override.conf.example \
   ~/.config/systemd/user/howan.service.d/override.conf
# edit the copy with your machine's paths (the file explains how to find them)
systemctl --user daemon-reload && systemctl --user restart howan.service
```

See [docs/guides/50-shader-player.md](docs/guides/50-shader-player.md#gpu-runtime-libraries-on-a-non-fhs-build)
for when this is needed and how to discover the correct values.

## Run

```bash
howan daemon            # resident daemon: detect idle and show the saver autonomously
howan daemon --idle-timeout 60     # idle seconds before the saver appears (default 300)
howan daemon --dpms-timeout 3600   # seconds before the daemon hands off to compositor DPMS (default 7200)
howan start             # show the saver immediately (default when no subcommand is given)
howan start --shader path/to/shader.glsl   # play one shader file instead of the bundled default
howan stop              # terminate a running `start` (no-op if none is running)
```

`--shader <path>` (on `daemon` and `start`) plays a single shader file chosen by
extension: `.wgsl` for WGSL, `.glsl` / `.frag` for a Shadertoy-convention GLSL
`mainImage` shader. Without it, the bundled WGSL shader plays.

`howan daemon` is the primary mode: a long-lived process that detects idle via
GNOME's `org.gnome.Mutter.IdleMonitor` and shows the saver when the seat has
been idle for `T1`, staying resident across show/dismiss cycles. See
[docs/guides/40-resident-daemon.md](docs/guides/40-resident-daemon.md).

The saver renders a GPU-animated shader — the bundled WGSL default, or a
WGSL/GLSL (Shadertoy) file via `--shader`; see
[docs/guides/50-shader-player.md](docs/guides/50-shader-player.md).

`start`/`stop` are kept for manual testing. The earlier swayidle-driven
activation is superseded by the daemon; see
[docs/guides/20-swayidle.md](docs/guides/20-swayidle.md).

## License

GPL-3.0. See [LICENSE](LICENSE).
