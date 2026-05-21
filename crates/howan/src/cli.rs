//! Command-line interface for howan.
//!
//! howan runs as a resident daemon (`howan daemon`) that owns idle detection
//! and shows the saver autonomously; see `docs/guides/40-resident-daemon.md`.
//! The manual/debug `start` / `stop` pair is kept for showing the saver
//! immediately and terminating it (it predates the daemon and was the
//! swayidle-driven activation path, now superseded — see
//! `docs/guides/20-swayidle.md`):
//!
//! - `daemon` runs the long-lived process and shows the saver after `T1` idle.
//! - `start` launches the saver immediately and exits on input.
//! - `stop` terminates a running `start`.
//!
//! Running `howan` with no subcommand defaults to `start`, the common
//! interactive case ("just show the saver now"), matching the original M1
//! binary.

use std::time::Duration;

use clap::{Parser, Subcommand};

use crate::daemon::DEFAULT_T1;

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

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Run the resident daemon: detect idle and show the saver autonomously.
    Daemon(DaemonArgs),
    /// Launch the saver immediately (blocks until dismissed or stopped).
    Start,
    /// Terminate a running saver. A no-op success if none is running.
    Stop,
}

/// Arguments for `howan daemon`.
#[derive(Debug, Parser, PartialEq, Eq)]
pub struct DaemonArgs {
    /// How long the seat must be idle, in seconds, before the saver is shown
    /// (the design's `T1`). Defaults to 300 (5 minutes). Full duration-string /
    /// TOML configuration is a later milestone; this single override is the
    /// only knob for now.
    #[arg(long = "idle-timeout", value_name = "SECONDS")]
    idle_timeout_secs: Option<u64>,
}

impl DaemonArgs {
    /// The effective idle timeout (the design's `T1`), applying the 5-minute
    /// default.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_T1)
    }
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
        assert!(matches!(
            Cli::parse_from(["howan", "daemon"]).into_command(),
            Command::Daemon(_)
        ));
    }

    #[test]
    fn daemon_idle_timeout_defaults_to_five_minutes() {
        let cli = Cli::parse_from(["howan", "daemon"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert_eq!(args.idle_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn daemon_idle_timeout_override_is_parsed_in_seconds() {
        let cli = Cli::parse_from(["howan", "daemon", "--idle-timeout", "7"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert_eq!(args.idle_timeout(), Duration::from_secs(7));
    }
}
