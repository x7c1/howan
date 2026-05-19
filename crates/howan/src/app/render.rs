//! Solid-color SHM renderer.
//!
//! Allocates and caches a single ARGB8888 buffer filled with opaque black,
//! reallocating only when the surface size changes. M1 has no per-frame
//! redraw needs because the contents never change; the renderer attaches the
//! buffer on each configure callback.
//!
//! Carved out from the rest of the app so the boundary is obvious when a
//! later milestone replaces this with a GPU-backed renderer.

use smithay_client_toolkit::shm::{
    slot::{Buffer, SlotPool},
    Shm, ShmHandler,
};
use wayland_client::protocol::{wl_shm, wl_surface::WlSurface};

use super::HowanApp;

pub(crate) struct Renderer {
    shm: Shm,
    pool: SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
}

impl Renderer {
    pub(crate) fn new(
        shm: Shm,
        initial_width: u32,
        initial_height: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Pre-size the pool for one full BGRA buffer at the initial dimensions.
        // The pool grows on demand when the compositor configures a larger size.
        let pool_size = (initial_width * initial_height * 4) as usize;
        let pool = SlotPool::new(pool_size, &shm)?;
        Ok(Self {
            shm,
            pool,
            width: initial_width,
            height: initial_height,
            buffer: None,
        })
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Adopt a new surface size. Invalidates the cached buffer so the next
    /// `render` call reallocates.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.buffer = None;
        }
    }

    /// Paint a single solid-black buffer covering the entire surface, damage
    /// the surface, and attach the buffer. The caller is responsible for the
    /// `wl_surface.commit` that flushes the change to the compositor.
    pub(crate) fn render(&mut self, surface: &WlSurface) {
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

        surface.damage_buffer(0, 0, width, height);
        if let Err(err) = buffer.attach_to(surface) {
            eprintln!("howan: failed to attach buffer: {err}");
        }
    }

    pub(crate) fn shm_mut(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ShmHandler for HowanApp {
    fn shm_state(&mut self) -> &mut Shm {
        self.renderer.shm_mut()
    }
}
