//! The composited Wayland saver surface and the state that drives it.
//!
//! `HowanApp` owns the durable Wayland state — the registry, seat, output, and
//! the `wl_compositor` / `xdg_wm_base` globals — plus the durable wgpu GPU state
//! ([`Gpu`]) and the `wl_display` connection, all persisting for the whole
//! lifetime of the process. The saver itself (an output-sized composited
//! `xdg_toplevel` plus its per-surface GPU [`Renderer`]) is split out into
//! [`Saver`] and held in `HowanApp::saver` as an `Option`, so it can be created
//! on demand and dropped on dismiss **without** tearing down the connection or
//! the GPU device. See `docs/guides/50-shader-player.md`.
//!
//! Two entry points use this state:
//!
//! - [`run`] — the one-shot `howan start`: it shows the saver immediately and
//!   exits the process on the first input (the manual/debug path).
//! - [`run_daemon`] — the resident `howan daemon`: it stays connected with no
//!   surface, shows the saver when the idle source fires, dispatches input by
//!   the elapsed-time two-phase lifecycle (see [`SaverPhase`] and
//!   `docs/guides/40-resident-daemon.md`), and on dismiss drops the *surface*
//!   (not the process), re-arming for the next idle cycle. The DpmsHandoff timer
//!   releases the idle inhibitor (so the compositor can blank the display)
//!   but leaves the surface up, so the desktop is never exposed behind the
//!   saver during the compositor's blank-countdown window.
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
//! SCTK is used for the compositor / xdg-shell / seat / pointer / touch glue,
//! but `wl_keyboard` is bound directly through `wayland-client` so we can avoid
//! pulling in libxkbcommon at build time. We only need to know that some key was
//! pressed; full keymap interpretation is unnecessary.
//!
//! The module is split into three files so the boundaries that future
//! milestones will cross are explicit:
//!
//! - `app.rs` (this file) holds `run`, `run_daemon`, and the top-level
//!   `HowanApp` / `Saver` state.
//! - `app::render` owns the GPU-backed WGSL renderer: the durable wgpu device /
//!   pipeline ([`Gpu`]) and the per-surface [`Renderer`] that animates the
//!   bundled shader once per Wayland frame callback. Isolating it here kept the
//!   earlier swap from the `wl_shm` renderer local.
//! - `app::handlers` contains every Wayland-protocol handler trait impl
//!   plus the `delegate_*!` macros.
//!
//! # Locking is delegated to GNOME
//!
//! howan no longer issues `loginctl lock-session` / `Session.Lock` on input
//! after a grace period. The saver is a purely visual screensaver that hides
//! the desktop while idle and dismisses on input; responsibility for actually
//! locking the session belongs to GNOME's own configuration
//! (`org.gnome.desktop.screensaver lock-enabled` / `lock-delay` /
//! `org.gnome.desktop.session idle-delay`). See Q-phase2-lock in the howan
//! plan for the rationale.

mod handlers;
mod render;

use std::error::Error;
use std::rc::Rc;
use std::time::{Duration, Instant};

use calloop::channel::{channel, Event as ChannelEvent};
use calloop::signals::{Signal, Signals};
use calloop::timer::{TimeoutAction, Timer};
use calloop::RegistrationToken;
use tracing::{error, info, warn};
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

use self::render::{Gpu, Renderer};
use crate::daemon::{IdleEvent, IdleSource};
use crate::pidfile::PidFileGuard;

/// Pre-configure starting size for the renderer's surface, never the intended
/// final size. The window is resized to the active output's current mode
/// dimensions as soon as the output geometry is known (see
/// `resize_to_active_output`), which reconfigures the wgpu swapchain.
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
    // the DpmsHandoff state in practice. A `T_dpms` large enough never to fire
    // is sufficient.
    let mut app = HowanApp::new(&globals, &qh, conn.clone(), Duration::from_secs(u64::MAX / 2))?;
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
/// that the seat has been idle for `T1`. Input tears down the saver surface,
/// the DpmsHandoff timer releases the idle inhibitor (leaving the surface mapped
/// so the desktop is not exposed when the compositor blanks the display), and
/// either path drives the daemon to re-arm the idle source while staying
/// resident for the next cycle. `SIGTERM`/`SIGINT` terminate the whole daemon
/// cleanly.
///
/// The loop consumes idle events through the [`IdleSource`] trait, so a future
/// backend (e.g. `ext-idle-notify-v1` on wlroots) can be dropped in by writing
/// a new `IdleSource` implementation without touching this function.
///
/// `t_dpms` comes from the daemon CLI flag and defines the boundary between
/// the `Inhibiting` state (input dismisses) and the `DpmsHandoff` state — see
/// [`SaverPhase`] and `docs/guides/40-resident-daemon.md`.
pub fn run_daemon(
    idle_source: Box<dyn IdleSource>,
    t_dpms: Duration,
) -> Result<(), Box<dyn Error>> {
    // Lifecycle: log the effective thresholds and the selected idle backend
    // exactly once at the top of the daemon run so `journalctl --user -u
    // howan.service --since ...` shows what the daemon was configured with.
    // See docs/guides/40-resident-daemon.md ("Verifying the daemon via the
    // journal").
    info!(
        backend = idle_source.backend_name(),
        t1_secs = idle_source.t1().as_secs(),
        t_dpms_secs = t_dpms.as_secs(),
        "daemon starting"
    );

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

    let mut app = HowanApp::new(&globals, &qh, conn.clone(), t_dpms)?;

    // The DpmsHandoff timer source. We register/cancel a calloop Timer for
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
        // The DpmsHandoff timer-fire path itself does **not** drop the saver
        // anymore (the surface is kept up to hide the desktop during the
        // compositor's blank window), so `saver.is_none()` here means an
        // input dismiss has just happened. If the DpmsHandoff timer already
        // fired, the calloop closure above returned `TimeoutAction::Drop`
        // and the source has already been removed — so
        // `loop_handle.remove(token)` below finds a stale handle. That is
        // harmless: calloop silently ignores unknown tokens.
        if app.saver.is_none() {
            if let Some(token) = dpms_timer_token.take() {
                loop_handle.remove(token);
            }
        }

        // Both re-arm paths set `pending_rearm`: input dismiss via
        // `dismiss` (Immediate) and the DpmsHandoff timer via `dpms_handoff`
        // (AfterActive, with the surface intentionally still up). Route
        // the variant to the matching re-arm primitive — see
        // [`RearmIntent`] for what each variant means.
        match app.take_pending_rearm() {
            Some(RearmIntent::Immediate) => idle_source.rearm()?,
            Some(RearmIntent::AfterActive) => idle_source.rearm_after_active()?,
            None => {}
        }
    }

    app.release_input_handles();
    info!("daemon shutting down");
    Ok(())
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
    /// The durable, process-lifetime wgpu state (instance/adapter/device/queue,
    /// the compiled shader pipeline, and the uniform buffer). Creating a wgpu
    /// device is expensive, so it is built once and shared (behind an `Rc`) with
    /// every per-cycle `Saver`'s `Renderer`, which only rebuilds the cheap
    /// per-surface wgpu objects. See `app::render` and
    /// `docs/guides/50-shader-player.md`.
    gpu: Rc<Gpu>,
    /// The Wayland connection, kept for the whole process. The GPU renderer
    /// reads the raw `wl_display` pointer from its backend to create the wgpu
    /// surface (see `Renderer::new`). Holding it here also keeps that pointer
    /// valid for as long as any wgpu surface derived from it exists.
    conn: Connection,
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
    /// The serial of the most recent `wl_pointer.enter` event delivered to the
    /// saver surface, cached so [`dpms_handoff`] can re-issue
    /// `wl_pointer.set_cursor` with a null cursor. The initial Enter (handled in
    /// `app/handlers.rs`) hides the cursor for the saver's lifetime, but Mutter
    /// re-evaluates seat state when the idle inhibitor is destroyed and renders
    /// its default cursor on top of the still-mapped saver in the DpmsHandoff
    /// → DPMS-off window. Re-applying the null cursor with the cached serial at
    /// `dpms_handoff` keeps the saver visually clean through that window. `None`
    /// until the first Enter on the saver, in which case no re-apply is possible
    /// (and the cursor was already not hidden, matching pre-task behavior).
    ///
    /// [`dpms_handoff`]: HowanApp::dpms_handoff
    pub(crate) last_pointer_enter_serial: Option<u32>,
    /// The output the saver surface is shown on. We track the output the
    /// surface entered ("active output only"); until a surface-enter event
    /// arrives we fall back to the first advertised output.
    pub(crate) active_output: Option<WlOutput>,
    /// `Inhibiting` → `DpmsHandoff` boundary (`T_dpms` in the design): the
    /// daemon arms a calloop timer for this duration when the saver is shown
    /// and on fire releases the idle inhibitor (via [`HowanApp::dpms_handoff`])
    /// so the compositor's own idle blank can take over. The saver surface
    /// itself is **kept mapped** through the compositor's blank window so the
    /// desktop is not exposed behind it — see [`SaverPhase::DpmsHandoff`].
    t_dpms: Duration,
    /// Set by the `SIGTERM`/`SIGINT` handler to terminate the whole process.
    /// Input dismiss does **not** set this — it only drops `saver`.
    exit: bool,
    /// Set when the daemon loop should re-arm its idle source after an
    /// `Inhibiting` / `DpmsHandoff` event. The variant records *which* path ran
    /// so `run_daemon` can pick between the two re-arm primitives on
    /// [`IdleSource`](crate::daemon::IdleSource): [`dismiss`] sets
    /// `Immediate` (input tore down the surface), and [`dpms_handoff`] sets
    /// `AfterActive` (the DpmsHandoff timer destroyed the inhibitor while
    /// keeping the surface mapped). Cleared by [`take_pending_rearm`].
    ///
    /// [`dismiss`]: HowanApp::dismiss
    /// [`dpms_handoff`]: HowanApp::dpms_handoff
    /// [`take_pending_rearm`]: HowanApp::take_pending_rearm
    pending_rearm: Option<RearmIntent>,
}

/// Which re-arm primitive on [`IdleSource`](crate::daemon::IdleSource) the
/// daemon loop should call after a dismiss. Decided at dismiss time so the
/// loop does not have to re-derive the phase that ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RearmIntent {
    /// `Inhibiting` input dismiss: the user produced input, arm a fresh idle
    /// watch immediately.
    Immediate,
    /// `DpmsHandoff`: the handoff destroyed the idle inhibitor
    /// without any input (the saver surface stays mapped — see
    /// [`HowanApp::dpms_handoff`]), so the next idle watch must be gated on
    /// a user-active transition (see Q4 in the howan plan).
    AfterActive,
}

/// The phase the saver is currently in, decided by how long it has been
/// shown. See [`Saver::phase`] and `docs/guides/40-resident-daemon.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaverPhase {
    /// Up to `T_dpms` after the saver was shown: the idle inhibitor is held
    /// (the compositor's DPMS blank is suppressed) and the saver covers the
    /// screen. Input dismisses the saver outright.
    Inhibiting,
    /// `T_dpms` (inclusive) and onward: the daemon's calloop timer has
    /// released the idle inhibitor, handing off to the compositor's standard
    /// DPMS blank, while leaving the saver surface mapped as a cover — the
    /// desktop stays hidden behind the saver through the compositor's blank
    /// window. Input that wakes the display now lands on the saver, and this
    /// branch is the normal path: it dismisses the surface like `Inhibiting`
    /// does.
    DpmsHandoff,
}

/// The recreatable on-screen saver: an output-sized composited toplevel and the
/// GPU-backed WGSL renderer that paints it. Dropped on dismiss; recreated each
/// idle cycle by [`HowanApp::show_saver`].
pub(crate) struct Saver {
    /// The wgpu renderer. Declared **before** `window` so it drops first: the
    /// wgpu surface inside it wraps the raw `wl_surface` pointer owned by
    /// `window`, and the surface must not outlive that pointer. Rust drops
    /// struct fields in declaration order, so this ordering is load-bearing.
    pub(crate) renderer: Renderer,
    pub(crate) window: Window,
    /// Whether a `wl_surface.frame` callback is currently in flight. The
    /// per-frame loop requests a callback only when none is pending, so layout
    /// events (configure / output changes) that also paint do not stack
    /// multiple callbacks and spin the loop faster than vsync. Set when a
    /// callback is requested, cleared in [`HowanApp::on_frame`].
    frame_pending: bool,
    /// Set once the xdg surface has received its first configure. We must not
    /// attach a buffer and commit before that (xdg-shell forbids committing a
    /// buffer to a surface that has never been configured); output/seat events
    /// can otherwise trigger `draw()` too early. Strict compositors (e.g.
    /// `weston`) reject the premature commit; Mutter tolerates it.
    pub(crate) configured: bool,
    /// The idle inhibitor held against this saver's `wl_surface` for as long as
    /// the saver should suppress the compositor's idle blank, so the
    /// compositor does not blank the display (DPMS off) behind it. `None` in
    /// either of two cases: the idle-inhibit manager global was absent at
    /// startup, or the inhibitor has already been destroyed by
    /// [`HowanApp::dpms_handoff`] (at `T_dpms`) before the surface itself was
    /// dropped.
    ///
    /// Released either by [`HowanApp::dpms_handoff`] (the DpmsHandoff timer,
    /// surface stays mapped) or by [`Saver`]'s `Drop` (input dismiss, surface goes
    /// away). Both sites explicitly send `zwp_idle_inhibitor_v1.destroy`.
    /// This **must** be explicit: `wayland-client` proxies do not send their
    /// destructor request when the Rust handle is dropped, so without it the
    /// inhibitor leaks and Mutter keeps treating the session as non-idle even
    /// after dismiss — blocking both the DPMS resume and the next idle
    /// detection (the saver would show only once).
    inhibitor: Option<ZwpIdleInhibitorV1>,
    /// The instant this saver was constructed — i.e. when the saver first
    /// became visible for the current cycle. It is the single source of truth
    /// for the two-phase lifecycle: [`Saver::phase`] compares `now - shown_at`
    /// against `T_dpms` to decide whether input or the timer should take
    /// the `Inhibiting` or `DpmsHandoff` branch.
    shown_at: Instant,
}

impl HowanApp {
    /// Bind the durable Wayland globals. No surface is created here; call
    /// [`show_saver`](HowanApp::show_saver) to put the saver on screen.
    ///
    /// `t_dpms` defines the `Inhibiting` → `DpmsHandoff` boundary — see
    /// [`SaverPhase`] and `docs/guides/40-resident-daemon.md`.
    fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<HowanApp>,
        conn: Connection,
        t_dpms: Duration,
    ) -> Result<Self, Box<dyn Error>> {
        let compositor = CompositorState::bind(globals, qh)
            .map_err(|err| format!("wl_compositor not available: {err}"))?;
        let xdg_shell = XdgShell::bind(globals, qh)
            .map_err(|err| format!("xdg_wm_base not available: {err}"))?;

        // Build the durable wgpu state once. This is the expensive
        // adapter/device request plus shader-pipeline compilation; the
        // per-cycle `Saver` reuses it and only rebuilds the per-surface objects.
        let gpu = Rc::new(Gpu::new()?);

        // Bind the idle-inhibit manager through the existing GlobalList. This is
        // best-effort: a compositor without the global degrades to "no
        // inhibitor", and the saver still shows. We log once so the absence is
        // diagnosable, then keep `None`. Unlike the compositor / xdg globals
        // above, a missing idle-inhibit manager is *not* fatal — DPMS
        // suppression is an enhancement to the saver, not a precondition for it.
        let idle_inhibit_manager = match globals.bind::<ZwpIdleInhibitManagerV1, _, _>(qh, 1..=1, ())
        {
            Ok(manager) => Some(manager),
            Err(err) => {
                warn!(
                    error = %err,
                    "idle-inhibit manager unavailable; \
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
            gpu,
            conn,
            qh: qh.clone(),
            idle_inhibit_manager,
            saver: None,
            keyboard: None,
            pointer: None,
            touch: None,
            active_output: None,
            t_dpms,
            exit: false,
            pending_rearm: None,
            last_pointer_enter_serial: None,
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
            &self.gpu,
            &self.conn,
            self.idle_inhibit_manager.as_ref(),
            qh,
        ) {
            Ok(saver) => {
                let inhibitor_acquired = saver.inhibitor.is_some();
                self.saver = Some(saver);
                info!(
                    inhibitor_acquired,
                    "saver shown"
                );
                if inhibitor_acquired {
                    info!("inhibitor acquired");
                }
            }
            Err(err) => error!(error = %err, "failed to create saver surface"),
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

    /// Render one animated frame of the shader and ensure the per-frame loop is
    /// running. No-op when no saver is shown.
    ///
    /// `iTime` is derived from `now - shown_at`, so the shader animates from the
    /// moment the saver became visible. wgpu's `present()` inside
    /// `Renderer::render` commits the `wl_surface`, replacing the old `wl_shm`
    /// attach + `wl_surface.commit` flow — the caller no longer commits the
    /// window itself.
    ///
    /// The next `wl_surface.frame` callback is requested *before* presenting
    /// (unless one is already in flight), because wgpu's `present()` issues the
    /// `wl_surface.commit` and a frame callback only takes effect on the commit
    /// that follows its request. Requesting it after `render()` would leave it
    /// in pending surface state that nothing commits, so [`HowanApp::on_frame`]
    /// would never fire and the loop would stall after a single frame. With the
    /// request in place first, the present commit carries it and wakes
    /// `on_frame` to paint the next frame. The loop is compositor-paced
    /// (typically vsync), which caps the FPS without a busy-loop; it stops
    /// naturally when the surface is dropped on dismiss.
    pub(crate) fn draw(&mut self) {
        let qh = self.qh.clone();
        if let Some(saver) = self.saver.as_mut() {
            let elapsed = Instant::now().saturating_duration_since(saver.shown_at);
            saver.request_frame_if_idle(&qh);
            saver.renderer.render(elapsed);
        }
    }

    /// Wayland frame-callback tick: advance and render the next animated frame.
    ///
    /// Driven by `CompositorHandler::frame` (see `app/handlers.rs`). Clears the
    /// in-flight flag, then `draw` paints and re-requests the next callback,
    /// keeping the loop going. A frame callback that arrives after the saver was
    /// dismissed (raced teardown) finds no saver and is a no-op.
    pub(crate) fn on_frame(&mut self, surface: &wayland_client::protocol::wl_surface::WlSurface) {
        let Some(saver) = self.saver.as_mut() else {
            return;
        };
        // Ignore callbacks for any surface other than the current saver's.
        if surface != saver.window.wl_surface() {
            return;
        }
        saver.frame_pending = false;
        self.draw();
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

    /// Tear down the saver surface on the *immediate re-arm* path (input or a
    /// compositor close request).
    ///
    /// This is the "drop surface + flag re-arm" primitive: it drops `saver`,
    /// forgets the active output, and flags the daemon loop to re-arm
    /// *immediately* (the [`RearmIntent::Immediate`] variant). It does
    /// **not** set the process-exit flag — in the daemon, dismiss means "stay
    /// resident". The one-shot `run` loop notices `saver` is `None` and exits
    /// on its own. Idempotent: repeated calls after the first dismiss do
    /// nothing.
    ///
    /// Higher-level entry points dispatch to this primitive by phase:
    ///
    /// - Input goes through [`on_input`](HowanApp::on_input). The DpmsHandoff
    ///   arm reaches `dismiss` after a prior [`dpms_handoff`](HowanApp::dpms_handoff)
    ///   has already destroyed the inhibitor and left the surface mapped; the
    ///   `Immediate` set here overrides the `AfterActive` the handoff set.
    /// - The DpmsHandoff calloop timer goes through
    ///   [`dpms_handoff`](HowanApp::dpms_handoff), the *separate* "release
    ///   inhibitor + flag re-arm" primitive for the no-input path: it keeps
    ///   the `Saver` surface mapped (so the desktop is not exposed during the
    ///   compositor's blank window) and uses the [`RearmIntent::AfterActive`]
    ///   variant — see that method.
    /// - A compositor-issued close request goes through `dismiss` directly
    ///   (no phase logic — the compositor's "please close" is unconditional).
    pub(crate) fn dismiss(&mut self) {
        if let Some(saver) = self.saver.take() {
            self.active_output = None;
            self.pending_rearm = Some(RearmIntent::Immediate);
            // If the inhibitor is still held (i.e. dismiss happened *before*
            // the DpmsHandoff timer fired), `Saver`'s `Drop` will destroy it;
            // surface that as a structured release event with the dismiss
            // reason. After a DpmsHandoff the field is already `None`
            // and the corresponding release was logged at that site with
            // reason = "dpms_handoff", so we skip the log here.
            let elapsed_since_shown = Instant::now().saturating_duration_since(saver.shown_at);
            info!(
                elapsed_since_shown_ms = elapsed_since_shown.as_millis() as u64,
                "saver dismissed"
            );
            if saver.inhibitor.is_some() {
                info!(reason = "dismiss", "inhibitor released");
            }
        }
    }

    /// Dispatch on user input according to the current saver phase.
    ///
    /// - `Inhibiting`: drop the surface (the M3 behavior).
    /// - `DpmsHandoff`: the **normal** path for input after the DPMS
    ///   handoff. The handoff released the inhibitor but left the surface
    ///   mapped, so the compositor blanked the display behind the saver
    ///   (not the desktop). Input wakes the display to the saver, and this
    ///   branch dismisses it like `Inhibiting` does, overriding the pending
    ///   `AfterActive` intent with `Immediate`. In the corner case where
    ///   input lands at the exact `T_dpms` boundary before the calloop
    ///   timer has dispatched, the inhibitor is still held — `Saver`'s
    ///   `Drop` (run by `dismiss`) then destroys it, matching the
    ///   pre-`T_dpms` cleanup.
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
        let now = Instant::now();
        let phase = saver.phase(now, self.t_dpms);
        let elapsed_since_shown = now.saturating_duration_since(saver.shown_at);
        info!(
            phase = ?phase,
            elapsed_since_shown_ms = elapsed_since_shown.as_millis() as u64,
            "input received"
        );
        match phase {
            SaverPhase::Inhibiting => self.dismiss(),
            SaverPhase::DpmsHandoff => self.dismiss(),
        }
    }

    /// DpmsHandoff timer callback: release the inhibitor while keeping the saver
    /// surface mapped, so the compositor's standard idle blank can take over
    /// without exposing the desktop behind the saver. The next input then
    /// routes through [`on_input`](HowanApp::on_input)'s DpmsHandoff branch, which
    /// calls [`dismiss`](HowanApp::dismiss) to tear the surface down.
    ///
    /// Unlike [`dismiss`](HowanApp::dismiss), this runs without any user
    /// input — the seat is still idle — so it flags
    /// [`RearmIntent::AfterActive`] instead of `Immediate`. `run_daemon`
    /// then calls
    /// [`IdleSource::rearm_after_active`](crate::daemon::IdleSource::rearm_after_active),
    /// which gates the next idle watch on a real user-active transition to
    /// avoid racing the compositor's own idle blank. See
    /// `docs/guides/40-resident-daemon.md` (the DpmsHandoff handoff) and Q4 in
    /// the howan plan.
    ///
    /// The inhibitor is destroyed via the same `zwp_idle_inhibitor_v1.destroy`
    /// call [`Saver`]'s `Drop` performs; the `Saver::inhibitor` field is left
    /// `None`, so the eventual `Saver::Drop` becomes a no-op for the inhibitor.
    ///
    /// Idempotent: if the saver is already gone (e.g. input dismiss won a
    /// tight race against the calloop timer in the same dispatch tick) this
    /// is a no-op — the pending re-arm intent already set by the winning
    /// path is left alone.
    pub(crate) fn dpms_handoff(&mut self) {
        if let Some(saver) = self.saver.as_mut() {
            let elapsed_since_shown = Instant::now().saturating_duration_since(saver.shown_at);
            let inhibitor_was_held = saver.inhibitor.is_some();
            if let Some(inhibitor) = saver.inhibitor.take() {
                inhibitor.destroy();
            }
            // Re-apply the null cursor for the saver surface. The initial Enter
            // hides the cursor (see `app/handlers.rs`), but Mutter re-evaluates
            // seat state when the idle inhibitor is destroyed and renders its
            // default cursor on top of the still-mapped saver until DPMS off.
            // Re-issuing `set_cursor` with the cached Enter serial keeps the
            // saver visually clean through the compositor's blank window. When
            // the pointer has not yet entered the saver (`last_pointer_enter_serial
            // == None`) there is nothing to re-apply.
            if let (Some(pointer), Some(serial)) =
                (self.pointer.as_ref(), self.last_pointer_enter_serial)
            {
                pointer.set_cursor(serial, None, 0, 0);
            }
            self.pending_rearm = Some(RearmIntent::AfterActive);
            info!(
                elapsed_since_shown_ms = elapsed_since_shown.as_millis() as u64,
                "phase transition: Inhibiting -> DpmsHandoff"
            );
            if inhibitor_was_held {
                info!(reason = "dpms_handoff", "inhibitor released");
            }
            info!("dpms handoff: saver surface retained");
        }
    }

    /// Request termination of the whole process (set by the signal handler).
    fn request_exit(&mut self) {
        info!("signal received; daemon shutting down");
        self.exit = true;
    }

    /// Whether the process should quit (a signal was received).
    fn should_quit(&self) -> bool {
        self.exit
    }

    /// Take and clear the pending re-arm intent set by [`dismiss`] or
    /// [`dpms_handoff`]. `None` means there is nothing to do this tick.
    ///
    /// [`dismiss`]: HowanApp::dismiss
    /// [`dpms_handoff`]: HowanApp::dpms_handoff
    fn take_pending_rearm(&mut self) -> Option<RearmIntent> {
        self.pending_rearm.take()
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
    /// The wgpu renderer wraps this `wl_surface` to drive the GPU shader; that
    /// wrapping (`Renderer::new`) takes no `set_fullscreen` step and declares no
    /// opaque region — it only reads the raw `wl_display` / `wl_surface`
    /// pointers. The shader outputs opaque pixels (alpha 1.0) for appearance,
    /// which is a separate thing from declaring an opaque *region* on the
    /// surface; only the latter governs Mutter's scanout eligibility.
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
        gpu: &Rc<Gpu>,
        conn: &Connection,
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

        // Wrap the just-created `wl_surface` in a wgpu surface sharing the
        // durable GPU/device. This only reads the raw `wl_display` /
        // `wl_surface` pointers (see `Renderer::new`); it does not call
        // `set_fullscreen` and does not declare an opaque region.
        let renderer = Renderer::new(gpu, conn, window.wl_surface(), INITIAL_WIDTH, INITIAL_HEIGHT)?;

        // Hold an idle inhibitor against the saver surface so the compositor does
        // not blank the display behind it. When the manager is `None` (global
        // absent) `make_inhibitor` produces `None` without any Wayland call — see
        // its unit test.
        let inhibitor = make_inhibitor(idle_inhibit_manager, |manager| {
            manager.create_inhibitor(window.wl_surface(), qh, ())
        });

        Ok(Self {
            renderer,
            window,
            frame_pending: false,
            configured: false,
            inhibitor,
            shown_at: Instant::now(),
        })
    }

    /// Request a `wl_surface.frame` callback for the next frame, unless one is
    /// already in flight.
    ///
    /// This is the throttle that keeps the per-frame loop at the compositor's
    /// pace: layout events (configure / output changes) paint and call this too,
    /// but they will not stack a second callback while the loop's own callback
    /// is still pending. Once the callback fires, [`HowanApp::on_frame`] clears
    /// the flag and re-requests, so exactly one callback is outstanding at a
    /// time. The callback stops being re-requested when the saver is dropped.
    fn request_frame_if_idle(&mut self, qh: &QueueHandle<HowanApp>) {
        if self.frame_pending {
            return;
        }
        let surface = self.window.wl_surface();
        surface.frame(qh, surface.clone());
        self.frame_pending = true;
    }

    /// Decide which phase the saver is currently in.
    ///
    /// The decision is pure: it compares `now - shown_at` against `t_dpms`.
    /// The boundary is inclusive on the lower side — exactly at `t_dpms` we
    /// are already in `DpmsHandoff`. This matches the timer semantics in
    /// `run_daemon`, which arms `Timer::from_duration(t_dpms)`: when the
    /// timer fires, the elapsed time is `t_dpms` and we must be in
    /// `DpmsHandoff`.
    pub(crate) fn phase(&self, now: Instant, t_dpms: Duration) -> SaverPhase {
        let elapsed = now.saturating_duration_since(self.shown_at);
        if elapsed >= t_dpms {
            SaverPhase::DpmsHandoff
        } else {
            SaverPhase::Inhibiting
        }
    }
}

impl Drop for Saver {
    /// Explicitly destroy the idle inhibitor before the surface is torn down,
    /// unless [`HowanApp::dpms_handoff`] has already taken and destroyed it.
    ///
    /// `wayland-client` does **not** send a proxy's destructor request when the
    /// Rust handle is dropped, so the inhibitor must be destroyed by hand.
    /// Without this, Mutter keeps the session inhibited after dismiss and never
    /// reports the next idle period — the saver shows only once. Sending
    /// `destroy` here, before the `window` field drops and tears down the
    /// surface, releases the inhibitor in the protocol-correct order; the
    /// request is flushed on the daemon's next event-loop dispatch.
    ///
    /// After a DpmsHandoff the `inhibitor` field is already `None`
    /// (`HowanApp::dpms_handoff` took it via `Option::take` and called
    /// `destroy` itself), so this becomes a no-op — exactly the same code
    /// path the absent-manager case takes.
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
    use std::time::{Duration, Instant};

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
    fn phase_of(shown_at: Instant, now: Instant, t_dpms: Duration) -> SaverPhase {
        let elapsed = now.saturating_duration_since(shown_at);
        if elapsed >= t_dpms {
            SaverPhase::DpmsHandoff
        } else {
            SaverPhase::Inhibiting
        }
    }

    #[test]
    fn phase_well_below_t_dpms_is_inhibiting() {
        let shown_at = Instant::now();
        let now = shown_at + Duration::from_secs(5);
        let t_dpms = Duration::from_secs(60);
        assert_eq!(phase_of(shown_at, now, t_dpms), SaverPhase::Inhibiting);
    }

    /// A saver shown well above zero but below `T_dpms` is still in
    /// `Inhibiting`: only `T_dpms` matters and there is no intermediate
    /// boundary, so input dismisses outright regardless of how long the
    /// saver has been up — even an hour into the cycle.
    #[test]
    fn phase_anywhere_below_t_dpms_is_inhibiting() {
        let shown_at = Instant::now();
        // 30 s on the test clock stands in for "the saver has been up a
        // long time"; the result must still be `Inhibiting`.
        let now = shown_at + Duration::from_secs(30);
        let t_dpms = Duration::from_secs(60);
        assert_eq!(phase_of(shown_at, now, t_dpms), SaverPhase::Inhibiting);
    }

    #[test]
    fn phase_at_exact_t_dpms_is_dpms_handoff() {
        // Boundary at exactly T_dpms is `DpmsHandoff`. This matches the calloop
        // timer semantics in `run_daemon`: when `Timer::from_duration(t_dpms)`
        // fires, `now - shown_at == t_dpms` and the callback must take the
        // `DpmsHandoff` branch.
        let shown_at = Instant::now();
        let t_dpms = Duration::from_secs(60);
        let now = shown_at + t_dpms;
        assert_eq!(phase_of(shown_at, now, t_dpms), SaverPhase::DpmsHandoff);
    }

    #[test]
    fn phase_past_t_dpms_is_dpms_handoff() {
        let shown_at = Instant::now();
        let t_dpms = Duration::from_secs(60);
        let now = shown_at + Duration::from_secs(120);
        assert_eq!(phase_of(shown_at, now, t_dpms), SaverPhase::DpmsHandoff);
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

    /// `dismiss` (input path) and `dpms_handoff` (DpmsHandoff path) must land on
    /// *different* re-arm intents — `Immediate` for input, `AfterActive` for
    /// DpmsHandoff — so `run_daemon` can route them to the matching `IdleSource`
    /// primitive (Q4: avoid the post-DpmsHandoff race against the compositor's
    /// own idle blank).
    ///
    /// They also differ structurally: `dismiss` drops the whole `Saver`,
    /// while `dpms_handoff` keeps the surface and takes only the inhibitor
    /// so the desktop is never exposed during the compositor's blank-
    /// countdown window.
    ///
    /// `HowanApp` itself cannot be constructed without a live Wayland
    /// connection, but only the `saver` / `pending_rearm` fields participate
    /// in the dismiss/handoff logic, so a small stub mirrors that shape
    /// exactly (including a `SaverStub` whose `inhibitor` slot models the
    /// `Saver::inhibitor` field).
    #[test]
    fn dismiss_and_dpms_handoff_set_distinct_rearm_intents() {
        use super::RearmIntent;

        struct SaverStub {
            inhibitor: Option<()>,
        }
        struct StubApp {
            saver: Option<SaverStub>,
            pending_rearm: Option<RearmIntent>,
        }
        impl StubApp {
            // Mirrors `HowanApp::dismiss`: drop the whole saver.
            fn dismiss(&mut self) {
                if self.saver.take().is_some() {
                    self.pending_rearm = Some(RearmIntent::Immediate);
                }
            }
            // Mirrors `HowanApp::dpms_handoff`: keep the saver, take and
            // drop only the inhibitor.
            fn dpms_handoff(&mut self) {
                if let Some(saver) = self.saver.as_mut() {
                    let _ = saver.inhibitor.take();
                    self.pending_rearm = Some(RearmIntent::AfterActive);
                }
            }
        }

        // Input dismiss → immediate re-arm, surface gone.
        let mut app = StubApp {
            saver: Some(SaverStub {
                inhibitor: Some(()),
            }),
            pending_rearm: None,
        };
        app.dismiss();
        assert_eq!(app.pending_rearm, Some(RearmIntent::Immediate));
        assert!(app.saver.is_none());

        // DpmsHandoff → re-arm gated on the next user-active transition,
        // surface still up, inhibitor gone.
        let mut app = StubApp {
            saver: Some(SaverStub {
                inhibitor: Some(()),
            }),
            pending_rearm: None,
        };
        app.dpms_handoff();
        assert_eq!(app.pending_rearm, Some(RearmIntent::AfterActive));
        assert!(
            app.saver.is_some(),
            "dpms_handoff must NOT drop the saver surface — keeping it mapped \
             is what hides the desktop during the compositor's blank window"
        );
        assert!(
            app.saver.as_ref().unwrap().inhibitor.is_none(),
            "dpms_handoff must take and destroy the inhibitor"
        );

        // Both are idempotent — calling again after the saver is already
        // gone leaves the intent alone (avoids overwriting the right kind
        // of re-arm if a stray event fires after dismiss).
        let mut app = StubApp {
            saver: None,
            pending_rearm: Some(RearmIntent::AfterActive),
        };
        app.dismiss();
        assert_eq!(app.pending_rearm, Some(RearmIntent::AfterActive));
        app.dpms_handoff();
        assert_eq!(app.pending_rearm, Some(RearmIntent::AfterActive));
    }

    /// After `dpms_handoff` runs, an input event arriving on the still-mapped
    /// saver routes through the DpmsHandoff arm of `on_input`, which dismisses
    /// the saver (drops the surface) and overrides the pending re-arm intent
    /// from `AfterActive` (set by the handoff) to `Immediate` (the user is
    /// back, so the next idle cycle starts cleanly).
    ///
    /// `HowanApp` itself cannot be constructed in CI (no live Wayland), so a
    /// stub mirrors the relevant fields and the two methods' bodies verbatim.
    #[test]
    fn input_after_dpms_handoff_dismisses_and_overrides_to_immediate() {
        use super::RearmIntent;

        struct SaverStub {
            inhibitor: Option<()>,
            shown_at: Instant,
        }
        impl SaverStub {
            fn phase(&self, now: Instant, t_dpms: Duration) -> SaverPhase {
                let elapsed = now.saturating_duration_since(self.shown_at);
                if elapsed >= t_dpms {
                    SaverPhase::DpmsHandoff
                } else {
                    SaverPhase::Inhibiting
                }
            }
        }
        struct StubApp {
            saver: Option<SaverStub>,
            pending_rearm: Option<RearmIntent>,
            t_dpms: Duration,
        }
        impl StubApp {
            // Mirrors `HowanApp::dpms_handoff`.
            fn dpms_handoff(&mut self) {
                if let Some(saver) = self.saver.as_mut() {
                    let _ = saver.inhibitor.take();
                    self.pending_rearm = Some(RearmIntent::AfterActive);
                }
            }
            // Mirrors `HowanApp::dismiss`.
            fn dismiss(&mut self) {
                if self.saver.take().is_some() {
                    self.pending_rearm = Some(RearmIntent::Immediate);
                }
            }
            // Mirrors `HowanApp::on_input` with the same two arms in the
            // same order.
            fn on_input(&mut self, now: Instant) {
                let Some(saver) = self.saver.as_ref() else {
                    return;
                };
                match saver.phase(now, self.t_dpms) {
                    SaverPhase::Inhibiting => self.dismiss(),
                    SaverPhase::DpmsHandoff => self.dismiss(),
                }
            }
        }

        let shown_at = Instant::now();
        let t_dpms = Duration::from_secs(60);
        let mut app = StubApp {
            saver: Some(SaverStub {
                inhibitor: Some(()),
                shown_at,
            }),
            pending_rearm: None,
            t_dpms,
        };

        // Handoff at T_dpms: surface stays, inhibitor gone, AfterActive set.
        app.dpms_handoff();
        assert!(app.saver.is_some());
        assert!(app.saver.as_ref().unwrap().inhibitor.is_none());
        assert_eq!(app.pending_rearm, Some(RearmIntent::AfterActive));

        // Input at `shown_at + t_dpms` lands in the DpmsHandoff arm of
        // `on_input`. The arm dismisses → surface gone, Immediate
        // overrides AfterActive.
        app.on_input(shown_at + t_dpms);
        assert!(
            app.saver.is_none(),
            "DpmsHandoff input must dismiss the surface"
        );
        assert_eq!(
            app.pending_rearm,
            Some(RearmIntent::Immediate),
            "DpmsHandoff input must override the AfterActive intent with Immediate"
        );
    }
}
