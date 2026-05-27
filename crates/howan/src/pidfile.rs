//! PID-file based IPC between `howan start` and `howan stop`.
//!
//! `swayidle` runs the `resume` hook (`howan stop`) as a process separate from
//! the `timeout` hook (`howan start`), so `stop` needs a way to find the
//! already-running saver and ask it to quit. The simplest design that carries
//! forward to later milestones is a PID file: `start` writes its own PID on
//! launch and removes it on exit; `stop` reads it and sends `SIGTERM`.
//!
//! The file lives at `$XDG_RUNTIME_DIR/howan.pid`. `XDG_RUNTIME_DIR` is the
//! correct home for transient per-user runtime state and is cleaned up on
//! logout. When it is unset (e.g. a bare login shell or a non-systemd session)
//! we fall back to the system temp dir so the mechanism still works.

use std::env;
use std::error::Error;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process;

use tracing::warn;

const PID_FILE_NAME: &str = "howan.pid";

/// Resolve the path of the PID file (`$XDG_RUNTIME_DIR/howan.pid`, or the
/// system temp dir when `XDG_RUNTIME_DIR` is unset; see the module header).
pub fn pid_file_path() -> PathBuf {
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    dir.join(PID_FILE_NAME)
}

/// A PID file owned by the current `start` process.
///
/// Writing the file is tied to this guard's lifetime: dropping it removes the
/// file. `run()` keeps the guard alive for the duration of the event loop, so
/// the file is cleaned up on every exit path, including unwinding on error.
pub struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    /// Write the current process PID to the PID file, returning a guard that
    /// removes the file on drop.
    ///
    /// If a live `howan` instance already owns the file we do not clobber it:
    /// M2 does not need singleton enforcement (swayidle will not fire
    /// `timeout` twice without an intervening `resume`), but silently
    /// overwriting another instance's PID would strand it — `stop` could then
    /// never reach it. A stale file (owner no longer alive) is replaced.
    pub fn acquire() -> Result<Self, Box<dyn Error>> {
        let path = pid_file_path();
        if let Some(pid) = read_pid(&path)? {
            if process_is_alive(pid) {
                return Err(format!(
                    "another howan instance is already running (pid {pid}); \
                     run `howan stop` first or remove {}",
                    path.display()
                )
                .into());
            }
        }
        fs::write(&path, format!("{}\n", process::id()))
            .map_err(|err| format!("failed to write pid file {}: {err}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        // Best effort: a failure here must not panic during unwind, and a
        // missing file is the expected steady state, not worth a log line.
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => warn!(
                path = %self.path.display(),
                error = %err,
                "failed to remove pid file"
            ),
        }
    }
}

/// Read the PID stored in the file, if any.
///
/// Returns `Ok(None)` when the file does not exist or holds no parseable PID
/// (a truncated or garbage file is treated as "no live instance" rather than a
/// hard error, since the only safe action is to ignore it).
fn read_pid(path: &std::path::Path) -> Result<Option<i32>, Box<dyn Error>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.trim().parse::<i32>().ok()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to read pid file {}: {err}", path.display()).into()),
    }
}

/// Check whether a process with the given PID is alive, using `kill(pid, 0)`.
///
/// `kill` with signal 0 performs permission/existence checks without delivering
/// a signal. `ESRCH` means the process is gone (stale PID); any other outcome
/// (success, or `EPERM` from a PID we are not allowed to signal) means it
/// exists.
fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` is a thin libc wrapper; passing signal 0 only probes for
    // the process and never mutates our address space.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Terminate a running `howan start` instance, if one exists.
///
/// This is the body of `howan stop`. It is a no-op success when there is no
/// running instance: a missing PID file, an unparseable file, or a stale PID
/// (process already gone) all return `Ok(())` without printing an alarming
/// error, because "nothing to stop" is a normal outcome of the resume hook.
pub fn stop() -> Result<(), Box<dyn Error>> {
    let path = pid_file_path();
    let Some(pid) = read_pid(&path)? else {
        return Ok(());
    };

    if !process_is_alive(pid) {
        // Stale file left by a crashed instance: clean it up so a subsequent
        // `stop` stays a clean no-op, then report success.
        remove_stale(&path);
        return Ok(());
    }

    // SAFETY: thin libc wrapper; SIGTERM is delivered to the target process and
    // does not touch our address space.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // The process exited between the liveness check and here.
            Some(code) if code == libc::ESRCH => {
                remove_stale(&path);
                Ok(())
            }
            _ => Err(format!("failed to signal howan instance (pid {pid}): {err}").into()),
        }
    } else {
        // The running instance removes its own PID file as it unwinds; we do
        // not race to delete it here.
        Ok(())
    }
}

/// Remove a PID file we have determined to be stale, ignoring a concurrent
/// removal.
fn remove_stale(path: &std::path::Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => warn!(
            path = %path.display(),
            error = %err,
            "failed to remove stale pid file"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_path_prefers_xdg_runtime_dir() {
        // Saving/restoring env is inherently process-global; this test runs
        // alone in its module and restores what it changed.
        let original = env::var_os("XDG_RUNTIME_DIR");
        env::set_var("XDG_RUNTIME_DIR", "/run/user/test");
        assert_eq!(pid_file_path(), PathBuf::from("/run/user/test/howan.pid"));
        match original {
            Some(value) => env::set_var("XDG_RUNTIME_DIR", value),
            None => env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn process_is_alive_for_self_and_false_for_non_positive_pid() {
        assert!(process_is_alive(process::id() as i32));
        // A non-positive PID is never a real single process to signal.
        assert!(!process_is_alive(0));
        assert!(!process_is_alive(-1));
    }

    #[test]
    fn read_pid_missing_file_is_none() {
        let path = env::temp_dir().join("howan-test-definitely-missing.pid");
        let _ = fs::remove_file(&path);
        assert_eq!(read_pid(&path).unwrap(), None);
    }

    #[test]
    fn read_pid_garbage_is_none() {
        let path = env::temp_dir().join("howan-test-garbage.pid");
        fs::write(&path, "not-a-pid\n").unwrap();
        assert_eq!(read_pid(&path).unwrap(), None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn absent_pid_file_reads_as_no_instance() {
        // This is the precondition that makes `stop()` a no-op: with no PID
        // file, `read_pid` yields `None` and `stop()` returns early. We assert
        // that precondition directly rather than calling `stop()` against the
        // shared default path, which would disturb a real session.
        let dir = env::temp_dir().join("howan-test-stop-noop");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("howan.pid");
        let _ = fs::remove_file(&path);
        assert_eq!(read_pid(&path).unwrap(), None);
    }
}
