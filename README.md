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
howan start   # launch the saver (default when no subcommand is given)
howan stop    # terminate a running saver (no-op if none is running)
```

howan is meant to be driven by `swayidle`. See
[docs/guides/20-swayidle.md](docs/guides/20-swayidle.md) for the invocation and
the start/stop lifecycle.

## License

GPL-3.0. See [LICENSE](LICENSE).
