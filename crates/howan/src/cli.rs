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

/// Default `T_grace` (Phase 1 → Phase 2 boundary) in seconds: 1 hour.
/// After the saver has been shown this long, input switches from a plain
/// dismiss to a `loginctl lock-session`-equivalent handoff to the
/// compositor's lock screen. See `docs/guides/40-resident-daemon.md`.
const DEFAULT_T_GRACE_SECS: u64 = 60 * 60;

/// Default `T_dpms` (Phase 2 → Phase 3 boundary) in seconds: 2 hours.
/// After the saver has been shown this long, the daemon releases the idle
/// inhibitor and destroys the saver surface so the compositor's standard idle
/// blank takes over. See `docs/guides/40-resident-daemon.md`.
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
    /// TOML configuration is a later milestone; this and the two phase
    /// thresholds below are the only knobs for now.
    #[arg(long = "idle-timeout", value_name = "SECONDS")]
    idle_timeout_secs: Option<u64>,

    /// How long the saver may stay up before input switches from a plain
    /// dismiss (Phase 1) to a lock-session handoff (Phase 2), in seconds (the
    /// design's `T_grace`, measured from the moment the saver is shown).
    /// Defaults to 3600 (1 hour). Must be less than `--dpms-timeout`.
    #[arg(long = "grace-timeout", value_name = "SECONDS")]
    grace_timeout_secs: Option<u64>,

    /// How long the saver may stay up before the daemon releases the idle
    /// inhibitor and destroys the saver surface to let the compositor's
    /// standard idle blank take over (Phase 3), in seconds (the design's
    /// `T_dpms`, measured from the moment the saver is shown). Defaults to
    /// 7200 (2 hours). Must be greater than `--grace-timeout`.
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

    /// The effective grace timeout (the design's `T_grace`), applying the
    /// 1-hour default.
    pub fn grace_timeout(&self) -> Duration {
        self.grace_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_T_GRACE_SECS))
    }

    /// The effective DPMS-handoff timeout (the design's `T_dpms`), applying
    /// the 2-hour default.
    pub fn dpms_timeout(&self) -> Duration {
        self.dpms_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_T_DPMS_SECS))
    }

    /// Reject combinations where the phase windows are degenerate, i.e.
    /// `T_dpms <= T_grace`. Phase 2 must have a non-empty window between the
    /// two boundaries for the lock-handoff branch to be reachable; collapsing
    /// the windows would silently make Phase 2 unreachable. Called from `main`
    /// before the daemon starts.
    pub fn validate(&self) -> Result<(), String> {
        let grace = self.grace_timeout();
        let dpms = self.dpms_timeout();
        if dpms <= grace {
            return Err(format!(
                "--dpms-timeout ({}s) must be greater than --grace-timeout ({}s)",
                dpms.as_secs(),
                grace.as_secs()
            ));
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
    fn daemon_grace_timeout_defaults_to_one_hour() {
        let cli = Cli::parse_from(["howan", "daemon"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert_eq!(args.grace_timeout(), Duration::from_secs(3600));
    }

    #[test]
    fn daemon_grace_timeout_override_is_parsed_in_seconds() {
        let cli = Cli::parse_from(["howan", "daemon", "--grace-timeout", "30"]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert_eq!(args.grace_timeout(), Duration::from_secs(30));
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
    fn daemon_validate_rejects_dpms_le_grace() {
        // Equal: degenerate window — Phase 2 unreachable.
        let cli = Cli::parse_from([
            "howan",
            "daemon",
            "--grace-timeout",
            "60",
            "--dpms-timeout",
            "60",
        ]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert!(args.validate().is_err());

        // Strictly less than: dpms before grace makes no sense.
        let cli = Cli::parse_from([
            "howan",
            "daemon",
            "--grace-timeout",
            "60",
            "--dpms-timeout",
            "30",
        ]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn daemon_validate_accepts_dpms_gt_grace() {
        let cli = Cli::parse_from([
            "howan",
            "daemon",
            "--grace-timeout",
            "30",
            "--dpms-timeout",
            "60",
        ]);
        let Command::Daemon(args) = cli.into_command() else {
            panic!("expected daemon command");
        };
        assert!(args.validate().is_ok());
    }
}
