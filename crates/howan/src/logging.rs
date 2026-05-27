//! Tracing subscriber initialization for the daemon.
//!
//! `howan` runs under a systemd `--user` unit (see
//! `docs/guides/40-resident-daemon.md`), and systemd captures the service's
//! stderr into the journal. We therefore route all lifecycle events through
//! `tracing` and render them to stderr via `tracing-subscriber`. The same
//! subscriber serves `cargo run` from a terminal, which is also handy when
//! iterating locally.
//!
//! - **Sink:** stderr. The journal picks it up for the `--user` unit; an
//!   interactive `howan daemon` lands the same lines on the terminal.
//! - **ANSI off.** The journal does not render colors, and an interactive
//!   `journalctl --user -u howan.service` would render escape codes as garbage.
//!   Lines stay readable in both sinks.
//! - **Default level INFO** via `EnvFilter`. `RUST_LOG=howan=debug` (or any
//!   other valid filter directive) overrides it at runtime without rebuilding.
//! - **Timestamps kept.** The journal also stamps each line, but `cargo run`
//!   does not, and grepping a captured stderr file is easier when each line
//!   carries its own timestamp.
//! - **`try_init`** so a second call from a test (or any other entry point) is
//!   a no-op rather than a panic. Production calls this exactly once from
//!   `main`.

use tracing_subscriber::{fmt, EnvFilter};

/// Install the global `tracing` subscriber for the process.
///
/// Idempotent and panic-free: if a subscriber is already installed (e.g. a
/// test installed one, or `init` was called twice by mistake), the second
/// call returns silently via `try_init`. The error path is intentionally
/// dropped because the only failure mode is "already initialized", which is
/// not actionable.
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
}
