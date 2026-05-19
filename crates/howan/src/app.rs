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

use std::error::Error;
use std::time::Duration;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_pointer, delegate_registry, delegate_seat,
    delegate_shm, delegate_touch, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{calloop::EventLoop, calloop_wayland_source::WaylandSource},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        touch::TouchHandler,
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{
        wl_keyboard::{self, WlKeyboard},
        wl_output,
        wl_pointer::WlPointer,
        wl_seat::WlSeat,
        wl_shm,
        wl_surface::WlSurface,
        wl_touch::WlTouch,
    },
    Connection, Dispatch, QueueHandle,
};

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
    let shm =
        Shm::bind(&globals, &qh).map_err(|err| format!("wl_shm not available: {err}"))?;

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

    // Pre-size the pool for one full BGRA buffer at the initial dimensions.
    // The pool grows on demand when the compositor configures a larger size.
    let pool_size = (INITIAL_WIDTH * INITIAL_HEIGHT * 4) as usize;
    let pool = SlotPool::new(pool_size, &shm)?;

    let mut app = HowanApp {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        width: INITIAL_WIDTH,
        height: INITIAL_HEIGHT,
        buffer: None,
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

struct HowanApp {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    touch: Option<WlTouch>,
    exit: bool,
}

impl HowanApp {
    /// Paint a single solid-black buffer covering the entire surface and
    /// commit it. Called on each configure; the buffer is cached and only
    /// reallocated when the surface size changes.
    fn draw(&mut self) {
        let width = self.width as i32;
        let height = self.height as i32;
        let stride = width * 4;

        // Reallocate the buffer when the dimensions change. SCTK's `Buffer`
        // exposes `height()` and `stride()` but not `width()`; we derive
        // width from stride (4 bytes per ARGB pixel).
        if let Some(buffer) = &self.buffer {
            let cached_width = buffer.stride() / 4;
            if cached_width != width || buffer.height() != height {
                self.buffer = None;
            }
        }

        let buffer = match self.buffer.as_ref() {
            Some(buffer) => buffer,
            None => {
                let (buffer, canvas) = match self.pool.create_buffer(
                    width,
                    height,
                    stride,
                    wl_shm::Format::Argb8888,
                ) {
                    Ok(pair) => pair,
                    Err(err) => {
                        eprintln!("howan: failed to allocate shm buffer: {err}");
                        return;
                    }
                };
                // ARGB8888 little-endian layout in memory is [B, G, R, A].
                // Fully opaque black is `0x00, 0x00, 0x00, 0xFF`.
                for px in canvas.chunks_exact_mut(4) {
                    px[0] = 0x00;
                    px[1] = 0x00;
                    px[2] = 0x00;
                    px[3] = 0xFF;
                }
                self.buffer = Some(buffer);
                self.buffer.as_ref().expect("buffer just assigned")
            }
        };

        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, width, height);
        if let Err(err) = buffer.attach_to(surface) {
            eprintln!("howan: failed to attach buffer: {err}");
            return;
        }
        self.window.commit();
    }

    /// Mark the app for exit. Idempotent: repeated input events after the
    /// first dismiss do nothing extra.
    fn dismiss(&mut self) {
        self.exit = true;
    }
}

impl CompositorHandler for HowanApp {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for HowanApp {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for HowanApp {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        // Treat a compositor-issued close request the same as user dismiss.
        self.dismiss();
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        // Adopt the compositor-suggested size; fall back to the initial
        // dimensions only when the compositor leaves a dimension unset (`None`,
        // encoded as `0` on the wire and meaning "client decides"). For a
        // fullscreen surface the compositor almost always supplies the output
        // size.
        let new_width = configure.new_size.0.map(|v| v.get()).unwrap_or(INITIAL_WIDTH);
        let new_height = configure.new_size.1.map(|v| v.get()).unwrap_or(INITIAL_HEIGHT);

        if new_width != self.width || new_height != self.height {
            self.width = new_width;
            self.height = new_height;
            // Invalidate the cached buffer so `draw` reallocates at the new
            // size.
            self.buffer = None;
        }

        self.draw();
    }
}

impl SeatHandler for HowanApp {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard if self.keyboard.is_none() => {
                // We bypass `SeatState::get_keyboard` here because that path
                // requires SCTK's `xkbcommon` feature, which would pull in a
                // system library we do not need for "any key dismisses".
                // Plain `wl_seat.get_keyboard` is sufficient — the matching
                // `Dispatch<WlKeyboard, ()>` impl below observes `Key` events.
                let kbd = seat.get_keyboard(qh, ());
                self.keyboard = Some(kbd);
            }
            Capability::Pointer if self.pointer.is_none() => {
                match self.seat_state.get_pointer(qh, &seat) {
                    Ok(ptr) => self.pointer = Some(ptr),
                    Err(err) => eprintln!("howan: failed to acquire pointer: {err}"),
                }
            }
            Capability::Touch if self.touch.is_none() => {
                match self.seat_state.get_touch(qh, &seat) {
                    Ok(touch) => self.touch = Some(touch),
                    Err(err) => eprintln!("howan: failed to acquire touch: {err}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(kbd) = self.keyboard.take() {
                    kbd.release();
                }
            }
            Capability::Pointer => {
                if let Some(ptr) = self.pointer.take() {
                    ptr.release();
                }
            }
            Capability::Touch => {
                if let Some(touch) = self.touch.take() {
                    touch.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl Dispatch<WlKeyboard, ()> for HowanApp {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The first `Key` event of any kind (press or release) is enough to
        // count as user input. We treat both states as dismiss triggers
        // because some compositors deliver a synthetic release for keys held
        // when focus was acquired; in either case the user has interacted.
        if let wl_keyboard::Event::Key { .. } = event {
            state.dismiss();
        }
    }
}

impl PointerHandler for HowanApp {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        // Press dismisses the window. On Enter we hide the cursor by attaching
        // a null surface to the pointer image — Wayland leaves the compositor's
        // default cursor visible otherwise, which is distracting on a blank
        // overlay. Motion / Leave / axis events are intentionally ignored so
        // that incidental mouse movement on wake does not exit prematurely.
        for event in events {
            if event.surface != *self.window.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                PointerEventKind::Press { .. } => {
                    self.dismiss();
                    break;
                }
                _ => {}
            }
        }
    }
}

impl TouchHandler for HowanApp {
    fn down(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _serial: u32,
        _time: u32,
        _surface: WlSurface,
        _id: i32,
        _position: (f64, f64),
    ) {
        // First touch-down anywhere on the surface dismisses the window.
        self.dismiss();
    }

    fn up(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _serial: u32,
        _time: u32,
        _id: i32,
    ) {
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _time: u32,
        _id: i32,
        _position: (f64, f64),
    ) {
    }

    fn shape(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _major: f64,
        _minor: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _orientation: f64,
    ) {
    }

    fn cancel(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _touch: &WlTouch) {}
}

impl ShmHandler for HowanApp {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for HowanApp {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(HowanApp);
delegate_output!(HowanApp);
delegate_shm!(HowanApp);
delegate_seat!(HowanApp);
delegate_pointer!(HowanApp);
delegate_touch!(HowanApp);
delegate_xdg_shell!(HowanApp);
delegate_xdg_window!(HowanApp);
delegate_registry!(HowanApp);
