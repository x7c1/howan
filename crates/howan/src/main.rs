//! howan: a lightweight Wayland screensaver.
//!
//! Entry point and CLI dispatch. The primary mode is `howan daemon`: a resident
//! process that owns idle detection (GNOME's `org.gnome.Mutter.IdleMonitor` over
//! D-Bus) and shows the saver — an output-sized composited window running a
//! GPU-animated fragment shader (the bundled WGSL default or a WGSL/GLSL file
//! via `--shader`; see `docs/guides/50-shader-player.md`) — autonomously when
//! the seat has been idle for `T1`. Input dismisses the saver
//! but the daemon stays resident and re-arms for the next idle period. See
//! `docs/guides/40-resident-daemon.md`.
//!
//! `howan start` shows the saver immediately and exits on the first input;
//! `howan stop` terminates a running `start`. They talk through a PID file and
//! are kept for manual testing (the original swayidle-driven activation, now
//! superseded by the daemon; see `docs/guides/20-swayidle.md`).

mod app;
mod cli;
mod daemon;
mod logging;
mod pidfile;

use std::process::ExitCode;

use clap::Parser;
use tracing::error;

use cli::{Cli, Command};
use daemon::mutter::MutterIdleSource;

fn main() -> ExitCode {
    // Initialize the tracing subscriber before any other work so the rest of
    // `main` (including the daemon's startup lifecycle events) is captured by
    // the journal when running under the systemd `--user` unit. `try_init`
    // never panics on double-init; see `logging::init`.
    logging::init();

    let result = match Cli::parse().into_command() {
        Command::Daemon(args) => match args.validate() {
            Ok(()) => {
                let idle_source = Box::new(MutterIdleSource::new(args.idle_timeout()));
                app::run_daemon(idle_source, args.dpms_timeout(), args.shader())
            }
            Err(msg) => Err(msg.into()),
        },
        Command::Start(args) => app::run(args.shader()),
        Command::Stop => pidfile::stop(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "howan exited with error");
            ExitCode::FAILURE
        }
    }
}
