# howan

A lightweight Wayland screensaver written in Rust.

## Status

Pre-1.0. Under active development.

## Build

```bash
cargo build
```

## Run

```bash
howan daemon            # resident daemon: detect idle and show the saver autonomously
howan daemon --idle-timeout 60   # idle seconds before the saver appears (default 300)
howan start             # show the saver immediately (default when no subcommand is given)
howan stop              # terminate a running `start` (no-op if none is running)
```

`howan daemon` is the primary mode: a long-lived process that detects idle via
GNOME's `org.gnome.Mutter.IdleMonitor` and shows the saver when the seat has
been idle for `T1`, staying resident across show/dismiss cycles. See
[docs/guides/40-resident-daemon.md](docs/guides/40-resident-daemon.md).

`start`/`stop` are kept for manual testing. The earlier swayidle-driven
activation is superseded by the daemon; see
[docs/guides/20-swayidle.md](docs/guides/20-swayidle.md).

## License

GPL-3.0. See [LICENSE](LICENSE).
