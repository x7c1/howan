//! Idle detection for the resident daemon.
//!
//! The daemon ([`crate::app::run_daemon`]) does not know *how* idleness is
//! detected — it consumes "the seat has been idle for `T1`" events through the
//! [`IdleSource`] trait. This decouples the Wayland/calloop loop from the idle
//! transport so a future backend can be added by writing a new `IdleSource`
//! implementation without touching the loop.
//!
//! Exactly one backend is implemented today: [`mutter::MutterIdleSource`],
//! which talks to GNOME's `org.gnome.Mutter.IdleMonitor` D-Bus interface.
//! Mutter does not implement `ext-idle-notify-v1`, so `swayidle` cannot be used
//! on GNOME — see `docs/guides/40-resident-daemon.md`. A wlroots
//! `ext-idle-notify-v1` backend is explicitly out of scope here; the trait seam
//! is the place it would slot in.
//!
//! ## Threading model
//!
//! Idle backends typically run on their own thread (D-Bus is async/blocking)
//! and forward events into the single-threaded calloop loop via a
//! [`calloop::channel`]. `IdleSource::start` is handed the sender and returns an
//! opaque [`IdleHandle`] whose `Drop` tears the backend thread down. The daemon
//! loop only ever sees [`IdleEvent`]s on the channel.

pub mod mutter;

use std::error::Error;
use std::time::Duration;

use calloop::channel::Sender;

/// The default idle threshold (`T1`): how long the seat must be idle before the
/// saver is shown. Five minutes matches the activation design default.
pub const DEFAULT_T1: Duration = Duration::from_secs(5 * 60);

/// An event delivered from an [`IdleSource`] to the daemon loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// The seat has been idle for at least `T1`.
    Idle,
}

/// An opaque handle that keeps an idle backend alive. Dropping it shuts the
/// backend down (e.g. joins/terminates its thread).
pub trait IdleHandle: Send {}

/// A source of "idle reached `T1`" events for the daemon.
///
/// Implementors detect idleness however they like (D-Bus, a Wayland idle
/// protocol, polling, …) and forward [`IdleEvent::Idle`] through the channel
/// `Sender` they are given in [`start`]. The daemon loop holds this trait as a
/// `Box<dyn IdleSource>`, so new backends require no loop changes.
///
/// [`start`]: IdleSource::start
pub trait IdleSource {
    /// Begin watching for idle, forwarding [`IdleEvent`]s to `sender`.
    ///
    /// Returns a handle that keeps the backend alive; dropping it stops the
    /// watch. An error here (e.g. the backend's transport is unreachable —
    /// a non-GNOME session for the Mutter backend) must be returned so the
    /// daemon can fail with a clear diagnostic and a non-zero exit instead of
    /// hanging silently.
    fn start(&self, sender: Sender<IdleEvent>) -> Result<Box<dyn IdleHandle>, Box<dyn Error>>;

    /// Re-arm the watch after the saver was dismissed by input, so the next
    /// idle period produces another [`IdleEvent::Idle`].
    ///
    /// Some backends re-fire automatically and only need this as a no-op; others
    /// must re-add a watch. The contract is: after `rearm`, a subsequent idle
    /// period reliably yields another event. See each backend for specifics.
    fn rearm(&self) -> Result<(), Box<dyn Error>>;
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use calloop::channel::channel;

    use super::*;

    /// A backend with no transport, used to prove the daemon loop depends only
    /// on the `IdleSource` trait. It records how often `rearm` was called and
    /// can push an idle event on demand through the channel it was started with.
    struct FakeIdleSource {
        rearm_calls: Arc<AtomicU32>,
    }

    struct FakeHandle;
    impl IdleHandle for FakeHandle {}

    impl IdleSource for FakeIdleSource {
        fn start(
            &self,
            sender: Sender<IdleEvent>,
        ) -> Result<Box<dyn IdleHandle>, Box<dyn Error>> {
            // Emit one idle event immediately so a consumer can observe it.
            sender.send(IdleEvent::Idle)?;
            Ok(Box::new(FakeHandle))
        }

        fn rearm(&self) -> Result<(), Box<dyn Error>> {
            self.rearm_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn idle_source_is_consumable_through_the_trait_object() {
        // The daemon loop holds an `IdleSource` as a trait object, never the
        // concrete backend. Exercise that shape here.
        let rearm_calls = Arc::new(AtomicU32::new(0));
        let source: Box<dyn IdleSource> = Box::new(FakeIdleSource {
            rearm_calls: Arc::clone(&rearm_calls),
        });

        let (tx, rx) = channel::<IdleEvent>();
        let _handle = source.start(tx).expect("fake start never fails");

        // The event the backend pushed is observable as a plain `IdleEvent`,
        // with no knowledge of the backend's internals.
        match rx.try_recv() {
            Ok(IdleEvent::Idle) => {}
            other => panic!("expected an idle event, got {other:?}"),
        }

        source.rearm().expect("fake rearm never fails");
        assert_eq!(rearm_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_t1_is_five_minutes() {
        assert_eq!(DEFAULT_T1, Duration::from_secs(300));
    }
}
