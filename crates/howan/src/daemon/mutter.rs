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
//! - `RemoveWatch(UInt32 id)` — drop a watch.
//! - `GetIdletime() -> UInt64` — current idle time in ms; used as a cheap
//!   reachability probe.
//!
//! ## Re-arm strategy
//!
//! `AddIdleWatch` is one-shot: it fires once and does not re-fire on subsequent
//! idle periods. To produce an idle event on *every* idle period we re-add an
//! idle watch after each cycle. The re-arm is driven by the daemon, not by
//! Mutter's `AddUserActiveWatch`:
//!
//! 1. Add an idle watch for `T1`; when it fires, emit [`IdleEvent::Idle`].
//! 2. Block until the daemon calls [`IdleSource::rearm`] — which it does after
//!    the saver is dismissed by input and its idle inhibitor has been released —
//!    then add a fresh idle watch and repeat.
//!
//! We deliberately do **not** use `AddUserActiveWatch` to re-arm. While the
//! saver is shown the daemon holds a `zwp_idle_inhibit_manager_v1` inhibitor
//! (see the guide), which makes Mutter treat the session as non-idle and so
//! blinds Mutter's own idle/active tracking: a user-active watch armed while the
//! inhibitor is held does not fire on the real dismiss, which previously left
//! the loop unable to re-arm — the saver showed once and never reappeared.
//! Re-arming from the dismiss event is reliable instead: by the time the daemon
//! calls `rearm` the saver surface and its inhibitor are gone, so a fresh idle
//! watch counts idle normally. (You cannot both inhibit idle and detect idle
//! through the same Mutter IdleMonitor at once; the daemon, which knows exactly
//! when the saver was dismissed, is the right place to drive the re-arm.)

use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
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
    /// Sender to the watch thread, populated by [`start`](IdleSource::start).
    /// [`rearm`](IdleSource::rearm) pushes a unit through it to ask the thread
    /// to add a fresh idle watch for the next cycle.
    rearm_tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl MutterIdleSource {
    /// Build a Mutter idle source with the given idle threshold `T1`.
    pub fn new(t1: Duration) -> Self {
        Self {
            interval_ms: t1.as_millis() as u64,
            rearm_tx: Mutex::new(None),
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

        // The daemon signals re-arm (after a dismiss) through this channel; the
        // watch thread blocks on the receiver between idle cycles.
        let (rearm_tx, rearm_rx) = mpsc::channel::<()>();
        *self.rearm_tx.lock().expect("rearm_tx mutex poisoned") = Some(rearm_tx);

        let handle = thread::Builder::new()
            .name("howan-idle-mutter".into())
            .spawn(move || {
                if let Err(err) = run_watch_loop(interval_ms, &sender, &thread_running, &rearm_rx) {
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
        // Re-arm is driven from here (the dismiss path) rather than a Mutter
        // user-active watch, because the idle inhibitor held while the saver is
        // shown blinds Mutter's idle/active tracking (see the module docs).
        // Signal the watch thread to add a fresh idle watch. If `start` has not
        // run yet there is nothing to re-arm.
        let guard = self.rearm_tx.lock().expect("rearm_tx mutex poisoned");
        if let Some(tx) = guard.as_ref() {
            tx.send(())
                .map_err(|err| format!("failed to signal idle re-arm: {err}"))?;
        }
        Ok(())
    }
}

/// Drive the idle-watch / re-arm cycle until shutdown.
///
/// Each cycle has two blocking phases that never overlap, so no cross-source
/// `select` is needed: wait for an idle watch to fire (D-Bus signal), then wait
/// for the daemon to re-arm after the dismiss (the `rearm_rx` channel).
fn run_watch_loop(
    interval_ms: u64,
    sender: &Sender<IdleEvent>,
    running: &AtomicBool,
    rearm_rx: &mpsc::Receiver<()>,
) -> Result<(), Box<dyn Error>> {
    let connection =
        Connection::session().map_err(|err| format!("session bus connect failed: {err}"))?;
    let proxy = IdleMonitorProxyBlocking::new(&connection)
        .map_err(|err| format!("idle monitor proxy build failed: {err}"))?;

    // Listen for all WatchFired signals; we match the fired id against the idle
    // watch we currently expect.
    let mut signals = proxy
        .receive_watch_fired()
        .map_err(|err| format!("failed to subscribe to WatchFired: {err}"))?;

    // The idle watch currently armed, tracked only for best-effort cleanup on
    // exit (it is `None` while we are between cycles waiting on a re-arm).
    let mut idle_watch: Option<u32> = None;

    'cycles: loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Phase 1: arm an idle watch and wait for it to fire.
        let armed = proxy
            .add_idle_watch(interval_ms)
            .map_err(|err| format!("AddIdleWatch failed: {err}"))?;
        idle_watch = Some(armed);

        loop {
            let Some(signal) = signals.next() else {
                // The signal stream ended (bus closed): nothing more to wait on.
                return Ok(());
            };
            if !running.load(Ordering::Relaxed) {
                break 'cycles;
            }
            let args = match signal.args() {
                Ok(args) => args,
                Err(err) => {
                    eprintln!("howan: malformed WatchFired signal: {err}");
                    continue;
                }
            };
            if args.id == armed {
                break;
            }
            // A stale watch id (e.g. from a previous cycle): ignore.
        }

        // Idle threshold reached: tell the daemon to show the saver. The idle
        // watch is one-shot and now consumed.
        idle_watch = None;
        if sender.send(IdleEvent::Idle).is_err() {
            // The daemon has exited; stop the loop.
            break;
        }

        // Phase 2: block until the daemon re-arms us. It calls `rearm` after the
        // saver is dismissed by input and its idle inhibitor is released, so the
        // fresh idle watch added at the top of the next cycle counts normally.
        match rearm_rx.recv() {
            Ok(()) => {}     // re-arm for the next idle period
            Err(_) => break, // the sender was dropped: daemon shutting down
        }
    }

    // Best-effort cleanup of any outstanding idle watch.
    if let Some(id) = idle_watch {
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
        // is shutting down, so the thread will be reaped with it. (When the
        // thread is parked on the re-arm channel, dropping the source's sender
        // wakes it; that happens as the daemon tears down its `IdleSource`.)
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
    fn rearm_before_start_is_ok() {
        // With no watch thread started yet there is nothing to signal, so rearm
        // is a benign no-op. Once `start` has run, rearm signals the watch
        // thread to add a fresh idle watch (see the module docs).
        let source = MutterIdleSource::new(Duration::from_secs(1));
        assert!(source.rearm().is_ok());
    }
}
