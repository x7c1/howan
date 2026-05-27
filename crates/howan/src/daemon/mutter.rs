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
//! idle watch after each cycle. The daemon drives the re-arm, and the *kind*
//! of re-arm depends on which dismiss path ran:
//!
//! 1. Add an idle watch for `T1`; when it fires, emit [`IdleEvent::Idle`].
//! 2. Block until the daemon sends a [`RearmKind`] through the rearm channel:
//!    - [`RearmKind::Immediate`] — Phase 1 input dismiss. The user just
//!      produced input, so add a fresh `AddIdleWatch` right away.
//!    - [`RearmKind::AfterActive`] — Phase 3 DPMS handoff. The user is still
//!      idle (the timer fired *without* any input), so first add an
//!      `AddUserActiveWatch`, wait for it to fire on the next genuine
//!      idle→active transition, and only *then* add the next `AddIdleWatch`.
//!
//! ### Why an active-watch gate after Phase 3
//!
//! Without the gate, the daemon arms a fresh `AddIdleWatch` at `T_dpms` while
//! the seat is still idle. Because howan's `T1` is shorter than the
//! compositor's own `org.gnome.desktop.session idle-delay`, the new howan
//! watch fires first, re-shows the saver, re-acquires the inhibitor, and the
//! compositor never reaches DPMS off — the Phase 3 handoff is functionally
//! a no-op. `AddUserActiveWatch` (Mutter `org.gnome.Mutter.IdleMonitor`)
//! solves this cleanly: it fires once on the next idle→active transition, so
//! the next idle watch is added only when the user is back, after the
//! compositor's blank has had a chance to take effect.
//!
//! The input-dismiss path historically avoided `AddUserActiveWatch` because
//! a held idle inhibitor blinds Mutter's idle/active tracking. That objection
//! does not apply on the post-Phase-3 path: by the time `RearmKind::AfterActive`
//! reaches the watch loop, Phase 3 has already destroyed the `Saver` and
//! released the inhibitor, so the active watch sees unmasked state and fires
//! on real user activity. The Phase 1 input path still uses
//! `RearmKind::Immediate` for a different reason: the user is active by
//! definition there, and waiting for an active transition that has effectively
//! already happened would deadlock the next idle cycle.

use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use calloop::channel::Sender;
use tracing::{error, info, warn};
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

    /// Fire `WatchFired` once on the next idle → active transition. Used to
    /// gate the next idle watch on real user activity after a Phase 3 DPMS
    /// handoff (see the module docs).
    fn add_user_active_watch(&self) -> zbus::Result<u32>;

    /// Remove a previously added watch.
    fn remove_watch(&self, id: u32) -> zbus::Result<()>;

    /// Current idle time in milliseconds. Used only as a reachability probe.
    fn get_idletime(&self) -> zbus::Result<u64>;

    /// Emitted when a watch added above reaches its condition.
    #[zbus(signal)]
    fn watch_fired(&self, id: u32) -> zbus::Result<()>;
}

/// Which kind of re-arm the daemon is requesting after a dismiss.
///
/// Sent through the watch thread's rearm channel; the loop matches on it to
/// pick between "add an idle watch right now" (input dismiss) and "add a
/// user-active watch first, then an idle watch once it fires" (Phase 3 DPMS
/// handoff). See the module-level "Re-arm strategy" docs for the full
/// rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RearmKind {
    /// The user just produced input — re-add the idle watch immediately.
    Immediate,
    /// The Phase 3 timer dismissed the saver while the user was still idle —
    /// add a `AddUserActiveWatch` first, wait for it to fire, then add the
    /// next idle watch.
    AfterActive,
}

/// The GNOME Mutter idle source.
pub struct MutterIdleSource {
    /// Idle threshold `T1` in milliseconds.
    interval_ms: u64,
    /// Sender to the watch thread, populated by [`start`](IdleSource::start).
    /// [`rearm`](IdleSource::rearm) and
    /// [`rearm_after_active`](IdleSource::rearm_after_active) push a
    /// [`RearmKind`] through it to ask the thread for the next cycle.
    rearm_tx: Mutex<Option<mpsc::Sender<RearmKind>>>,
}

impl MutterIdleSource {
    /// Build a Mutter idle source with the given idle threshold `T1`.
    pub fn new(t1: Duration) -> Self {
        Self {
            interval_ms: t1.as_millis() as u64,
            rearm_tx: Mutex::new(None),
        }
    }

    /// Common path for both [`rearm`](IdleSource::rearm) and
    /// [`rearm_after_active`](IdleSource::rearm_after_active): forward a
    /// [`RearmKind`] to the watch thread if one is running, otherwise no-op
    /// (start has not been called yet — see `rearm_before_start_is_ok`).
    fn send_rearm(&self, kind: RearmKind) -> Result<(), Box<dyn Error>> {
        let guard = self.rearm_tx.lock().expect("rearm_tx mutex poisoned");
        if let Some(tx) = guard.as_ref() {
            tx.send(kind)
                .map_err(|err| format!("failed to signal idle re-arm ({kind:?}): {err}"))?;
        }
        Ok(())
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
    fn backend_name(&self) -> &'static str {
        "mutter"
    }

    fn t1(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }

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
        let (rearm_tx, rearm_rx) = mpsc::channel::<RearmKind>();
        *self.rearm_tx.lock().expect("rearm_tx mutex poisoned") = Some(rearm_tx);

        let handle = thread::Builder::new()
            .name("howan-idle-mutter".into())
            .spawn(move || {
                if let Err(err) = run_watch_loop(interval_ms, &sender, &thread_running, &rearm_rx) {
                    error!(error = %err, "Mutter idle watch loop ended");
                }
            })
            .map_err(|err| format!("failed to spawn idle watch thread: {err}"))?;

        Ok(Box::new(MutterIdleHandle {
            running,
            join: Some(handle),
        }))
    }

    fn rearm(&self) -> Result<(), Box<dyn Error>> {
        // Input-dismiss path; see the module "Re-arm strategy" docs.
        self.send_rearm(RearmKind::Immediate)
    }

    fn rearm_after_active(&self) -> Result<(), Box<dyn Error>> {
        // Phase 3 DPMS-handoff path; see the module "Re-arm strategy" docs.
        self.send_rearm(RearmKind::AfterActive)
    }
}

/// Drive the idle-watch / re-arm cycle until shutdown.
///
/// Each cycle has up to three blocking steps that never overlap, so no
/// cross-source `select` is needed. These are loop-internal steps, not the
/// saver's three `SaverPhase`s in `crate::app`; "step" is used here to avoid
/// the name collision.
///
/// 1. Wait for the armed idle watch to fire (D-Bus signal).
/// 2. Wait for the daemon to re-arm after dismiss (the `rearm_rx` channel).
/// 3. *Only when re-arm is [`RearmKind::AfterActive`]*: arm a
///    `AddUserActiveWatch`, wait for it to fire, then go back to step 1.
fn run_watch_loop(
    interval_ms: u64,
    sender: &Sender<IdleEvent>,
    running: &AtomicBool,
    rearm_rx: &mpsc::Receiver<RearmKind>,
) -> Result<(), Box<dyn Error>> {
    let connection =
        Connection::session().map_err(|err| format!("session bus connect failed: {err}"))?;
    let proxy = IdleMonitorProxyBlocking::new(&connection)
        .map_err(|err| format!("idle monitor proxy build failed: {err}"))?;

    // Listen for all WatchFired signals; we match the fired id against the
    // watch we currently expect.
    let mut signals = proxy
        .receive_watch_fired()
        .map_err(|err| format!("failed to subscribe to WatchFired: {err}"))?;

    // The watch currently armed, tracked only for best-effort cleanup on exit
    // (it is `None` while we are between cycles waiting on a re-arm). It can
    // be either the idle watch (step 1 below) or the user-active watch (added
    // at step 3 below for `RearmKind::AfterActive`), so a single slot is enough.
    let mut pending_watch: Option<u32> = None;

    // Which trigger armed the *current* idle watch — `initial` on the very
    // first arming, `dismiss` after a Phase 1 input dismiss, and
    // `add_user_active_watch` after a Phase 3 DPMS handoff (the active-watch
    // gate fired). Recorded only as a structured field on the `idle watch
    // armed` info event so the journal makes the re-arm path distinguishable
    // (Q4 vs. M3) — see docs/guides/40-resident-daemon.md ("Verifying the
    // daemon via the journal").
    let mut next_trigger: &'static str = "initial";

    'cycles: loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Step 1: arm an idle watch and wait for it to fire.
        let armed = proxy
            .add_idle_watch(interval_ms)
            .map_err(|err| format!("AddIdleWatch failed: {err}"))?;
        pending_watch = Some(armed);
        info!(
            watch_id = armed,
            interval_ms,
            trigger = next_trigger,
            "idle watch armed"
        );

        if !wait_for_watch(&mut signals, running, armed)? {
            break 'cycles;
        }

        // Idle threshold reached: tell the daemon to show the saver. The idle
        // watch is one-shot and now consumed.
        pending_watch = None;
        info!(t1_ms = interval_ms, "idle detected");
        if sender.send(IdleEvent::Idle).is_err() {
            // The daemon has exited; stop the loop.
            break;
        }

        // Step 2: block until the daemon re-arms us. It calls `rearm` after a
        // saver-Phase 1 input dismiss (immediate re-arm) or
        // `rearm_after_active` after a saver-Phase 3 DPMS handoff (gated on
        // the next user-active transition).
        let kind = match rearm_rx.recv() {
            Ok(kind) => kind,
            Err(_) => break, // the sender was dropped: daemon shutting down
        };

        // Step 3 (only for `AfterActive`): add an `AddUserActiveWatch` and
        // wait for it to fire before looping back to the top to arm the next
        // idle watch. By the time we reach this point the saver-Phase 3
        // dismiss has already released the idle inhibitor (see `Saver`'s
        // `Drop`), so Mutter's idle/active tracking is unmasked and the watch
        // fires on real user activity.
        if matches!(kind, RearmKind::AfterActive) {
            let active = proxy
                .add_user_active_watch()
                .map_err(|err| format!("AddUserActiveWatch failed: {err}"))?;
            pending_watch = Some(active);
            info!(watch_id = active, "user-active watch armed");
            if !wait_for_watch(&mut signals, running, active)? {
                break 'cycles;
            }
            pending_watch = None;
            info!("user-active watch fired");
            next_trigger = "add_user_active_watch";
        } else {
            next_trigger = "dismiss";
        }
    }

    // Best-effort cleanup of any outstanding watch.
    if let Some(id) = pending_watch {
        let _ = proxy.remove_watch(id);
    }
    Ok(())
}

/// Block on the `WatchFired` signal stream until the watch with id
/// `expected` fires, or we are asked to stop, or the signal stream ends.
///
/// Returns `Ok(true)` on the expected fire and `Ok(false)` when shutdown was
/// requested or the signal stream ended (both mean "break the outer cycle";
/// stream-end is treated as benign shutdown, matching the previous in-line
/// behavior). The `Err` arm is unreachable today — the return type leaves
/// room for a future fallible check inside the loop without growing the
/// call sites.
fn wait_for_watch(
    signals: &mut WatchFiredIterator,
    running: &AtomicBool,
    expected: u32,
) -> Result<bool, Box<dyn Error>> {
    loop {
        let Some(signal) = signals.next() else {
            // The signal stream ended (bus closed): nothing more to wait on.
            // Propagate as "stop the outer loop" rather than as an error,
            // matching the previous behavior.
            return Ok(false);
        };
        if !running.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let args = match signal.args() {
            Ok(args) => args,
            Err(err) => {
                warn!(error = %err, "malformed WatchFired signal");
                continue;
            }
        };
        if args.id == expected {
            return Ok(true);
        }
        // A stale watch id (e.g. from a previous cycle, or the other watch
        // type in the same cycle): ignore.
    }
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

    #[test]
    fn rearm_after_active_before_start_is_ok() {
        // The Phase 3 re-arm primitive shares the same "signal-or-noop" shape
        // as `rearm`: before `start` has run the watch thread does not exist,
        // so there is nothing to push the `RearmKind::AfterActive` message to
        // and the call must succeed without error. Once `start` has run, the
        // signal drives the user-active-watch gate in `run_watch_loop` (see
        // the module docs).
        let source = MutterIdleSource::new(Duration::from_secs(1));
        assert!(source.rearm_after_active().is_ok());
    }
}
