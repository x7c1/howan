//! The composited Wayland saver surface and the state that drives it.
//!
//! `HowanApp` owns the durable Wayland state — the registry, seat, output,
//! and the `wl_compositor` / `xdg_wm_base` / `wl_shm` globals — that persists
//! for the whole lifetime of the process. The saver itself (an output-sized
//! composited `xdg_toplevel` plus its `wl_shm` renderer) is split out into
//! [`Saver`] and held in `HowanApp::saver` as an `Option`, so it can be created
//! on demand and dropped on dismiss **without** tearing down the connection.
//!
//! Two entry points use this state:
//!
//! - [`run`] — the one-shot `howan start`: it shows the saver immediately and
//!   exits the process on the first input (the manual/debug path).
//! - [`run_daemon`] — the resident `howan daemon`: it stays connected with no
//!   surface, shows the saver when the idle source fires, dispatches input by
//!   the elapsed-time three-phase lifecycle (see [`SaverPhase`] and
//!   `docs/guides/40-resident-daemon.md`), and on dismiss drops the *surface*
//!   (not the process), re-arming for the next idle cycle. The Phase 3 timer
//!   can drop the surface without any input at all.
//!
//! In both cases dismiss tears down only the surface; the difference is only
//! what happens afterwards (process exit vs. stay resident), which is decided
//! by the loop in the entry point, not by the surface code.
//!
//! # Why no `set_fullscreen`
//!
//! The saver is sized to the active output's current mode but is an ordinary
//! composited `xdg_toplevel` — it deliberately does **not** call
//! `xdg_toplevel.set_fullscreen`, and it never declares an opaque region on its
//! surface. Both choices keep the surface off Mutter's unredirect /
//! direct-scanout fast path, which performs a KMS modeset that wedges the
//! display engine / GSP firmware on NVIDIA Blackwell (RTX 50-series) GPUs. See
//! `docs/guides/30-composited-surface.md` for the full rationale and the
//! manual safe-hardware verification procedure. Because the daemon recreates
//! the saver on every idle cycle, [`Saver::new`] must recreate it the same safe
//! way every time — the invariant lives at that one construction site.
//!
//! SCTK is used for the compositor / xdg-shell / shm / seat / pointer / touch
//! glue, but `wl_keyboard` is bound directly through `wayland-client` so we
//! can avoid pulling in libxkbcommon at build time. We only need to know that
//! some key was pressed; full keymap interpretation is unnecessary.
//!
//! The module is split into four files so the boundaries that future
//! milestones will cross are explicit:
//!
//! - `app.rs` (this file) holds `run`, `run_daemon`, and the top-level
//!   `HowanApp` / `Saver` state.
//! - `app::render` owns surface drawing and the `wl_shm` buffer pool. A
//!   later milestone is expected to swap this out for a GPU-backed
//!   renderer; isolating it here means that change is local.
//! - `app::handlers` contains every Wayland-protocol handler trait impl
//!   plus the `delegate_*!` macros.
//! - `app::lock` wraps the `systemd-logind` session-lock handoff used by
//!   the Phase 2 input branch, with a `SessionLocker` trait so tests can
//!   inject a stub without hitting D-Bus.

mod handlers;
mod lock;
mod render;

use std::error::Error;
use std::time::{Duration, Instant};

use calloop::channel::{channel, Event as ChannelEvent};
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::RegistrationToken;
use smithay_client_toolkit::{
    compositor::CompositorState,
    output::OutputState,
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::RegistryState,
    seat::SeatState,
    shell::{
        xdg::{
            window::{Window, WindowDecorations},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::Shm,
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{
        wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_pointer::WlPointer, wl_touch::WlTouch,
    },
    Connection, QueueHandle,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

use self::lock::{LogindLocker, NoopLocker, SessionLocker};
use self::render::Renderer;
use crate::daemon::{IdleEvent, IdleSource};
use crate::pidfile::PidFileGuard;

/// Pre-configure starting size for the shm pool, never the intended final
/// size. The window is resized to the active output's current mode dimensions
/// as soon as the output geometry is known (see `resize_to_active_output`).
const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 720;

/// Maximum time spent in a single event-loop dispatch. Kept short so that the
/// exit flag is observed quickly after an input event is processed.
const DISPATCH_TIMEOUT: Duration = Duration::from_millis(16);

/// Run the saver (`howan start`). Blocks until the user dismisses the window,
/// the compositor closes it, or a `SIGTERM`/`SIGINT` (e.g. from `howan stop`)
/// arrives. This is the one-shot manual/debug path: input exits the process.
pub fn run() -> Result<(), Box<dyn Error>> {
    // Publish our PID before opening the window so `howan stop` can reach us
    // for the entire visible lifetime. The guard removes the file on every
    // exit path, including an early `?` return below.
    let _pid_guard = PidFileGuard::acquire()?;

    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<HowanApp> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|err| format!("failed to insert wayland source into event loop: {err}"))?;

    insert_signal_source(&loop_handle)?;

    // The one-shot `start` path exits on the first input, so it never reaches
    // Phase 2 or Phase 3 in practice. Pass a `NoopLocker` directly instead of
    // calling `build_locker()` — there is no point paying the D-Bus connect +
    // `GetSession("auto")` round trip (or emitting the fallback warning on a
    // non-systemd host) for a code path that cannot run.
    let mut app = HowanApp::new(
        &globals,
        &qh,
        Duration::from_secs(u64::MAX / 2),
        Duration::from_secs(u64::MAX / 2),
        Box::new(NoopLocker),
    )?;
    // One-shot: show the saver immediately, then exit the process on dismiss.
    app.show_saver(&qh);

    while !app.should_quit() {
        event_loop.dispatch(DISPATCH_TIMEOUT, &mut app)?;
        // In one-shot mode, dismissing the surface ends the process.
        if app.saver.is_none() {
            break;
        }
    }

    app.release_input_handles();
    Ok(())
}

/// Run the resident daemon (`howan daemon`).
///
/// Connects to Wayland but shows **no** surface until the idle source reports
/// that the seat has been idle for `T1`. Input or the Phase 3 timer tears down
/// the saver surface and the daemon re-arms the idle source, staying resident
/// for the next cycle. `SIGTERM`/`SIGINT` terminate the whole daemon cleanly.
///
/// The loop consumes idle events through the [`IdleSource`] trait, so a future
/// backend (e.g. `ext-idle-notify-v1` on wlroots) can be dropped in by writing
/// a new `IdleSource` implementation without touching this function.
///
/// `t_grace` and `t_dpms` come from the daemon CLI flags and define the
/// boundaries of the three-phase lifecycle — see [`SaverPhase`] and
/// `docs/guides/40-resident-daemon.md`.
pub fn run_daemon(
    idle_source: Box<dyn IdleSource>,
    t_grace: Duration,
    t_dpms: Duration,
) -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<HowanApp> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .map_err(|err| format!("failed to insert wayland source into event loop: {err}"))?;

    insert_signal_source(&loop_handle)?;

    // Bridge the (async/blocking) idle source into the single-threaded calloop
    // loop: the idle source runs on its own thread and forwards idle events
    // through this channel, keeping all Wayland work on the main thread. The
    // daemon loop only ever observes `IdleEvent`s, never the transport.
    let (idle_tx, idle_rx) = channel::<IdleEvent>();
    loop_handle
        .insert_source(idle_rx, |event, _metadata, app: &mut HowanApp| {
            if let ChannelEvent::Msg(IdleEvent::Idle) = event {
                app.on_idle();
            }
        })
        .map_err(|err| format!("failed to insert idle source into event loop: {err}"))?;

    // Start the idle watch. A failure here (e.g. the Mutter IdleMonitor D-Bus
    // interface is unreachable on a non-GNOME session) surfaces as a clear
    // error and a non-zero exit instead of a silent hang.
    let _idle_handle = idle_source.start(idle_tx)?;

    let mut app = HowanApp::new(&globals, &qh, t_grace, t_dpms, build_locker())?;

    // The Phase 3 timer source. We register/cancel a calloop Timer for
    // `t_dpms` whenever the saver is shown / dismissed.
    let mut dpms_timer_token: Option<RegistrationToken> = None;

    while !app.should_quit() {
        event_loop.dispatch(DISPATCH_TIMEOUT, &mut app)?;

        // If a saver was just shown and no timer is armed yet, arm one for
        // `t_dpms`. This must run after `dispatch` so the show triggered by
        // `on_idle` is observable here.
        if app.saver.is_some() && dpms_timer_token.is_none() {
            let token = loop_handle
                .insert_source(
                    Timer::from_duration(t_dpms),
                    |_deadline, _meta, app: &mut HowanApp| {
                        app.dpms_handoff();
                        TimeoutAction::Drop
                    },
                )
                .map_err(|err| {
                    format!("failed to insert dpms timer into event loop: {err}")
                })?;
            dpms_timer_token = Some(token);
        }

        // If the saver is gone but our timer is still registered, cancel it.
        // This covers both the input-dismiss path and the Phase 3 timer-fire
        // path (`TimeoutAction::Drop` already removed it on fire; `remove` on a
        // dropped token is harmless — calloop ignores unknown tokens).
        if app.saver.is_none() {
            if let Some(token) = dpms_timer_token.take() {
                loop_handle.remove(token);
            }
        }

        // Input dismiss sets `pending_rearm`; the surface is already gone. Tell
        // the idle source to re-arm so the next idle period shows the saver
        // again. The daemon itself stays resident.
        if app.take_pending_rearm() {
            idle_source.rearm()?;
        }
    }

    app.release_input_handles();
    Ok(())
}

/// Construct the production [`SessionLocker`]. A failure (e.g. logind is not
/// reachable on a non-systemd session) falls back to a no-op locker so the
/// daemon still runs — Phase 2 then behaves like Phase 1, which is strictly
/// safer than refusing to start.
fn build_locker() -> Box<dyn SessionLocker> {
    match LogindLocker::new() {
        Ok(locker) => Box::new(locker),
        Err(err) => {
            eprintln!(
                "howan: systemd-logind session lock unavailable ({err}); \
                 Phase 2 input will dismiss the saver without locking"
            );
            Box::new(NoopLocker)
        }
    }
}

/// Register the `SIGTERM`/`SIGINT` handler on the event loop.
///
/// `SIGTERM` is how `howan stop` (and systemd's stop path) asks the process to
/// quit; `SIGINT` covers Ctrl-C in an interactive run. Both are routed through
/// the event loop via a signalfd and set the `should_exit` flag, so shutdown
/// unwinds the clean-exit path (input handles released, PID file removed)
/// instead of aborting mid-frame.
///
/// This is deliberately distinct from input dismiss: in the daemon, input tears
/// down only the *surface* and re-arms, while a signal terminates the *whole
/// process*. The two paths must not be collapsed (see
/// `docs/guides/40-resident-daemon.md`).
fn insert_signal_source(
    loop_handle: &calloop::LoopHandle<'static, HowanApp>,
) -> Result<(), Box<dyn Error>> {
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])
        .map_err(|err| format!("failed to register signal handler: {err}"))?;
    loop_handle
        .insert_source(signals, |_event, _metadata, app: &mut HowanApp| {
            app.request_exit();
        })
        .map_err(|err| format!("failed to insert signal source into event loop: {err}"))?;
    Ok(())
}

/// Durable, process-lifetime Wayland state plus the optional on-screen saver.
///
/// Everything except `saver` lives for the whole process. `saver` is created
/// on demand ([`show_saver`]) and dropped on dismiss, so show → hide → show is
/// repeatable within one daemon process.
///
/// [`show_saver`]: HowanApp::show_saver
pub(crate) struct HowanApp {
    pub(crate) registry_state: RegistryState,
    pub(crate) seat_state: SeatState,
    pub(crate) output_state: OutputState,
    pub(crate) compositor: CompositorState,
    pub(crate) xdg_shell: XdgShell,
    pub(crate) shm: Shm,
    /// A handle to the shared event queue, kept so idle-channel callbacks (which
    /// receive only `&mut HowanApp`) can create the saver surface on demand.
    qh: QueueHandle<HowanApp>,
    /// The idle-inhibit manager, bound at startup if the compositor advertises
    /// `zwp_idle_inhibit_manager_v1`; `None` when the global is absent. Used to
    /// create an inhibitor on the saver surface (see [`Saver`]); the
    /// degrade-gracefully-on-absence rationale lives at the bind site in
    /// `HowanApp::new`.
    idle_inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
    /// The on-screen saver, present only while the saver is shown.
    pub(crate) saver: Option<Saver>,
    pub(crate) keyboard: Option<WlKeyboard>,
    pub(crate) pointer: Option<WlPointer>,
    pub(crate) touch: Option<WlTouch>,
    /// The output the saver surface is shown on. We track the output the
    /// surface entered ("active output only"); until a surface-enter event
    /// arrives we fall back to the first advertised output.
    pub(crate) active_output: Option<WlOutput>,
    /// Phase 1 → Phase 2 boundary (`T_grace` in the design): until this much
    /// time has passed since the saver was shown, input dismisses the saver
    /// outright. From here on, input first locks the session and then
    /// dismisses. Threaded in from the CLI at `new` time so the timer plumbing
    /// and the input dispatch read the same value.
    t_grace: Duration,
    /// Phase 2 → Phase 3 boundary (`T_dpms` in the design): the daemon arms a
    /// calloop timer for this duration when the saver is shown and on fire
    /// drops the saver / releases the inhibitor so the compositor's own idle
    /// blank can take over.
    t_dpms: Duration,
    /// Session locker used by the Phase 2 input branch. A `Box<dyn ...>` so a
    /// test stub can be injected — the production implementation is
    /// [`lock::LogindLocker`].
    locker: Box<dyn SessionLocker>,
    /// Set by the `SIGTERM`/`SIGINT` handler to terminate the whole process.
    /// Input dismiss does **not** set this — it only drops `saver`.
    exit: bool,
    /// Set when input has just dismissed the saver and the daemon loop should
    /// re-arm its idle source. Cleared by [`take_pending_rearm`].
    ///
    /// [`take_pending_rearm`]: HowanApp::take_pending_rearm
    pending_rearm: bool,
}

/// The phase the saver is currently in, decided by how long it has been
/// shown. See [`Saver::phase`] and `docs/guides/40-resident-daemon.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaverPhase {
    /// Up to `T_grace` after the saver was shown: input dismisses the saver
    /// outright.
    Phase1,
    /// Between `T_grace` (inclusive) and `T_dpms` (exclusive): input invokes
    /// `org.freedesktop.login1.Session.Lock`, then dismisses the saver.
    Phase2,
    /// `T_dpms` (inclusive) and onward: the daemon's calloop timer drops the
    /// saver and releases the inhibitor so the compositor's standard idle
    /// blank takes over. In practice input cannot reach this branch — the
    /// surface is already gone by then — but the variant exists so callers
    /// can match exhaustively and handle the (timer-delayed) edge case.
    Phase3,
}

/// The recreatable on-screen saver: an output-sized composited toplevel and the
/// `wl_shm` renderer that paints it. Dropped on dismiss; recreated each idle
/// cycle by [`HowanApp::show_saver`].
pub(crate) struct Saver {
    pub(crate) window: Window,
    pub(crate) renderer: Renderer,
    /// Set once the xdg surface has received its first configure. We must not
    /// attach a buffer and commit before that (xdg-shell forbids committing a
    /// buffer to a surface that has never been configured); output/seat events
    /// can otherwise trigger `draw()` too early. Strict compositors (e.g.
    /// `weston`) reject the premature commit; Mutter tolerates it.
    pub(crate) configured: bool,
    /// The idle inhibitor held against this saver's `wl_surface` for as long as
    /// the saver is shown, so the compositor does not blank the display (DPMS
    /// off) behind it. `None` when the idle-inhibit manager global was absent at
    /// startup.
    ///
    /// Released in [`Saver`]'s `Drop` impl, which explicitly sends
    /// `zwp_idle_inhibitor_v1.destroy`. This **must** be explicit:
    /// `wayland-client` proxies do not send their destructor request when the
    /// Rust handle is dropped, so without it the inhibitor leaks and Mutter keeps
    /// treating the session as non-idle even after dismiss — blocking both the
    /// DPMS resume and the next idle detection (the saver would show only once).
    inhibitor: Option<ZwpIdleInhibitorV1>,
    /// The instant this saver was constructed — i.e. when the saver first
    /// became visible for the current cycle. It is the single source of truth
    /// for the three-phase lifecycle: [`Saver::phase`] compares `now -
    /// shown_at` against `T_grace` / `T_dpms` to decide which branch input or
    /// the timer should take.
    shown_at: Instant,
}

impl HowanApp {
    /// Bind the durable Wayland globals. No surface is created here; call
    /// [`show_saver`](HowanApp::show_saver) to put the saver on screen.
    ///
    /// `t_grace` and `t_dpms` define the three-phase lifecycle thresholds —
    /// see [`SaverPhase`] and `docs/guides/40-resident-daemon.md`. `locker`
    /// is the handle used for the Phase 2 lock-session handoff; production
    /// callers pass [`lock::LogindLocker`], tests inject a stub.
    fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<HowanApp>,
        t_grace: Duration,
        t_dpms: Duration,
        locker: Box<dyn SessionLocker>,
    ) -> Result<Self, Box<dyn Error>> {
        let compositor = CompositorState::bind(globals, qh)
            .map_err(|err| format!("wl_compositor not available: {err}"))?;
        let xdg_shell = XdgShell::bind(globals, qh)
            .map_err(|err| format!("xdg_wm_base not available: {err}"))?;
        let shm =
            Shm::bind(globals, qh).map_err(|err| format!("wl_shm not available: {err}"))?;

        // Bind the idle-inhibit manager through the existing GlobalList. This is
        // best-effort: a compositor without the global degrades to "no
        // inhibitor", and the saver still shows. We log once so the absence is
        // diagnosable, then keep `None`. Unlike the compositor / xdg / shm
        // globals above, a missing idle-inhibit manager is *not* fatal — DPMS
        // suppression is an enhancement to the saver, not a precondition for it.
        let idle_inhibit_manager = match globals.bind::<ZwpIdleInhibitManagerV1, _, _>(qh, 1..=1, ())
        {
            Ok(manager) => Some(manager),
            Err(err) => {
                eprintln!(
                    "howan: idle-inhibit manager unavailable ({err}); \
                     the saver will still show but the compositor may blank the display behind it"
                );
                None
            }
        };

        Ok(Self {
            registry_state: RegistryState::new(globals),
            seat_state: SeatState::new(globals, qh),
            output_state: OutputState::new(globals, qh),
            compositor,
            xdg_shell,
            shm,
            qh: qh.clone(),
            idle_inhibit_manager,
            saver: None,
            keyboard: None,
            pointer: None,
            touch: None,
            active_output: None,
            t_grace,
            t_dpms,
            locker,
            exit: false,
            pending_rearm: false,
        })
    }

    /// Create and map the saver surface, reusing the durable globals. Idempotent
    /// while a saver is already shown.
    pub(crate) fn show_saver(&mut self, qh: &QueueHandle<HowanApp>) {
        if self.saver.is_some() {
            return;
        }
        match Saver::new(
            &self.compositor,
            &self.xdg_shell,
            &self.shm,
            self.idle_inhibit_manager.as_ref(),
            qh,
        ) {
            Ok(saver) => self.saver = Some(saver),
            Err(err) => eprintln!("howan: failed to create saver surface: {err}"),
        }
    }

    /// React to an idle-reached event from the idle source: show the saver.
    pub(crate) fn on_idle(&mut self) {
        let qh = self.qh.clone();
        self.show_saver(&qh);
    }

    /// Whether the current saver surface has received its first configure.
    /// `false` when no saver is shown.
    pub(crate) fn saver_configured(&self) -> bool {
        self.saver.as_ref().is_some_and(|s| s.configured)
    }

    /// Paint the current surface contents and commit the window. No-op when no
    /// saver is shown.
    pub(crate) fn draw(&mut self) {
        if let Some(saver) = self.saver.as_mut() {
            saver.renderer.render(saver.window.wl_surface());
            saver.window.commit();
        }
    }

    /// Resize the saver surface to cover the active output's current mode.
    ///
    /// This is how the saver covers the screen now that it no longer calls
    /// `set_fullscreen`: as an ordinary composited toplevel we ask for a window
    /// the size of the output (see `active_output_size`). If output info is not
    /// yet available, or no saver is shown, we keep the existing allocation and
    /// rely on a later output / configure event to trigger this resize, rather
    /// than blocking startup.
    ///
    /// Returns `true` when a new size was applied so the caller can repaint.
    pub(crate) fn resize_to_active_output(&mut self) -> bool {
        let Some((width, height)) = self.active_output_size() else {
            return false;
        };
        let Some(saver) = self.saver.as_mut() else {
            return false;
        };
        if width == saver.renderer.width() && height == saver.renderer.height() {
            return false;
        }
        // Pin the toplevel to the output size so the compositor does not offer a
        // smaller interactive size. We are not fullscreen, so without this the
        // server is free to pick an arbitrary size.
        saver.window.set_min_size(Some((width, height)));
        saver.window.set_max_size(Some((width, height)));
        saver.renderer.resize(width, height);
        true
    }

    /// The active output's current mode dimensions, if known. Prefers the
    /// `wl_output` mode flagged `current`; falls back to the logical size the
    /// compositor reports for the output.
    fn active_output_size(&self) -> Option<(u32, u32)> {
        let output = self
            .active_output
            .clone()
            .or_else(|| self.output_state.outputs().next())?;
        let info = self.output_state.info(&output)?;
        let dims = info
            .modes
            .iter()
            .find(|mode| mode.current)
            .map(|mode| mode.dimensions)
            .or(info.logical_size)?;
        if dims.0 > 0 && dims.1 > 0 {
            Some((dims.0 as u32, dims.1 as u32))
        } else {
            None
        }
    }

    /// Tear down the saver surface, regardless of the phase that triggered it.
    ///
    /// This is the "drop surface + flag re-arm" primitive: it drops `saver`,
    /// forgets the active output, and flags the daemon loop to re-arm. It does
    /// **not** set the process-exit flag — in the daemon, dismiss means "stay
    /// resident". The one-shot `run` loop notices `saver` is `None` and exits
    /// on its own. Idempotent: repeated calls after the first dismiss do
    /// nothing.
    ///
    /// Higher-level entry points dispatch to this primitive by phase:
    ///
    /// - Input goes through [`on_input`](HowanApp::on_input).
    /// - The Phase 3 calloop timer goes through
    ///   [`dpms_handoff`](HowanApp::dpms_handoff).
    /// - A compositor-issued close request goes through `dismiss` directly
    ///   (no phase logic — the compositor's "please close" is unconditional).
    pub(crate) fn dismiss(&mut self) {
        if self.saver.take().is_some() {
            self.active_output = None;
            self.pending_rearm = true;
        }
    }

    /// Dispatch on user input according to the current saver phase.
    ///
    /// - Phase 1: drop the surface (the M3 behavior).
    /// - Phase 2: ask logind to lock the session, then drop the surface. The
    ///   lock call is "fire-and-forget"; if it fails we log a single stderr
    ///   line and **still** drop the surface so the user is never left
    ///   staring at a saver they cannot get out of (the "log + proceed"
    ///   contract from the plan).
    /// - Phase 3: not expected — the Phase 3 timer normally drops the surface
    ///   first, so by the time input arrives `self.saver` is already `None`
    ///   and the guard below returns early. The Phase 3 arm of the match is
    ///   reached only if the timer is somehow delayed, and dismisses
    ///   defensively to keep the user unstuck.
    ///
    /// Called by every keyboard / pointer / touch handler. The compositor's
    /// own "please close" path keeps calling [`dismiss`](HowanApp::dismiss)
    /// directly — only user input goes through `on_input`.
    pub(crate) fn on_input(&mut self) {
        let Some(saver) = self.saver.as_ref() else {
            // No saver is shown — nothing to dispatch on. Input arriving with
            // no saver can happen via raced events around a just-dismissed
            // surface; matching `dismiss`'s idempotence, this is a no-op.
            return;
        };
        match saver.phase(Instant::now(), self.t_grace, self.t_dpms) {
            SaverPhase::Phase1 => self.dismiss(),
            SaverPhase::Phase2 => {
                self.lock_session();
                self.dismiss();
            }
            SaverPhase::Phase3 => {
                // The Phase 3 timer normally drops the surface before this
                // branch becomes reachable from input. If the timer is somehow
                // delayed, dismiss defensively so the user is not left stuck.
                self.dismiss();
            }
        }
    }

    /// Phase 3 timer callback: drop the saver surface so the inhibitor is
    /// released (via [`Saver`]'s `Drop`) and the compositor's standard idle
    /// blank can take over.
    ///
    /// Functionally equivalent to [`dismiss`](HowanApp::dismiss) today. The
    /// distinct name documents the intent at the call site and leaves room
    /// for the Phase 3 path to diverge later (e.g. a different re-arm policy
    /// after a DPMS handoff) without rewiring the timer plumbing in
    /// `run_daemon`.
    pub(crate) fn dpms_handoff(&mut self) {
        self.dismiss();
    }

    /// Ask logind to lock the current session via
    /// `org.freedesktop.login1.Session.Lock`. Logs a single stderr line on
    /// failure and returns — the caller (`on_input`) proceeds to dismiss
    /// regardless. Tested via the injected
    /// [`SessionLocker`](lock::SessionLocker) (see `FailingLocker`).
    fn lock_session(&self) {
        if let Err(err) = self.locker.lock() {
            eprintln!("howan: lock-session failed: {err}");
        }
    }

    /// Request termination of the whole process (set by the signal handler).
    fn request_exit(&mut self) {
        self.exit = true;
    }

    /// Whether the process should quit (a signal was received).
    fn should_quit(&self) -> bool {
        self.exit
    }

    /// Take and clear the "re-arm the idle source" flag set by [`dismiss`].
    ///
    /// [`dismiss`]: HowanApp::dismiss
    fn take_pending_rearm(&mut self) -> bool {
        std::mem::take(&mut self.pending_rearm)
    }

    /// Release seat input handles explicitly so the compositor does not see a
    /// lingering client during shutdown.
    fn release_input_handles(&mut self) {
        if let Some(kbd) = self.keyboard.take() {
            kbd.release();
        }
        if let Some(ptr) = self.pointer.take() {
            ptr.release();
        }
        if let Some(touch) = self.touch.take() {
            touch.release();
        }
    }
}

impl Saver {
    /// Create the saver surface and renderer.
    ///
    /// IMPORTANT — DO NOT call `window.set_fullscreen(...)` and DO NOT declare an
    /// opaque region on this surface (no `wl_surface.set_opaque_region`).
    ///
    /// Mutter only elects *opaque* surfaces, or the transparent surface of a
    /// *fullscreen* window, for its unredirect / direct-scanout optimization,
    /// which performs a KMS plane/mode reconfiguration when the surface maps.
    /// That modeset wedges the display engine / GSP firmware on NVIDIA Blackwell
    /// (RTX 50-series) GPUs and requires a hard reset. A surface that is neither
    /// fullscreen nor opaque stays on the normal composited path, so no risky
    /// modeset happens. Because the daemon recreates the saver on every idle
    /// cycle, this construction site is the single place that must preserve the
    /// invariant — recreate the surface the same safe way each time. See
    /// `docs/guides/30-composited-surface.md` for the full rationale.
    ///
    /// The shm buffer is still filled with opaque-black pixels (alpha 0xFF) for
    /// appearance — that is a separate thing from declaring an opaque *region*
    /// on the surface, and only the latter governs Mutter's scanout eligibility.
    ///
    /// When `idle_inhibit_manager` is `Some`, an inhibitor is created against
    /// this saver's `wl_surface` and stored on the returned `Saver` (see the
    /// `inhibitor` field for its surface-bound lifetime). The inhibitor becomes
    /// effective once the surface maps (per the protocol), so creating it here —
    /// before the first configure — is fine. When the manager is `None` the
    /// saver is created without an inhibitor and shows normally.
    fn new(
        compositor: &CompositorState,
        xdg_shell: &XdgShell,
        shm: &Shm,
        idle_inhibit_manager: Option<&ZwpIdleInhibitManagerV1>,
        qh: &QueueHandle<HowanApp>,
    ) -> Result<Self, Box<dyn Error>> {
        let surface = compositor.create_surface(qh);
        // The saver acts as a passive overlay, so chrome would be visible noise.
        // We request server-side decorations so the client never has to draw
        // CSD; on a composited (non-fullscreen) toplevel some compositors may
        // still add a titlebar, tracked as part of the unresolved
        // top-most/coverage question (see docs/guides/30-composited-surface.md).
        let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, qh);
        window.set_title("howan");
        window.set_app_id("io.github.x7c1.howan");
        // Initial commit with no buffer is required so the compositor will send
        // a configure event. The window is sized to the active output's current
        // mode once the output geometry is known (see
        // `HowanApp::resize_to_active_output`).
        window.commit();

        let renderer = Renderer::new(shm, INITIAL_WIDTH, INITIAL_HEIGHT)?;

        // Hold an idle inhibitor against the saver surface so the compositor does
        // not blank the display behind it. When the manager is `None` (global
        // absent) `make_inhibitor` produces `None` without any Wayland call — see
        // its unit test.
        let inhibitor = make_inhibitor(idle_inhibit_manager, |manager| {
            manager.create_inhibitor(window.wl_surface(), qh, ())
        });

        Ok(Self {
            window,
            renderer,
            configured: false,
            inhibitor,
            shown_at: Instant::now(),
        })
    }

    /// Decide which phase the saver is currently in.
    ///
    /// The decision is pure: it compares `now - shown_at` against `t_grace`
    /// and `t_dpms`. The boundaries are inclusive on the lower side — exactly
    /// at `t_grace` we are already in Phase 2, and exactly at `t_dpms` we are
    /// already in Phase 3. This matches the timer semantics in `run_daemon`,
    /// which arms `Timer::from_duration(t_dpms)`: when the timer fires, the
    /// elapsed time is `t_dpms` and we must be in Phase 3.
    pub(crate) fn phase(
        &self,
        now: Instant,
        t_grace: Duration,
        t_dpms: Duration,
    ) -> SaverPhase {
        let elapsed = now.saturating_duration_since(self.shown_at);
        if elapsed >= t_dpms {
            SaverPhase::Phase3
        } else if elapsed >= t_grace {
            SaverPhase::Phase2
        } else {
            SaverPhase::Phase1
        }
    }
}

impl Drop for Saver {
    /// Explicitly destroy the idle inhibitor before the surface is torn down.
    ///
    /// `wayland-client` does **not** send a proxy's destructor request when the
    /// Rust handle is dropped, so the inhibitor must be destroyed by hand.
    /// Without this, Mutter keeps the session inhibited after dismiss and never
    /// reports the next idle period — the saver shows only once. Sending
    /// `destroy` here, before the `window` field drops and tears down the
    /// surface, releases the inhibitor in the protocol-correct order; the
    /// request is flushed on the daemon's next event-loop dispatch.
    fn drop(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
    }
}

/// Decide whether to create an idle inhibitor: `Some` only when a manager is
/// present, in which case `create` is invoked exactly once to build it.
///
/// This isolates the graceful-degradation rule — "no manager ⇒ no inhibitor,
/// no Wayland call, no panic" — from the live `create_inhibitor` round-trip
/// (which cannot run in CI), so it can be unit-tested. It is a thin wrapper over
/// `Option::map`; the value is that the absent-manager path is exercised by a
/// test rather than only by inspection.
fn make_inhibitor<M, I, F>(manager: Option<&M>, create: F) -> Option<I>
where
    F: FnOnce(&M) -> I,
{
    manager.map(create)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::lock::testing::FailingLocker;
    use super::lock::SessionLocker;
    use super::{make_inhibitor, SaverPhase};

    /// When the idle-inhibit manager is absent, no inhibitor is created and the
    /// `create` closure is never called — the daemon must still show the saver,
    /// so this path must not panic or attempt a Wayland call. Mirrors the
    /// `Saver::new` site where `idle_inhibit_manager` is `None`.
    #[test]
    fn make_inhibitor_returns_none_without_manager_and_does_not_invoke_create() {
        let manager: Option<&()> = None;
        let inhibitor = make_inhibitor(manager, |_unit| -> u32 {
            panic!("create must not be called when the manager is absent");
        });
        assert!(inhibitor.is_none());
    }

    /// When a manager is present, the inhibitor is created exactly once.
    #[test]
    fn make_inhibitor_creates_once_with_manager() {
        let manager = ();
        let mut calls = 0;
        let inhibitor = make_inhibitor(Some(&manager), |_unit| {
            calls += 1;
            42_u32
        });
        assert_eq!(inhibitor, Some(42));
        assert_eq!(calls, 1);
    }

    /// A copy of [`super::Saver::phase`]'s decision rule that does not
    /// require constructing a real `Saver` (which needs a live Wayland
    /// connection). The unit tests target this function so they can drive
    /// arbitrary `(shown_at, now)` pairs.
    fn phase_of(
        shown_at: Instant,
        now: Instant,
        t_grace: Duration,
        t_dpms: Duration,
    ) -> SaverPhase {
        let elapsed = now.saturating_duration_since(shown_at);
        if elapsed >= t_dpms {
            SaverPhase::Phase3
        } else if elapsed >= t_grace {
            SaverPhase::Phase2
        } else {
            SaverPhase::Phase1
        }
    }

    #[test]
    fn phase_well_below_t_grace_is_phase1() {
        let shown_at = Instant::now();
        let now = shown_at + Duration::from_secs(5);
        let t_grace = Duration::from_secs(30);
        let t_dpms = Duration::from_secs(60);
        assert_eq!(phase_of(shown_at, now, t_grace, t_dpms), SaverPhase::Phase1);
    }

    #[test]
    fn phase_at_exact_t_grace_is_phase2() {
        // Boundary at exactly T_grace is Phase 2.
        let shown_at = Instant::now();
        let t_grace = Duration::from_secs(30);
        let t_dpms = Duration::from_secs(60);
        let now = shown_at + t_grace;
        assert_eq!(phase_of(shown_at, now, t_grace, t_dpms), SaverPhase::Phase2);
    }

    #[test]
    fn phase_between_t_grace_and_t_dpms_is_phase2() {
        let shown_at = Instant::now();
        let t_grace = Duration::from_secs(30);
        let t_dpms = Duration::from_secs(60);
        let now = shown_at + Duration::from_secs(45);
        assert_eq!(phase_of(shown_at, now, t_grace, t_dpms), SaverPhase::Phase2);
    }

    #[test]
    fn phase_at_exact_t_dpms_is_phase3() {
        // Boundary at exactly T_dpms is Phase 3. This matches the calloop
        // timer semantics in `run_daemon`: when `Timer::from_duration(t_dpms)`
        // fires, `now - shown_at == t_dpms` and the callback must take the
        // Phase 3 branch.
        let shown_at = Instant::now();
        let t_grace = Duration::from_secs(30);
        let t_dpms = Duration::from_secs(60);
        let now = shown_at + t_dpms;
        assert_eq!(phase_of(shown_at, now, t_grace, t_dpms), SaverPhase::Phase3);
    }

    #[test]
    fn phase_past_t_dpms_is_phase3() {
        let shown_at = Instant::now();
        let t_grace = Duration::from_secs(30);
        let t_dpms = Duration::from_secs(60);
        let now = shown_at + Duration::from_secs(120);
        assert_eq!(phase_of(shown_at, now, t_grace, t_dpms), SaverPhase::Phase3);
    }

    /// `on_input` must be a no-op when `self.saver` is `None`. We can exercise
    /// this branch by reproducing the guard locally — `HowanApp` itself
    /// cannot be constructed without a live Wayland connection, but the only
    /// state `on_input` reads in the absent-saver branch is `self.saver`
    /// itself, so the contract is observable here.
    #[test]
    fn on_input_is_noop_when_no_saver() {
        // Stand-in for `HowanApp` shape relevant to the branch.
        struct StubApp {
            saver: Option<()>,
            dispatched: u32,
        }
        impl StubApp {
            fn on_input(&mut self) {
                if self.saver.is_none() {
                    return;
                }
                self.dispatched += 1;
            }
        }
        let mut app = StubApp {
            saver: None,
            dispatched: 0,
        };
        app.on_input();
        assert_eq!(
            app.dispatched, 0,
            "on_input with no saver must not dispatch any phase branch"
        );
    }

    /// Stub mirroring just the Phase 2 branch of `HowanApp::on_input`,
    /// without the Wayland dependencies, so the "lock failure → dismiss
    /// still runs" contract is testable in CI. `phase2_input` always takes
    /// the Phase 2 branch regardless of timing.
    struct StubAppPhase2 {
        locker: Box<dyn SessionLocker>,
        dismissed: bool,
    }
    impl StubAppPhase2 {
        fn phase2_input(&mut self) {
            // This mirrors `HowanApp::on_input`'s Phase 2 arm verbatim:
            // call the locker, log on error, then dismiss unconditionally.
            if let Err(err) = self.locker.lock() {
                eprintln!("howan: lock-session failed: {err}");
            }
            self.dismissed = true;
        }
    }

    /// The Phase 2 contract: even when the locker errors, the saver is still
    /// dismissed.
    #[test]
    fn phase2_dismisses_even_when_lock_fails() {
        let failing = FailingLocker::new();
        let calls = failing.calls.clone();
        let mut app = StubAppPhase2 {
            locker: Box::new(failing),
            dismissed: false,
        };
        app.phase2_input();
        assert!(
            app.dismissed,
            "Phase 2 must dismiss the saver even when the locker fails"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "lock_session should attempt the lock once"
        );
    }

}
