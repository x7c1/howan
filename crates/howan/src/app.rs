//! The composited Wayland saver window (`howan start`) — covers the active
//! output and exits on the first input event.
//!
//! The application binds the minimum set of Wayland globals required for an
//! output-sized xdg-shell window, paints a single solid-black `wl_shm` buffer,
//! and toggles an exit flag the first time the user produces any keyboard,
//! pointer, or touch input. The same exit flag is also set by a `SIGTERM` /
//! `SIGINT` handler (how `howan stop` asks the saver to quit; see
//! `docs/guides/20-swayidle.md`), so every shutdown path is identical.
//!
//! # Why no `set_fullscreen`
//!
//! The window is sized to the active output's current mode but is an ordinary
//! composited `xdg_toplevel` — it deliberately does **not** call
//! `xdg_toplevel.set_fullscreen`, and it never declares an opaque region on its
//! surface. Both choices keep the surface off Mutter's unredirect /
//! direct-scanout fast path, which performs a KMS modeset that wedges the
//! display engine / GSP firmware on NVIDIA Blackwell (RTX 50-series) GPUs. See
//! `docs/guides/30-composited-surface.md` for the full rationale and the
//! manual safe-hardware verification procedure.
//!
//! SCTK is used for the compositor / xdg-shell / shm / seat / pointer / touch
//! glue, but `wl_keyboard` is bound directly through `wayland-client` so we
//! can avoid pulling in libxkbcommon at build time. M1 only needs to know
//! that some key was pressed; full keymap interpretation is unnecessary.
//!
//! The module is split into three files so the boundaries that future
//! milestones will cross are explicit:
//!
//! - `app.rs` (this file) holds `run` and the top-level `HowanApp` state.
//! - `app::render` owns surface drawing and the `wl_shm` buffer pool. A
//!   later milestone is expected to swap this out for a GPU-backed
//!   renderer; isolating it here means that change is local.
//! - `app::handlers` contains every Wayland-protocol handler trait impl
//!   plus the `delegate_*!` macros.

mod handlers;
mod render;

use std::error::Error;
use std::time::Duration;

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
    Connection,
};

use self::render::Renderer;
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
/// arrives.
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

    // SIGTERM is how `howan stop` (the swayidle `resume` hook) asks us to quit;
    // SIGINT covers Ctrl-C in an interactive run. Both are routed through the
    // event loop via a signalfd and set the same `exit` flag the input handlers
    // toggle, so shutdown unwinds the existing clean-exit path (input handles
    // released, PID file removed) instead of aborting mid-frame.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])
        .map_err(|err| format!("failed to register signal handler: {err}"))?;
    loop_handle
        .insert_source(signals, |_event, _metadata, app: &mut HowanApp| {
            app.dismiss();
        })
        .map_err(|err| format!("failed to insert signal source into event loop: {err}"))?;

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|err| format!("wl_compositor not available: {err}"))?;
    let xdg_shell =
        XdgShell::bind(&globals, &qh).map_err(|err| format!("xdg_wm_base not available: {err}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|err| format!("wl_shm not available: {err}"))?;

    let surface = compositor.create_surface(&qh);
    // The saver acts as a passive overlay, so chrome would be visible noise. We
    // request server-side decorations so the client never has to draw CSD; on a
    // composited (non-fullscreen) toplevel some compositors may still add a
    // titlebar, which is tracked as part of the unresolved top-most/coverage
    // question (see docs/guides/30-composited-surface.md).
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("howan");
    window.set_app_id("io.github.x7c1.howan");
    //
    // IMPORTANT — DO NOT call `window.set_fullscreen(...)` and DO NOT declare an
    // opaque region on this surface (no `wl_surface.set_opaque_region`).
    //
    // Mutter only elects *opaque* surfaces, or the transparent surface of a
    // *fullscreen* window, for its unredirect / direct-scanout optimization,
    // which performs a KMS plane/mode reconfiguration when the surface maps.
    // That modeset wedges the display engine / GSP firmware on NVIDIA Blackwell
    // (RTX 50-series) GPUs and requires a hard reset. A surface that is neither
    // fullscreen nor opaque stays on the normal composited path, so no risky
    // modeset happens.
    //
    // The shm buffer is still filled with opaque-black pixels (alpha 0xFF) for
    // appearance — that is a separate thing from declaring an opaque *region*
    // on the surface, and only the latter governs Mutter's scanout eligibility.
    // Leaving the surface non-opaque is the deliberate safety choice; do not
    // "optimize" it back by adding an opaque region. See
    // docs/guides/30-composited-surface.md for the full rationale.
    //
    // TEMPORARY WORKAROUND — this is not the ideal design. The ideal design uses
    // `set_fullscreen` (or a `wlr-layer-shell` overlay) because it *guarantees*
    // full screen coverage and top-most stacking, which this composited path
    // cannot guarantee. We accept that limitation only to dodge the Blackwell
    // modeset crash above. Restore the `set_fullscreen`-based design once BOTH:
    //   (1) an upstream NVIDIA driver / GSP-firmware release fixes the Blackwell
    //       modeset crash, AND
    //   (2) the SSH-guarded Blackwell run (Stage 2 in the guide) re-confirms a
    //       fullscreen surface no longer wedges the GPU.
    // See the "Restoration path" section of docs/guides/30-composited-surface.md.
    //
    // The window is instead sized to the active output's current mode once the
    // output geometry is known (see `HowanApp::resize_to_active_output`).
    //
    // Initial commit with no buffer is required so the compositor will send a
    // configure event.
    window.commit();

    let renderer = Renderer::new(shm, INITIAL_WIDTH, INITIAL_HEIGHT)?;

    let mut app = HowanApp {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        window,
        renderer,
        keyboard: None,
        pointer: None,
        touch: None,
        active_output: None,
        configured: false,
        exit: false,
    };

    while !app.exit {
        event_loop.dispatch(DISPATCH_TIMEOUT, &mut app)?;
    }

    // Release input handles explicitly so the compositor does not see a
    // lingering client during shutdown.
    if let Some(kbd) = app.keyboard.take() {
        kbd.release();
    }
    if let Some(ptr) = app.pointer.take() {
        ptr.release();
    }
    if let Some(touch) = app.touch.take() {
        touch.release();
    }

    Ok(())
}

pub(crate) struct HowanApp {
    pub(crate) registry_state: RegistryState,
    pub(crate) seat_state: SeatState,
    pub(crate) output_state: OutputState,
    pub(crate) window: Window,
    pub(crate) renderer: Renderer,
    pub(crate) keyboard: Option<WlKeyboard>,
    pub(crate) pointer: Option<WlPointer>,
    pub(crate) touch: Option<WlTouch>,
    /// The output the saver surface is shown on. We track the output the
    /// surface entered (M1 "active output only"); until a surface-enter event
    /// arrives we fall back to the first advertised output.
    pub(crate) active_output: Option<WlOutput>,
    /// Set once the xdg surface has received its first configure. We must not
    /// attach a buffer and commit before that (xdg-shell forbids committing a
    /// buffer to a surface that has never been configured); output/seat events
    /// can otherwise trigger `draw()` too early. Strict compositors (e.g.
    /// `weston`) reject the premature commit; Mutter tolerates it.
    pub(crate) configured: bool,
    pub(crate) exit: bool,
}

impl HowanApp {
    /// Paint the current surface contents and commit the window.
    pub(crate) fn draw(&mut self) {
        self.renderer.render(self.window.wl_surface());
        self.window.commit();
    }

    /// Resize the surface to cover the active output's current mode.
    ///
    /// This is how the saver covers the screen now that it no longer calls
    /// `set_fullscreen`: as an ordinary composited toplevel we ask for a window
    /// the size of the output (see `active_output_size`). If output info is not
    /// yet available we keep the existing allocation and rely on a later output
    /// / configure event to trigger this resize, rather than blocking startup.
    ///
    /// Returns `true` when a new size was applied so the caller can repaint.
    pub(crate) fn resize_to_active_output(&mut self) -> bool {
        let Some((width, height)) = self.active_output_size() else {
            return false;
        };
        if width == self.renderer.width() && height == self.renderer.height() {
            return false;
        }
        // Pin the toplevel to the output size so the compositor does not offer a
        // smaller interactive size. We are not fullscreen, so without this the
        // server is free to pick an arbitrary size.
        self.window.set_min_size(Some((width, height)));
        self.window.set_max_size(Some((width, height)));
        self.renderer.resize(width, height);
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

    /// Mark the app for exit. Idempotent: repeated input events after the
    /// first dismiss do nothing extra.
    pub(crate) fn dismiss(&mut self) {
        self.exit = true;
    }
}
