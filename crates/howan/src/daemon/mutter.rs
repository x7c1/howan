//! GNOME idle backend over `org.gnome.Mutter.IdleMonitor` (D-Bus).
//!
//! Mutter does not implement the `ext-idle-notify-v1` Wayland protocol, so
//! `swayidle` cannot detect idle on GNOME (it exits with `Compositor doesn't
//! support idle protocol`). Mutter does, however, expose idle *detection* on
//! the session bus through `org.gnome.Mutter.IdleMonitor`, which this backend
//! uses. See `docs/guides/40-resident-daemon.md`.
//!
//! ## Interface
//!
//! Bus name `org.gnome.Mutter.IdleMonitor`, object
//! `/org/gnome/Mutter/IdleMonitor/Core`, interface
//! `org.gnome.Mutter.IdleMonitor`. The methods used:
//!
//! - `AddIdleWatch(UInt64 interval_ms) -> UInt32 id` — fires `WatchFired(id)`
//!   once when the seat has been idle for `interval_ms`.
//! - `AddUserActiveWatch() -> UInt32 id` — fires `WatchFired(id)` once the next
//!   time the user becomes active. Used to re-arm the idle watch for the next
//!   cycle.
//! - `RemoveWatch(UInt32 id)` — drop a watch.
//! - `GetIdletime() -> UInt64` — current idle time in ms; used as a cheap
//!   reachability probe.
//!
//! ## Re-arm strategy
//!
//! `AddIdleWatch` is one-shot: it fires once and does not re-fire on subsequent
//! idle periods. To produce an idle event on *every* idle period we run a small
//! state machine on the backend thread:
//!
//! 1. Add an idle watch for `T1`.
//! 2. When it fires, emit [`IdleEvent::Idle`] and immediately add a
//!    *user-active* watch.
//! 3. When the user-active watch fires (the user moved/typed — i.e. the saver
//!    was dismissed), add a fresh idle watch for `T1` again.
//!
//! This loops indefinitely, so each idle period yields exactly one event. The
//! daemon's explicit [`IdleSource::rearm`] is therefore a no-op for this backend
//! (the thread already re-arms via the user-active watch), but it is kept so the
//! trait contract holds and a polling backend could use it.

use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use calloop::channel::Sender;
use zbus::blocking::Connection;

use super::{IdleEvent, IdleHandle, IdleSource};

/// Blocking proxy for `org.gnome.Mutter.IdleMonitor`.
#[zbus::proxy(
    interface = "org.gnome.Mutter.IdleMonitor",
    default_service = "org.gnome.Mutter.IdleMonitor",
    default_path = "/org/gnome/Mutter/IdleMonitor/Core"
)]
trait IdleMonitor {
    /// Fire `WatchFired` once after the seat has been idle for `interval` ms.
    fn add_idle_watch(&self, interval: u64) -> zbus::Result<u32>;

    /// Fire `WatchFired` once the next time the user becomes active.
    fn add_user_active_watch(&self) -> zbus::Result<u32>;

    /// Remove a previously added watch.
    fn remove_watch(&self, id: u32) -> zbus::Result<()>;

    /// Current idle time in milliseconds. Used only as a reachability probe.
    fn get_idletime(&self) -> zbus::Result<u64>;

    /// Emitted when a watch added above reaches its condition.
    #[zbus(signal)]
    fn watch_fired(&self, id: u32) -> zbus::Result<()>;
}

/// The GNOME Mutter idle source.
pub struct MutterIdleSource {
    /// Idle threshold `T1` in milliseconds.
    interval_ms: u64,
}

impl MutterIdleSource {
    /// Build a Mutter idle source with the given idle threshold `T1`.
    pub fn new(t1: Duration) -> Self {
        Self {
            interval_ms: t1.as_millis() as u64,
        }
    }

    /// Connect to the session bus and confirm the Mutter IdleMonitor interface
    /// is actually reachable, returning a ready proxy.
    ///
    /// This is called synchronously from [`start`](IdleSource::start) so a
    /// non-GNOME session (where the interface is absent) fails fast with a clear
    /// diagnostic instead of leaving the daemon hanging.
    fn connect() -> Result<IdleMonitorProxyBlocking<'static>, Box<dyn Error>> {
        let connection = Connection::session().map_err(|err| {
            format!("failed to connect to the D-Bus session bus: {err}")
        })?;
        let proxy = IdleMonitorProxyBlocking::new(&connection).map_err(|err| {
            format!("failed to build org.gnome.Mutter.IdleMonitor proxy: {err}")
        })?;
        // Probe the interface so an unreachable IdleMonitor (e.g. a non-GNOME
        // session) surfaces here rather than as a silent no-fire.
        proxy.get_idletime().map_err(|err| {
            format!(
                "org.gnome.Mutter.IdleMonitor is not available on this session \
                 (is this a GNOME/Mutter session?): {err}"
            )
        })?;
        Ok(proxy)
    }
}

impl IdleSource for MutterIdleSource {
    fn start(&self, sender: Sender<IdleEvent>) -> Result<Box<dyn IdleHandle>, Box<dyn Error>> {
        // Validate reachability on the calling thread so the error propagates to
        // the daemon's exit code. The watch loop then runs on its own
        // connection/thread.
        let _probe = Self::connect()?;

        let interval_ms = self.interval_ms;
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);

        let handle = thread::Builder::new()
            .name("howan-idle-mutter".into())
            .spawn(move || {
                if let Err(err) = run_watch_loop(interval_ms, &sender, &thread_running) {
                    eprintln!("howan: Mutter idle watch loop ended: {err}");
                }
            })
            .map_err(|err| format!("failed to spawn idle watch thread: {err}"))?;

        Ok(Box::new(MutterIdleHandle {
            running,
            join: Some(handle),
        }))
    }

    fn rearm(&self) -> Result<(), Box<dyn Error>> {
        // No-op: the backend thread re-arms itself via the user-active watch
        // (see the module docs). Kept to honor the trait contract.
        Ok(())
    }
}

/// Drive the idle/user-active watch state machine until `running` is cleared.
fn run_watch_loop(
    interval_ms: u64,
    sender: &Sender<IdleEvent>,
    running: &AtomicBool,
) -> Result<(), Box<dyn Error>> {
    let connection =
        Connection::session().map_err(|err| format!("session bus connect failed: {err}"))?;
    let proxy = IdleMonitorProxyBlocking::new(&connection)
        .map_err(|err| format!("idle monitor proxy build failed: {err}"))?;

    // Listen for all WatchFired signals; we match the fired id against the watch
    // we currently expect.
    let signals = proxy
        .receive_watch_fired()
        .map_err(|err| format!("failed to subscribe to WatchFired: {err}"))?;

    let mut idle_watch = proxy
        .add_idle_watch(interval_ms)
        .map_err(|err| format!("AddIdleWatch failed: {err}"))?;
    let mut active_watch: Option<u32> = None;

    for signal in signals {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        let args = match signal.args() {
            Ok(args) => args,
            Err(err) => {
                eprintln!("howan: malformed WatchFired signal: {err}");
                continue;
            }
        };
        let fired = args.id;

        if fired == idle_watch {
            // Idle threshold reached: tell the daemon to show the saver. If the
            // channel is gone the daemon has exited; stop the loop.
            if sender.send(IdleEvent::Idle).is_err() {
                break;
            }
            // Arm a user-active watch so we learn when the user returns and can
            // re-arm the idle watch for the next cycle. Without it the loop
            // could never re-arm (the one-shot idle watch is already consumed),
            // so a failure here is fatal to the loop rather than swallowed —
            // ending the loop surfaces the "watch loop ended" diagnostic.
            match proxy.add_user_active_watch() {
                Ok(id) => active_watch = Some(id),
                Err(err) => {
                    return Err(format!("AddUserActiveWatch failed: {err}").into());
                }
            }
        } else if Some(fired) == active_watch {
            // The user became active (saver dismissed). The active watch is
            // one-shot and now consumed; re-arm the idle watch for the next
            // idle period.
            active_watch = None;
            match proxy.add_idle_watch(interval_ms) {
                Ok(id) => idle_watch = id,
                Err(err) => {
                    return Err(format!("re-arming AddIdleWatch failed: {err}").into());
                }
            }
        }
    }

    // Best-effort cleanup of any outstanding watches.
    let _ = proxy.remove_watch(idle_watch);
    if let Some(id) = active_watch {
        let _ = proxy.remove_watch(id);
    }
    Ok(())
}

/// Keeps the Mutter watch thread alive; dropping it stops the thread.
struct MutterIdleHandle {
    running: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl IdleHandle for MutterIdleHandle {}

impl Drop for MutterIdleHandle {
    fn drop(&mut self) {
        // Ask the loop to stop. The signal iterator may still block on the next
        // signal, so we do not join indefinitely — detach instead. The process
        // is shutting down, so the thread will be reaped with it.
        self.running.store(false, Ordering::Relaxed);
        // Drop the join handle without blocking; the watch thread is a daemon
        // helper and exits with the process.
        let _ = self.join.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_is_milliseconds_of_t1() {
        let source = MutterIdleSource::new(Duration::from_secs(5));
        assert_eq!(source.interval_ms, 5_000);
    }

    #[test]
    fn rearm_is_a_noop_for_mutter() {
        let source = MutterIdleSource::new(Duration::from_secs(1));
        assert!(source.rearm().is_ok());
    }
}
