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

/// Default `T_dpms` (Phase 1 → Phase 3 boundary) in seconds: 2 hours.
/// After the saver has been shown this long, the daemon releases the idle
/// inhibitor while keeping the saver surface mapped, so the compositor's
/// standard idle blank takes over behind the saver. See
/// `docs/guides/40-resident-daemon.md`.
const DEFAULT_T_DPMS_SECS: u64 = 2 * 60 * 60;

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
    /// TOML configuration is a later milestone; this and the phase threshold
    /// below are the only knobs for now.
    #[arg(long = "idle-timeout", value_name = "SECONDS")]
    idle_timeout_secs: Option<u64>,

    /// How long the saver may stay up before the daemon releases the idle
    /// inhibitor (while keeping the saver surface mapped) to let the
    /// compositor's standard idle blank take over (Phase 3), in seconds (the
    /// design's `T_dpms`, measured from the moment the saver is shown).
    /// Defaults to 7200 (2 hours). Must be greater than zero.
    #[arg(long = "dpms-timeout", value_name = "SECONDS")]
    dpms_timeout_secs: Option<u64>,
}

impl DaemonArgs {
    /// The effective idle timeout (the design's `T1`), applying the 5-minute
    /// default.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_T1)
    }

    /// The effective DPMS-handoff timeout (the design's `T_dpms`), applying
    /// the 2-hour default.
    pub fn dpms_timeout(&self) -> Duration {
        self.dpms_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_T_DPMS_SECS))
    }

    /// Reject a degenerate `T_dpms` of zero: it would fire the handoff timer
    /// at saver-show, collapsing Phase 1 to nothing. Called from `main`
    /// before the daemon starts.
    pub fn validate(&self) -> Result<(), String> {
        let dpms = self.dpms_timeout();
        if dpms.is_zero() {
            return Err("--dpms-timeout must be greater than zero".to_string());
        }
        Ok(())
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

    #[test]
    fn daemon_dpms_timeout_defaults_to_two_hours() {
        let cli = Cli::parse_from(["howan", "daemon"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert_eq!(args.dpms_timeout(), Duration::from_secs(7200));
    }

    #[test]
    fn daemon_dpms_timeout_override_is_parsed_in_seconds() {
        let cli = Cli::parse_from(["howan", "daemon", "--dpms-timeout", "60"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert_eq!(args.dpms_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn daemon_validate_rejects_zero_dpms_timeout() {
        let cli = Cli::parse_from(["howan", "daemon", "--dpms-timeout", "0"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn daemon_validate_accepts_positive_dpms_timeout() {
        let cli = Cli::parse_from(["howan", "daemon", "--dpms-timeout", "60"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert!(args.validate().is_ok());
    }

    #[test]
    fn daemon_removed_phase2_flag_is_rejected() {
        // The previous Phase 2 flag was removed when locking was delegated
        // to GNOME (Q-phase2-lock). Clap must reject it as an unknown
        // argument so users with a pre-existing override notice the
        // change instead of having it silently ignored.
        assert!(
            Cli::try_parse_from(["howan", "daemon", "--grace-timeout", "30"]).is_err(),
            "the removed Phase 2 flag must be rejected as an unknown argument"
        );
    }
}
