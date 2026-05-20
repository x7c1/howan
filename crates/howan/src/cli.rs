//! Command-line interface for howan.
//!
//! howan is driven by `swayidle`, which invokes a pair of commands:
//!
//! ```text
//! swayidle -w timeout 300 'howan start' resume 'howan stop'
//! ```
//!
//! so the CLI only needs two subcommands for M2: `start` launches the
//! fullscreen saver, `stop` terminates a running saver. Running `howan` with no
//! subcommand defaults to `start`, which is the common interactive case ("just
//! show the saver now") and matches what the M1 binary did before the CLI
//! existed.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "howan",
    about = "A lightweight Wayland screensaver",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Launch the fullscreen saver (blocks until dismissed or stopped).
    Start,
    /// Terminate a running saver. A no-op success if none is running.
    Stop,
}

impl Cli {
    /// The effective command, applying the "no subcommand means `start`"
    /// default.
    pub fn into_command(self) -> Command {
        self.command.unwrap_or(Command::Start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn verify_cli() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_defaults_to_start() {
        let cli = Cli::parse_from(["howan"]);
        assert!(matches!(cli.into_command(), Command::Start));
    }

    #[test]
    fn explicit_subcommands_parse() {
        assert!(matches!(
            Cli::parse_from(["howan", "start"]).into_command(),
            Command::Start
        ));
        assert!(matches!(
            Cli::parse_from(["howan", "stop"]).into_command(),
            Command::Stop
        ));
    }
}
