//! GPU-backed WGSL shader renderer.
//!
//! Replaces the earlier solid-black `wl_shm` renderer (M6). It draws a single
//! bundled WGSL fragment shader, animated over time, to the saver surface once
//! per Wayland frame callback. The shader is compiled into the binary with
//! [`include_str!`] (see [`SHADER_SOURCE`]) and built into a wgpu render
//! pipeline at runtime — nothing is read from the filesystem.
//!
//! # Two-layer split
//!
//! Creating a wgpu adapter/device is expensive and the daemon recreates the
//! saver (and thus its surface) on every idle cycle, so the durable and the
//! per-surface state are separated:
//!
//! - [`Gpu`] owns the process-lifetime wgpu objects — instance, adapter,
//!   device, queue, the compiled render pipeline, and the single uniform buffer
//!   + bind group. It is created once and held on `HowanApp` behind an `Rc`.
//! - [`Renderer`] owns the per-surface wgpu [`wgpu::Surface`] and its
//!   configuration, plus a shared handle to the durable [`Gpu`]. It is rebuilt
//!   each time the saver is shown and dropped on dismiss.
//!
//! # Uniforms
//!
//! Two Shadertoy-style uniforms drive the shader (mirroring the names so M7's
//! GLSL/Shadertoy compatibility is a small step):
//!
//! - `iTime` — seconds since the saver became visible (`Saver::shown_at`),
//!   `Duration::as_secs_f32`. It resets each idle cycle.
//! - `iResolution` — the surface size as `vec3(width, height, width / height)`.
//!   The `.z` component is the aspect ratio (width over height), used by the
//!   shader to avoid stretching the pattern on non-square surfaces.
//!
//! [`uniforms`] computes both purely from an elapsed `Duration` and a surface
//! size; it is unit-tested without a GPU or Wayland connection.
//!
//! # Composited-surface invariant
//!
//! Creating the wgpu surface from the `wl_surface` does **not** call
//! `set_fullscreen` and does **not** declare an opaque region — it only wraps
//! the existing raw `wl_display` / `wl_surface` pointers. The shader outputs
//! opaque pixels (alpha 1.0) for appearance, which is unrelated to declaring an
//! opaque *region*; only the latter governs Mutter's scanout eligibility. See
//! `docs/guides/30-composited-surface.md` and `docs/guides/50-shader-player.md`.

use std::rc::Rc;
use std::time::Duration;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use tracing::warn;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

/// The bundled WGSL shader, compiled into the binary. Never read from disk.
const SHADER_SOURCE: &str = include_str!("shader.wgsl");

/// The uniform block uploaded to the shader each frame.
///
/// The field order and padding match `struct Uniforms` in `shader.wgsl`: a
/// `vec3<f32>` is 16-byte aligned in a WGSL uniform, so `i_time` fills the
/// fourth slot rather than adding explicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Uniforms {
    /// `vec3(width, height, width / height)`.
    i_resolution: [f32; 3],
    /// Seconds since the saver became visible.
    i_time: f32,
}

/// Compute the shader uniforms for a given elapsed time and surface size.
///
/// Pure and GPU-free so it can be unit-tested: `iTime` is the elapsed seconds
/// (`Duration::as_secs_f32`) and `iResolution` is
/// `[width, height, width / height]`. A zero height degrades to a `0.0` aspect
/// rather than producing a non-finite value.
pub(crate) fn uniforms(elapsed: Duration, width: u32, height: u32) -> Uniforms {
    let w = width as f32;
    let h = height as f32;
    let aspect = if height == 0 { 0.0 } else { w / h };
    Uniforms {
        i_resolution: [w, h, aspect],
        i_time: elapsed.as_secs_f32(),
    }
}

/// Process-lifetime wgpu state shared across saver cycles.
///
/// Holds the expensive-to-create instance/adapter/device/queue, the compiled
/// render pipeline, and the single uniform buffer + bind group. The daemon
/// keeps one `Gpu` (behind an `Rc`) for the whole process and only rebuilds the
/// per-surface [`Renderer`] each idle cycle.
pub(crate) struct Gpu {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// The surface texture format the pipeline's color target was built for.
    /// Each per-surface `Renderer` configures its swapchain with the same
    /// format so the render pass output matches the pipeline.
    format: wgpu::TextureFormat,
}

impl Gpu {
    /// Create the durable wgpu objects and compile the bundled shader into a
    /// render pipeline. Expensive (adapter + device request); call once.
    ///
    /// The adapter is requested without a compatible surface — the saver
    /// surface does not exist yet at process start — so the pipeline's color
    /// target is built against a fixed, widely-supported swapchain format
    /// ([`wgpu::TextureFormat::Bgra8Unorm`]); each [`Renderer`] later configures
    /// its surface with that same format (see [`Gpu`]'s `format` field).
    pub(crate) fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok_or("no suitable wgpu adapter found")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("howan-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("howan-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });

        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("howan-uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("howan-bind-group-layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("howan-bind-group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("howan-pipeline-layout"),
                bind_group_layouts: &[&bind_group_layout],
                push_constant_ranges: &[],
            });

        // The fixed format the method doc above explains. `Bgra8Unorm` is the
        // standard Wayland WSI swapchain format, so it matches what the surface
        // reports in practice.
        let format = wgpu::TextureFormat::Bgra8Unorm;

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("howan-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            instance,
            device,
            queue,
            pipeline,
            uniform_buffer,
            bind_group,
            format,
        })
    }
}

/// The per-surface wgpu renderer, rebuilt each time the saver is shown.
///
/// Owns the wgpu [`wgpu::Surface`] wrapping the saver's `wl_surface` and the
/// current surface size, and shares the durable [`Gpu`] through an `Rc`.
pub(crate) struct Renderer {
    gpu: Rc<Gpu>,
    surface: wgpu::Surface<'static>,
    width: u32,
    height: u32,
    /// Whether the surface has been configured for the current size. The first
    /// `render` after a `resize` reconfigures the swapchain.
    configured: bool,
}

impl Renderer {
    /// Create a per-surface renderer wrapping `wl_surface`.
    ///
    /// Derives the raw Wayland display/window handles from the `Connection`'s
    /// backend (`wl_display`) and the surface's object id (`wl_surface`), then
    /// asks wgpu for a surface over them. This only wraps the existing
    /// pointers; it never calls `set_fullscreen` and never declares an opaque
    /// region (see the module doc and `Saver::new`).
    ///
    /// # Safety of the raw handles
    ///
    /// wgpu's `create_surface_unsafe` requires the display/window handles to
    /// outlive the returned surface. The `Connection` is durable on `HowanApp`
    /// (whole-process lifetime), and the `Renderer` is dropped before the
    /// `Window`/`wl_surface` it points at (field order in `Saver` puts
    /// `renderer` before `window`), so the surface never outlives its pointers.
    pub(crate) fn new(
        gpu: &Rc<Gpu>,
        conn: &Connection,
        surface: &WlSurface,
        initial_width: u32,
        initial_height: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
        let surface_ptr = surface.id().as_ptr() as *mut std::ffi::c_void;

        let display_handle = {
            let ptr = std::ptr::NonNull::new(display_ptr)
                .ok_or("wl_display pointer was null")?;
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(ptr))
        };
        let window_handle = {
            let ptr = std::ptr::NonNull::new(surface_ptr)
                .ok_or("wl_surface pointer was null")?;
            RawWindowHandle::Wayland(WaylandWindowHandle::new(ptr))
        };

        // SAFETY: see the doc comment. The handles wrap live `wl_display` /
        // `wl_surface` pointers that outlive this surface.
        let wgpu_surface = unsafe {
            gpu.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: display_handle,
                    raw_window_handle: window_handle,
                })?
        };

        Ok(Self {
            gpu: Rc::clone(gpu),
            surface: wgpu_surface,
            width: initial_width,
            height: initial_height,
            configured: false,
        })
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    /// Adopt a new surface size. The next `render` reconfigures the swapchain.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.configured = false;
    }

    /// (Re)configure the wgpu surface swapchain for the current size.
    fn configure(&mut self) {
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.gpu.format,
            width: self.width.max(1),
            height: self.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        self.surface.configure(&self.gpu.device, &config);
        self.configured = true;
    }

    /// Render one frame of the shader for the given elapsed time and present it.
    ///
    /// Uploads `iTime` / `iResolution` (see [`uniforms`]) to the uniform buffer,
    /// runs the shader over a full-screen triangle, and presents the swapchain
    /// frame. wgpu's `present()` commits the `wl_surface`, so the caller does
    /// not commit it separately.
    pub(crate) fn render(&mut self, elapsed: Duration) {
        if !self.configured {
            self.configure();
        }

        let uniforms = uniforms(elapsed, self.width, self.height);
        self.gpu
            .queue
            .write_buffer(&self.gpu.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                // The swapchain needs reconfiguring (e.g. after a resize the
                // compositor processed late). Reconfigure and skip this frame;
                // the next frame callback paints again.
                self.configure();
                return;
            }
            Err(err) => {
                warn!(error = %err, "failed to acquire wgpu surface frame");
                return;
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("howan-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("howan-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.gpu.pipeline);
            pass.set_bind_group(0, &self.gpu.bind_group, &[]);
            // Three vertices: the full-screen triangle (no vertex buffer).
            pass.draw(0..3, 0..1);
        }
        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::uniforms;

    /// `iTime` is the elapsed seconds (`as_secs_f32`) and `iResolution` is
    /// `[width, height, width / height]`. Pure computation — no GPU/Wayland.
    /// Mirrors the `make_inhibitor` / `phase_of` unit-test pattern in `app.rs`.
    #[test]
    fn uniforms_map_elapsed_and_size() {
        let u = uniforms(Duration::from_millis(2500), 1920, 1080);
        assert_eq!(u.i_time, 2.5);
        assert_eq!(u.i_resolution[0], 1920.0);
        assert_eq!(u.i_resolution[1], 1080.0);
        assert!((u.i_resolution[2] - 1920.0 / 1080.0).abs() < f32::EPSILON);
    }

    /// A square surface has aspect 1.0, and zero elapsed maps to iTime 0.
    #[test]
    fn uniforms_square_surface_and_zero_time() {
        let u = uniforms(Duration::ZERO, 512, 512);
        assert_eq!(u.i_time, 0.0);
        assert_eq!(u.i_resolution, [512.0, 512.0, 1.0]);
    }

    /// A zero height degrades to a `0.0` aspect rather than a non-finite value,
    /// so a stray pre-configure size never feeds NaN/inf to the shader.
    #[test]
    fn uniforms_zero_height_is_finite() {
        let u = uniforms(Duration::from_secs(1), 800, 0);
        assert_eq!(u.i_resolution[2], 0.0);
        assert!(u.i_resolution[2].is_finite());
    }
}
