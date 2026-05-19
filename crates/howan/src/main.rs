//! howan: a lightweight Wayland screensaver.
//!
//! M1 entry point. Opens a fullscreen window with a solid black background and
//! exits cleanly on the first keyboard, pointer, or touch input.

mod app;

use std::process::ExitCode;

fn main() -> ExitCode {
    match app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("howan: {err}");
            ExitCode::FAILURE
        }
    }
}
