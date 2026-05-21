//! howan: a lightweight Wayland screensaver.
//!
//! Entry point and CLI dispatch. The primary mode is `howan daemon`: a resident
//! process that owns idle detection (GNOME's `org.gnome.Mutter.IdleMonitor` over
//! D-Bus) and shows the saver — an output-sized composited window with a solid
//! black background — autonomously when the seat has been idle for `T1`. Input
//! dismisses the saver but the daemon stays resident and re-arms for the next
//! idle period. See `docs/guides/40-resident-daemon.md`.
//!
//! `howan start` shows the saver immediately and exits on the first input;
//! `howan stop` terminates a running `start`. They talk through a PID file and
//! are kept for manual testing (the original swayidle-driven activation, now
//! superseded by the daemon; see `docs/guides/20-swayidle.md`).

mod app;
mod cli;
mod daemon;
mod pidfile;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
use daemon::mutter::MutterIdleSource;

fn main() -> ExitCode {
    let result = match Cli::parse().into_command() {
        Command::Daemon(args) => {
            let idle_source = Box::new(MutterIdleSource::new(args.idle_timeout()));
            app::run_daemon(idle_source)
        }
        Command::Start => app::run(),
        Command::Stop => pidfile::stop(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("howan: {err}");
            ExitCode::FAILURE
        }
    }
}
