//! M1 fullscreen Wayland window that exits on the first input event.
//!
//! The application binds the minimum set of Wayland globals required for a
//! fullscreen xdg-shell window, paints a single solid-black `wl_shm` buffer,
//! and toggles an exit flag the first time the user produces any keyboard,
//! pointer, or touch input.
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
    protocol::{wl_keyboard::WlKeyboard, wl_pointer::WlPointer, wl_touch::WlTouch},
    Connection,
};

use self::render::Renderer;

/// Reasonable initial size used for the very first allocation. The compositor
/// is expected to resize the window via `configure` to the output dimensions
/// after `set_fullscreen`, but we still need a non-zero starting size for the
/// shm pool.
const INITIAL_WIDTH: u32 = 1280;
const INITIAL_HEIGHT: u32 = 720;

/// Maximum time spent in a single event-loop dispatch. Kept short so that the
/// exit flag is observed quickly after an input event is processed.
const DISPATCH_TIMEOUT: Duration = Duration::from_millis(16);

/// Run the M1 application. Blocks until the user dismisses the window.
pub fn run() -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<HowanApp> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .map_err(|err| format!("failed to insert wayland source into event loop: {err}"))?;

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|err| format!("wl_compositor not available: {err}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh)
        .map_err(|err| format!("xdg_wm_base not available: {err}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|err| format!("wl_shm not available: {err}"))?;

    let surface = compositor.create_surface(&qh);
    // The window is fullscreen and acts as a passive overlay, so chrome
    // would be visible noise. We request server-side decorations because
    // compositors strip decorations on fullscreen surfaces in practice, and
    // `RequestServer` avoids pulling in client-side decoration drawing code.
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("howan");
    window.set_app_id("io.github.x7c1.howan");
    // Passing `None` lets the compositor pick the active output, which matches
    // the M1 "active output only" requirement.
    window.set_fullscreen(None);
    // Initial commit with no buffer is required so the compositor will send a
    // configure event with the chosen size.
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
    pub(crate) exit: bool,
}

impl HowanApp {
    /// Paint the current surface contents and commit the window.
    pub(crate) fn draw(&mut self) {
        self.renderer.render(self.window.wl_surface());
        self.window.commit();
    }

    /// Mark the app for exit. Idempotent: repeated input events after the
    /// first dismiss do nothing extra.
    pub(crate) fn dismiss(&mut self) {
        self.exit = true;
    }
}
