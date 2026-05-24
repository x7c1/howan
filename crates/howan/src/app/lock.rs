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
//! ## Test seam
//!
//! The work the caller performs on Phase 2 is "lock the session, *then*
//! dismiss the saver" — and the contract is that **dismiss runs even when the
//! lock fails** so the user is never left staring at a screen they cannot get
//! out of. To exercise that "log + proceed" branch without hitting D-Bus in
//! CI, `HowanApp` holds a `Box<dyn SessionLocker>`. The production
//! implementation is `LogindLocker`; tests inject a stub that returns
//! `Err(...)` and assert the saver is still dismissed.

use std::error::Error;

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

/// Blocking proxy for `org.freedesktop.login1.Session`. We only need `Lock`;
/// the path is supplied at construction time (it varies per session, e.g.
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
}

/// Production [`SessionLocker`] that talks to `systemd-logind` over the
/// system bus.
///
/// The connection and the session path are resolved at construction time so
/// the cost of building the proxies is paid once, not on every lock attempt.
/// A construction failure (e.g. logind unreachable on a non-systemd session)
/// returns an error; the caller falls back to a no-op locker so the daemon
/// still runs — the loss is only that Phase 2 input behaves like Phase 1.
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

/// A locker that always succeeds without doing anything. Used as a fallback
/// when the production locker cannot be constructed (e.g. logind is absent),
/// so the daemon still runs — Phase 2 then behaves like Phase 1, which is a
/// strictly safer degradation than refusing to start.
pub(crate) struct NoopLocker;

impl SessionLocker for NoopLocker {
    fn lock(&self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::error::Error;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use super::SessionLocker;

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
}
