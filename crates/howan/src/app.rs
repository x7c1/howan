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
//!   surface, shows the saver when the idle source fires, and on input drops
//!   the *surface* (not the process), re-arming for the next idle cycle. See
//!   `docs/guides/40-resident-daemon.md`.
//!
//! In both cases input dismisses the saver; the difference is only what happens
//! afterwards (process exit vs. stay resident), which is decided by the loop in
//! the entry point, not by the surface code.
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
//! The module is split into three files so the boundaries that future
//! milestones will cross are explicit:
//!
//! - `app.rs` (this file) holds `run`, `run_daemon`, and the top-level
//!   `HowanApp` / `Saver` state.
//! - `app::render` owns surface drawing and the `wl_shm` buffer pool. A
//!   later milestone is expected to swap this out for a GPU-backed
//!   renderer; isolating it here means that change is local.
//! - `app::handlers` contains every Wayland-protocol handler trait impl
//!   plus the `delegate_*!` macros.

mod handlers;
mod render;

use std::error::Error;
use std::time::Duration;

use calloop::channel::{channel, Event as ChannelEvent};
use calloop::signals::{Signal, Signals};
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

    let mut app = HowanApp::new(&globals, &qh)?;
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
/// that the seat has been idle for `T1`. On the first input the saver surface
/// is destroyed and the daemon re-arms the idle source, staying resident for
/// the next cycle. `SIGTERM`/`SIGINT` terminate the whole daemon cleanly.
///
/// The loop consumes idle events through the [`IdleSource`] trait, so a future
/// backend (e.g. `ext-idle-notify-v1` on wlroots) can be dropped in by writing
/// a new `IdleSource` implementation without touching this function.
pub fn run_daemon(idle_source: Box<dyn IdleSource>) -> Result<(), Box<dyn Error>> {
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

    let mut app = HowanApp::new(&globals, &qh)?;

    while !app.should_quit() {
        event_loop.dispatch(DISPATCH_TIMEOUT, &mut app)?;

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
    /// Set by the `SIGTERM`/`SIGINT` handler to terminate the whole process.
    /// Input dismiss does **not** set this — it only drops `saver`.
    exit: bool,
    /// Set when input has just dismissed the saver and the daemon loop should
    /// re-arm its idle source. Cleared by [`take_pending_rearm`].
    ///
    /// [`take_pending_rearm`]: HowanApp::take_pending_rearm
    pending_rearm: bool,
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
}

impl HowanApp {
    /// Bind the durable Wayland globals. No surface is created here; call
    /// [`show_saver`](HowanApp::show_saver) to put the saver on screen.
    fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<HowanApp>,
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

    /// Dismiss the saver in response to user input.
    ///
    /// This tears down only the *surface* — it drops `saver` and forgets the
    /// active output, and flags the daemon loop to re-arm. It does **not** set
    /// the process-exit flag: in the daemon, input means "stay resident". The
    /// one-shot `run` loop notices `saver` is `None` and exits on its own.
    ///
    /// Idempotent: repeated input events after the first dismiss do nothing.
    pub(crate) fn dismiss(&mut self) {
        if self.saver.take().is_some() {
            self.active_output = None;
            self.pending_rearm = true;
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
        })
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
    use super::make_inhibitor;

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
}
