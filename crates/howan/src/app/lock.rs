//! `systemd-logind` session lock handoff for the Phase 2 of the saver's
//! lifecycle.
//!
//! When the saver has been up past `T_grace`, the first input should hand off
//! to the compositor's own lock screen instead of dismissing the saver
//! outright. The D-Bus equivalent of `loginctl lock-session` is
//! `org.freedesktop.login1.Session.Lock` on the current session — that is what
//! this module calls. We rely on `zbus` (already pulled in for the Mutter
//! IdleMonitor backend) so no extra dependency is needed.
//!
//! The current session's object path is obtained at construction time from
//! `org.freedesktop.login1.Manager.GetSession("auto")`, which yields the
//! caller's current session without depending on the `XDG_SESSION_ID`
//! environment variable being correctly populated.
//!
//! ## Waiting for the lock screen to actually mount
//!
//! `Session.Lock` returns to howan as soon as logind has forwarded the request,
//! which on GNOME is hundreds of ms to several seconds *before* the
//! compositor mounts its `ext-session-lock-v1` lock surface and starts
//! rendering. If howan dismisses its saver immediately after the method call
//! returns, the user sees a noticeable black gap between the saver
//! disappearing and the lock screen appearing — howan's composited saver
//! shows the panel chrome, so the contrast is jarring and reads as broken.
//!
//! The canonical "lock screen is now up" signal is the
//! `org.freedesktop.login1.Session.LockedHint` property flipping to `true`:
//! GNOME Shell sets it via `SetLockedHint(true)` once it has mounted the
//! lock surface. The [`LockSurveillance`] trait lets the Phase 2 input
//! handler block on that transition (with a fallback timeout) before
//! dropping the saver, so the lock surface — which sits on the topmost
//! `ext-session-lock-v1` layer — covers the saver before the saver itself
//! goes away. The user experiences a clean transition with no visible black
//! gap.
//!
//! ## Test seam
//!
//! The work the caller performs on Phase 2 is "lock the session, wait for
//! the lock screen to mount, *then* dismiss the saver" — and the contract
//! is that **dismiss runs even when the lock fails or the hint never
//! flips** so the user is never left staring at a screen they cannot get
//! out of. To exercise those branches without hitting D-Bus in CI,
//! `HowanApp` holds a `Box<dyn SessionLocker>` and a
//! `Box<dyn LockSurveillance>` separately. The production implementation
//! `LogindLocker` provides both halves; tests inject stubs that return the
//! various [`LockedHintWait`] outcomes (and a [`SessionLocker`] error) and
//! assert the saver is still dismissed.

use std::error::Error;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

/// A handle that knows how to ask the compositor to lock the current session.
///
/// No `Send` bound is required: the locker is owned by `HowanApp`, which is
/// itself constructed on the main thread and never moved across threads —
/// calloop's idle/timer callbacks receive `&mut HowanApp` by reference rather
/// than capturing it, so the locker only ever runs on the thread that built
/// it.
pub(crate) trait SessionLocker {
    /// Invoke whatever lock mechanism the implementor wraps. Returns `Err`
    /// when the lock fails to even fire; the caller treats that as a soft
    /// failure (log + proceed to dismiss).
    fn lock(&self) -> Result<(), Box<dyn Error>>;
}

/// The outcome of waiting for the compositor to confirm that its lock screen
/// has mounted. See [`LockSurveillance::wait_for_locked_hint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockedHintWait {
    /// `LockedHint` transitioned to `true` (or was already `true`) within the
    /// supplied timeout. `elapsed` is measured from the moment
    /// `wait_for_locked_hint` was entered, not from the preceding `Lock`
    /// call, but in practice the two are within a microsecond of each other.
    Observed { elapsed: Duration },
    /// The timeout elapsed before `LockedHint` was observed as `true`. The
    /// caller proceeds to dismiss the saver anyway so the user is never left
    /// staring at a saver they cannot get out of, but the journal records a
    /// WARN so the regression is surfaceable.
    TimedOut,
}

/// Observe `org.freedesktop.login1.Session.LockedHint` flipping to `true`,
/// with a fallback timeout.
///
/// Split out from [`SessionLocker`] because the test seam for "lock issued,
/// hint observed" is distinct from "lock issued at all": each unit test
/// injects a stub of the surveillance half independently of the locker half,
/// so the success / timeout / lock-failure paths can each be exercised
/// directly without driving real D-Bus. The same `LogindLocker` value
/// implements both traits in production — see [`LogindLocker`].
pub(crate) trait LockSurveillance {
    /// Block (up to `timeout`) until `LockedHint == true` is observed for
    /// the current session. The method is allowed to return immediately when
    /// the property is already `true`, which is the racy-but-benign case
    /// where GNOME Shell flipped the hint between `Lock()` returning and the
    /// caller subscribing to the property-changed stream.
    fn wait_for_locked_hint(&self, timeout: Duration) -> LockedHintWait;
}

/// Blocking proxy for `org.freedesktop.login1.Manager`. Only `GetSession` is
/// declared because that is all we need — the rest of the interface is not
/// relevant to howan.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LogindManager {
    /// Look up a session by id. The special id `"auto"` returns the caller's
    /// current session, which is exactly what we want for "lock the user's
    /// own session".
    fn get_session(&self, session_id: &str) -> zbus::Result<OwnedObjectPath>;
}

/// Blocking proxy for `org.freedesktop.login1.Session`. We need `Lock` plus
/// the `LockedHint` property (read access and a property-changed signal
/// derived from `org.freedesktop.DBus.Properties.PropertiesChanged`). The
/// path is supplied at construction time (it varies per session, e.g.
/// `/org/freedesktop/login1/session/_3`).
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1"
)]
trait LogindSession {
    /// Ask logind to lock this session. logind forwards the request to the
    /// session's compositor / lock agent; on GNOME that triggers the GNOME
    /// lock screen.
    fn lock(&self) -> zbus::Result<()>;

    /// Whether the session has been hinted as locked. GNOME Shell flips this
    /// to `true` via `SetLockedHint(true)` as soon as it has mounted its
    /// `ext-session-lock-v1` lock surface, so the property transition is the
    /// canonical "lock screen is now up" signal. `#[zbus::proxy]` exposes a
    /// blocking getter (`locked_hint()`) and a property-changed iterator
    /// (`receive_locked_hint_changed()`) automatically.
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}

/// Production [`SessionLocker`] *and* [`LockSurveillance`] that talks to
/// `systemd-logind` over the system bus.
///
/// The connection and the session path are resolved at construction time so
/// the cost of building the proxies is paid once, not on every lock attempt.
/// A construction failure (e.g. logind unreachable on a non-systemd session)
/// returns an error; the caller falls back to a no-op locker so the daemon
/// still runs — the loss is only that Phase 2 input behaves like Phase 1.
///
/// `Clone` is derived so `HowanApp` can hold the two trait halves as
/// independent `Box<dyn ...>` values without juggling an `Arc`. The
/// underlying `LogindSessionProxyBlocking` is itself `Clone` (it wraps a
/// shared `zbus::Connection`), so cloning is a handle copy rather than a
/// fresh bus connect.
#[derive(Clone)]
pub(crate) struct LogindLocker {
    session: LogindSessionProxyBlocking<'static>,
}

impl LogindLocker {
    /// Connect to the system bus, resolve the current session's object path
    /// through `Manager.GetSession("auto")`, and build a session-bound proxy
    /// ready to call `Lock`.
    pub(crate) fn new() -> Result<Self, Box<dyn Error>> {
        let connection = Connection::system()
            .map_err(|err| format!("failed to connect to the D-Bus system bus: {err}"))?;
        let manager = LogindManagerProxyBlocking::new(&connection)
            .map_err(|err| format!("failed to build org.freedesktop.login1.Manager proxy: {err}"))?;
        let session_path = manager.get_session("auto").map_err(|err| {
            format!(
                "failed to resolve current session via \
                 org.freedesktop.login1.Manager.GetSession(\"auto\"): {err}"
            )
        })?;
        let session = LogindSessionProxyBlocking::builder(&connection)
            .path(session_path)
            .map_err(|err| format!("invalid session path: {err}"))?
            .build()
            .map_err(|err| format!("failed to build org.freedesktop.login1.Session proxy: {err}"))?;
        Ok(Self { session })
    }
}

impl SessionLocker for LogindLocker {
    fn lock(&self) -> Result<(), Box<dyn Error>> {
        self.session
            .lock()
            .map_err(|err| format!("org.freedesktop.login1.Session.Lock failed: {err}").into())
    }
}

impl LockSurveillance for LogindLocker {
    /// Watch `LockedHint` through the existing session proxy. The zbus
    /// blocking property iterator does not expose a timeout-aware `next`, so
    /// the wait is implemented as:
    ///
    /// 1. Probe the cached / current value — if already `true` (the lock
    ///    surface was mounted between `Lock()` returning and us getting here),
    ///    return [`LockedHintWait::Observed`] with elapsed = 0 without
    ///    creating any thread.
    /// 2. Otherwise subscribe to `receive_locked_hint_changed()` and spawn a
    ///    short-lived helper thread that pumps the iterator into a
    ///    `std::sync::mpsc` channel. The main thread does `recv_timeout` so
    ///    it never blocks the Wayland event loop for longer than `timeout`.
    /// 3. On timeout the helper thread is **orphaned**: there is no
    ///    cooperative cancel for a thread blocked on `Iterator::next()`
    ///    against an open D-Bus connection. The helper exits cleanly the
    ///    next time the property changes (or when the daemon process
    ///    exits). Phase 2 fires at most once per saver cycle and a timeout
    ///    is the genuinely-broken case, so the leak is bounded by the
    ///    daemon's lifetime; tracked here rather than at the call site so
    ///    the explanation stays with the code that decides it.
    fn wait_for_locked_hint(&self, timeout: Duration) -> LockedHintWait {
        let started_at = Instant::now();
        // Fast path: the property may already be `true` if Shell flipped
        // the hint before we got a chance to subscribe. `locked_hint()`
        // reads from the auto-managed property cache.
        if let Ok(true) = self.session.locked_hint() {
            return LockedHintWait::Observed {
                elapsed: started_at.elapsed(),
            };
        }

        // Clone the `'static` blocking proxy into the helper thread so the
        // `PropertyIterator` it builds is owned (no borrow back to `&self`);
        // the clone shares the underlying zbus Connection, so this is a
        // cheap handle-clone, not a fresh bus connect.
        let session = self.session.clone();
        let (tx, rx) = mpsc::channel::<bool>();

        // The helper thread owns the iterator; dropping the receiver does
        // not unblock the iterator's `next`, so this thread is genuinely
        // orphan-on-timeout. See the doc comment above for why that is
        // acceptable here.
        thread::Builder::new()
            .name("howan-locked-hint".into())
            .spawn(move || {
                let mut changes = session.receive_locked_hint_changed();
                for changed in changes.by_ref() {
                    let Ok(value) = changed.get() else {
                        continue;
                    };
                    // Forward every transition we observe; the receiver
                    // filters for `true`. `send` only errors when the
                    // receiver has been dropped (timeout already fired),
                    // in which case there is nothing more to do.
                    if tx.send(value).is_err() {
                        return;
                    }
                    if value {
                        return;
                    }
                }
            })
            .ok();

        loop {
            let remaining = timeout.checked_sub(started_at.elapsed());
            let Some(remaining) = remaining else {
                return LockedHintWait::TimedOut;
            };
            match rx.recv_timeout(remaining) {
                Ok(true) => {
                    return LockedHintWait::Observed {
                        elapsed: started_at.elapsed(),
                    }
                }
                // A spurious `false` is possible if a stale change was queued
                // before we subscribed; keep waiting for `true`.
                Ok(false) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => return LockedHintWait::TimedOut,
                // The helper thread exited without sending `true`. Treat as
                // timeout — it means the stream ended (bus closed) without
                // observing the transition.
                Err(mpsc::RecvTimeoutError::Disconnected) => return LockedHintWait::TimedOut,
            }
        }
    }
}

/// A locker that always succeeds without doing anything. Used as a fallback
/// when the production locker cannot be constructed (e.g. logind is absent),
/// so the daemon still runs — Phase 2 then behaves like Phase 1, which is a
/// strictly safer degradation than refusing to start.
#[derive(Clone, Copy)]
pub(crate) struct NoopLocker;

impl SessionLocker for NoopLocker {
    fn lock(&self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

impl LockSurveillance for NoopLocker {
    /// Without a real lock call there is no lock screen to wait for, so
    /// the "wait" is a structural no-op that returns `Observed { elapsed:
    /// 0 }` immediately — Phase 2 then dismisses on the next line with no
    /// extra latency and no spurious WARN about a missing `LockedHint`.
    /// The startup-time fallback log
    /// (`systemd-logind session lock unavailable …`) already records that
    /// the daemon is running in this degraded mode; emitting a per-input
    /// warning on top would be noisy without adding diagnostic value.
    fn wait_for_locked_hint(&self, _timeout: Duration) -> LockedHintWait {
        LockedHintWait::Observed {
            elapsed: Duration::ZERO,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::error::Error;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{LockSurveillance, LockedHintWait, SessionLocker};

    /// A locker whose `lock()` always returns an error. Used by the
    /// "lock-failure → dismiss still runs" unit test in `app.rs`.
    pub(crate) struct FailingLocker {
        pub(crate) calls: Arc<AtomicU32>,
    }

    impl FailingLocker {
        pub(crate) fn new() -> Self {
            Self {
                calls: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl SessionLocker for FailingLocker {
        fn lock(&self) -> Result<(), Box<dyn Error>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err("synthetic lock failure".into())
        }
    }

    /// A [`LockSurveillance`] stub that always reports `Observed` after a
    /// canned elapsed duration. Used by the "lock + wait succeeds" path.
    pub(crate) struct ObservedSurveillance {
        pub(crate) elapsed: Duration,
        pub(crate) calls: Arc<AtomicU32>,
    }

    impl ObservedSurveillance {
        pub(crate) fn new(elapsed: Duration) -> Self {
            Self {
                elapsed,
                calls: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl LockSurveillance for ObservedSurveillance {
        fn wait_for_locked_hint(&self, _timeout: Duration) -> LockedHintWait {
            self.calls.fetch_add(1, Ordering::SeqCst);
            LockedHintWait::Observed {
                elapsed: self.elapsed,
            }
        }
    }

    /// A [`LockSurveillance`] stub that always reports `TimedOut`. Used by
    /// the "hint never observed → warn + dismiss anyway" path.
    pub(crate) struct TimedOutSurveillance {
        pub(crate) calls: Arc<AtomicU32>,
    }

    impl TimedOutSurveillance {
        pub(crate) fn new() -> Self {
            Self {
                calls: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    impl LockSurveillance for TimedOutSurveillance {
        fn wait_for_locked_hint(&self, _timeout: Duration) -> LockedHintWait {
            self.calls.fetch_add(1, Ordering::SeqCst);
            LockedHintWait::TimedOut
        }
    }

    /// A [`LockSurveillance`] stub that panics if invoked. Used by the
    /// lock-failure test to assert that `wait_for_locked_hint` is **not**
    /// reached when `SessionLocker::lock` errors.
    pub(crate) struct UnreachableSurveillance;

    impl LockSurveillance for UnreachableSurveillance {
        fn wait_for_locked_hint(&self, _timeout: Duration) -> LockedHintWait {
            panic!("wait_for_locked_hint must not be called when Lock failed");
        }
    }
}
