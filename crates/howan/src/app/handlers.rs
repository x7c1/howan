//! Wayland protocol handler implementations and `delegate_*` macros.
//!
//! Each handler trait that SCTK requires is implemented here on `HowanApp`,
//! together with the `delegate_*!` macros that generate the matching
//! `Dispatch<_, _>` impls. Grouping the protocol glue in one file keeps
//! the higher-level concerns (top-level lifecycle in `super`, rendering in
//! `super::render`) free of dispatcher boilerplate.

use smithay_client_toolkit::{
    compositor::CompositorHandler,
    delegate_compositor, delegate_output, delegate_pointer, delegate_registry, delegate_seat,
    delegate_shm, delegate_touch, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        touch::TouchHandler,
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::window::{Window, WindowConfigure, WindowHandler},
        WaylandSurface,
    },
};
use wayland_client::{
    protocol::{
        wl_keyboard::{self, WlKeyboard},
        wl_output,
        wl_pointer::WlPointer,
        wl_seat::WlSeat,
        wl_surface::WlSurface,
        wl_touch::WlTouch,
    },
    Connection, Dispatch, QueueHandle,
};

use super::HowanApp;

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
        // Adopt the compositor-suggested size; fall back to the renderer's
        // current size only when the compositor leaves a dimension unset
        // (`None`, encoded as `0` on the wire and meaning "client decides").
        // For a fullscreen surface the compositor almost always supplies the
        // output size.
        let new_width = configure
            .new_size
            .0
            .map(|v| v.get())
            .unwrap_or_else(|| self.renderer.width());
        let new_height = configure
            .new_size
            .1
            .map(|v| v.get())
            .unwrap_or_else(|| self.renderer.height());
        self.renderer.resize(new_width, new_height);
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
        // Any pointer motion or button press dismisses the window — this
        // matches the traditional screensaver UX where the user expects mere
        // mouse movement to wake the screen. On Enter we hide the cursor by
        // attaching a null surface to the pointer image; Wayland leaves the
        // compositor's default cursor visible otherwise, which is distracting
        // on a blank overlay. Leave / axis events are ignored.
        for event in events {
            if event.surface != *self.window.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    pointer.set_cursor(serial, None, 0, 0);
                }
                PointerEventKind::Motion { .. } | PointerEventKind::Press { .. } => {
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
