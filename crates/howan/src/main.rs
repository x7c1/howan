//! howan: a lightweight Wayland screensaver.
//!
//! Entry point and CLI dispatch. `howan start` launches the saver — an
//! output-sized composited window with a solid black background that exits on
//! the first keyboard, pointer, or touch input; `howan stop` terminates a
//! running saver. The two halves talk through a PID file so swayidle's separate
//! `timeout`/`resume` hooks can reach the same instance. See
//! `docs/guides/20-swayidle.md`.

mod app;
mod cli;
mod pidfile;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

fn main() -> ExitCode {
    let result = match Cli::parse().into_command() {
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
