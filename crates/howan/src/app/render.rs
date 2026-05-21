//! Solid-color SHM renderer.
//!
//! Paints a single ARGB8888 buffer filled with opaque black covering the whole
//! surface. `render` is event-driven (configure / output changes / each saver
//! show), not a per-frame loop, because the contents never change.
//!
//! Each `render` acquires a buffer from the `SlotPool` rather than caching and
//! re-attaching one fixed buffer. The pool reuses any slot the compositor has
//! released and only allocates another when none is free, which gives correct
//! double-buffering: if `render` is called again before the compositor releases
//! the previously attached buffer, we paint into a second slot instead of
//! re-attaching the still-active one. Re-attaching an active buffer is a
//! protocol error (`wl_buffer` "already active") — the daemon's repeated
//! show/redraw cycles surfaced exactly that when a single buffer was cached.
//!
//! Carved out from the rest of the app so the boundary is obvious when a
//! later milestone replaces this with a GPU-backed renderer.

use smithay_client_toolkit::shm::{slot::SlotPool, Shm, ShmHandler};
use wayland_client::protocol::{wl_shm, wl_surface::WlSurface};

use super::HowanApp;

pub(crate) struct Renderer {
    pool: SlotPool,
    width: u32,
    height: u32,
}

impl Renderer {
    /// Build a renderer with its own `wl_shm` pool. The pool only needs the
    /// `Shm` global at construction time; `HowanApp` retains the `Shm` so it
    /// can build a fresh renderer for each idle cycle without it being `Clone`.
    pub(crate) fn new(
        shm: &Shm,
        initial_width: u32,
        initial_height: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Pre-size the pool for one full ARGB8888 buffer at the initial dimensions.
        // The pool grows on demand when the compositor configures a larger size.
        let pool_size = (initial_width * initial_height * 4) as usize;
        let pool = SlotPool::new(pool_size, shm)?;
        Ok(Self {
            pool,
            width: initial_width,
            height: initial_height,
        })
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Adopt a new surface size. The next `render` allocates a buffer at the
    /// new dimensions from the pool.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }

    /// Paint a fresh solid-black buffer covering the entire surface, damage the
    /// surface, and attach the buffer. The caller is responsible for the
    /// `wl_surface.commit` that flushes the change to the compositor.
    ///
    /// A new buffer is taken from the pool on every call (the pool reuses a
    /// released slot when one is free), so we never re-attach a buffer the
    /// compositor still holds — which would fail with `wl_buffer` "already
    /// active".
    pub(crate) fn render(&mut self, surface: &WlSurface) {
        let width = self.width as i32;
        let height = self.height as i32;
        let stride = width * 4;

        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            {
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

        surface.damage_buffer(0, 0, width, height);
        if let Err(err) = buffer.attach_to(surface) {
            eprintln!("howan: failed to attach buffer: {err}");
        }
    }
}

impl ShmHandler for HowanApp {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
