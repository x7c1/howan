//! GPU-backed fragment-shader renderer.
//!
//! Replaces the earlier solid-black `wl_shm` renderer (M6). It draws a fragment
//! shader, animated over time, to the saver surface once per Wayland frame
//! callback. By default that is a bundled WGSL shader compiled into the binary
//! with [`include_str!`] (see [`SHADER_SOURCE`]) and built into a wgpu render
//! pipeline at runtime — nothing is read from the filesystem. With `--shader`
//! the fragment stage instead comes from a WGSL or GLSL/Shadertoy file (see
//! [`ShaderInput`] and `super::shader`).
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
//! The Shadertoy-style uniform set drives the shader (mirroring the names so the
//! GLSL/Shadertoy path in `super::shader` shares one uniform buffer):
//!
//! - `iResolution` — the surface size as `vec3(width, height, width / height)`.
//!   The `.z` component is the aspect ratio (width over height), used by the
//!   shader to avoid stretching the pattern on non-square surfaces.
//! - `iTime` — seconds since the saver became visible (`Saver::shown_at`),
//!   `Duration::as_secs_f32`. It resets each idle cycle.
//! - `iTimeDelta` — seconds since the previous frame.
//! - `iFrame` — frame counter (i32) since the saver became visible.
//! - `iMouse` — pointer state, **always zero**: the saver is idle and does not
//!   track the pointer, but the uniform is provided so a pasted Shadertoy shader
//!   that reads it still links.
//! - `iDate` — the wall clock as `(year, month, day, seconds-in-day)`.
//!
//! [`uniforms`] computes these purely from the timing inputs and a surface size;
//! it is unit-tested without a GPU or Wayland connection. `iDate` reads the wall
//! clock at frame time and so is not part of the pure computation's assertions.
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

use std::path::PathBuf;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use tracing::warn;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

use super::shader::{compile_glsl, ShaderLanguage};

/// The bundled WGSL shader, compiled into the binary. Never read from disk.
const SHADER_SOURCE: &str = include_str!("shader.wgsl");

/// Which shader the renderer should compile into its pipeline.
///
/// Without `--shader` the daemon uses [`ShaderInput::BundledWgsl`] — the
/// compiled-in default, behaving exactly as M6 did. `--shader <path>` selects
/// [`ShaderInput::File`], whose language is chosen by extension when the file is
/// loaded (see [`Gpu::new`] and `super::shader`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShaderInput {
    /// The bundled WGSL shader compiled into the binary (the default).
    BundledWgsl,
    /// A shader loaded from an explicit filesystem path (`--shader <path>`).
    File(PathBuf),
}

/// Build the wgpu shader source for a chosen [`ShaderInput`].
///
/// The bundled default and a `.wgsl` file both go through
/// `wgpu::ShaderSource::Wgsl`; a `.glsl` / `.frag` file is parsed + validated by
/// `super::shader::compile_glsl` and handed over as `wgpu::ShaderSource::Naga`,
/// the same naga IR the WGSL path produces internally. A read/parse/validate
/// failure returns an error so the caller can log it and fall back to the
/// bundled shader rather than crashing the daemon.
///
/// Returns the source plus the fragment entry-point name, which differs between
/// the bundled WGSL (`fs_main`) and the synthesized GLSL `main`.
fn shader_source_for(
    input: &ShaderInput,
) -> Result<(wgpu::ShaderSource<'static>, &'static str), Box<dyn std::error::Error>> {
    match input {
        ShaderInput::BundledWgsl => {
            Ok((wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()), "fs_main"))
        }
        ShaderInput::File(path) => {
            let language = ShaderLanguage::from_path(path).ok_or_else(|| {
                format!(
                    "unrecognized shader extension for {}: expected .wgsl, .glsl, or .frag",
                    path.display()
                )
            })?;
            let text = std::fs::read_to_string(path)
                .map_err(|err| format!("failed to read shader {}: {err}", path.display()))?;
            match language {
                ShaderLanguage::Wgsl => {
                    Ok((wgpu::ShaderSource::Wgsl(text.into()), "fs_main"))
                }
                ShaderLanguage::Glsl => {
                    let module = compile_glsl(&text)?;
                    Ok((wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)), "main"))
                }
            }
        }
    }
}

/// The uniform block uploaded to the shader each frame.
///
/// The field order and padding match `struct Uniforms` in `shader.wgsl` and the
/// Shadertoy GLSL uniform block in `super::shader`. Both a `vec3<f32>` and a
/// `vec4<f32>` are 16-byte aligned in a WGSL/std140 uniform, so the byte offsets
/// are:
///
/// | offset | field         | type                  |
/// |--------|---------------|-----------------------|
/// | 0      | `i_resolution`| `vec3<f32>`           |
/// | 12     | `i_time`      | `f32`                 |
/// | 16     | `i_time_delta`| `f32`                 |
/// | 20     | `i_frame`     | `i32`                 |
/// | 24     | `_pad`        | `[f32; 2]` (padding)  |
/// | 32     | `i_mouse`     | `vec4<f32>`           |
/// | 48     | `i_date`      | `vec4<f32>`           |
/// | 64     | (end)         |                       |
///
/// `_pad` carries `i_mouse` to the next 16-byte boundary. Adding or reordering a
/// field changes this layout and must be mirrored in both shader-side structs.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Uniforms {
    /// `vec3(width, height, width / height)`.
    i_resolution: [f32; 3],
    /// Seconds since the saver became visible.
    i_time: f32,
    /// Seconds since the previous frame.
    i_time_delta: f32,
    /// Frame counter since the saver became visible.
    i_frame: i32,
    /// Padding so `i_mouse` starts on a 16-byte boundary (std140).
    _pad: [f32; 2],
    /// Pointer state. Always zero in howan (idle, no pointer tracking).
    i_mouse: [f32; 4],
    /// Wall clock as `(year, month, day, seconds-in-day)`.
    i_date: [f32; 4],
}

/// Compute the shader uniforms for the given timing inputs and surface size.
///
/// Pure and GPU-free so it can be unit-tested: `iTime` is the elapsed seconds
/// (`Duration::as_secs_f32`), `iTimeDelta` is the seconds since the previous
/// frame, `iFrame` is the frame index, and `iResolution` is
/// `[width, height, width / height]`. A zero height degrades to a `0.0` aspect
/// rather than producing a non-finite value. `iMouse` is always zero. `iDate`
/// is supplied separately by the caller because it reads the wall clock, which
/// is not a pure function of the inputs.
pub(crate) fn uniforms(
    elapsed: Duration,
    delta: Duration,
    frame: i32,
    width: u32,
    height: u32,
    date: [f32; 4],
) -> Uniforms {
    let w = width as f32;
    let h = height as f32;
    let aspect = if height == 0 { 0.0 } else { w / h };
    Uniforms {
        i_resolution: [w, h, aspect],
        i_time: elapsed.as_secs_f32(),
        i_time_delta: delta.as_secs_f32(),
        i_frame: frame,
        _pad: [0.0; 2],
        i_mouse: [0.0; 4],
        i_date: date,
    }
}

/// Compute the Shadertoy `iDate` vec4 from the local wall clock: `(year, month,
/// day, seconds-since-midnight)`.
///
/// Uses `chrono`-free arithmetic over the system clock so no extra dependency is
/// pulled in: the date components come from a small civil-from-days conversion
/// and the time-of-day from the seconds remainder. On a clock-read failure
/// (system time before the Unix epoch) it degrades to all-zero rather than
/// panicking.
pub(crate) fn i_date_now() -> [f32; 4] {
    use std::time::{SystemTime, UNIX_EPOCH};

    let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return [0.0; 4];
    };
    let total_secs = since_epoch.as_secs();
    let secs_in_day = (total_secs % 86_400) as f32;
    let days = (total_secs / 86_400) as i64;

    // Civil-from-days algorithm (Howard Hinnant's `civil_from_days`), giving the
    // proleptic Gregorian (year, month, day) for a count of days since
    // 1970-01-01. This avoids a date-library dependency for one vec4.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    [year as f32, month as f32, day as f32, secs_in_day]
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
    /// Create the durable wgpu objects and compile the chosen shader into a
    /// render pipeline. Expensive (adapter + device request); call once.
    ///
    /// `input` selects the shader: [`ShaderInput::BundledWgsl`] for the
    /// compiled-in default (the M6 behavior) or [`ShaderInput::File`] for a
    /// `--shader <path>` file, whose language is detected by extension. A GLSL
    /// (Shadertoy) file is parsed + validated by `super::shader::compile_glsl`
    /// before reaching the pipeline. A load/parse/validate failure is returned
    /// as an error so the caller can fall back to the bundled shader rather than
    /// crash (see `HowanApp::new`).
    ///
    /// The adapter is requested without a compatible surface — the saver
    /// surface does not exist yet at process start — so the pipeline's color
    /// target is built against a fixed, widely-supported swapchain format
    /// ([`wgpu::TextureFormat::Bgra8Unorm`]); each [`Renderer`] later configures
    /// its surface with that same format (see [`Gpu`]'s `format` field).
    pub(crate) fn new(input: &ShaderInput) -> Result<Self, Box<dyn std::error::Error>> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            // The saver covers a whole output; on a discrete-GPU machine that is
            // the GPU we want (and the one the Blackwell composited-path design
            // targets), so prefer it over an integrated adapter.
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok_or("no suitable wgpu adapter found")?;

        // Record which adapter wgpu actually selected. This makes it obvious in
        // the journal whether the saver runs on the real GPU (`device_type:
        // DiscreteGpu`, e.g. an NVIDIA Vulkan adapter) or fell back to a
        // software rasterizer (`device_type: Cpu`, e.g. llvmpipe/lavapipe) —
        // the latter renders correctly but on the CPU, which is easy to mistake
        // for "the GPU is idle".
        let info = adapter.get_info();
        tracing::info!(
            name = %info.name,
            backend = ?info.backend,
            device_type = ?info.device_type,
            driver = %info.driver,
            "wgpu adapter selected"
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("howan-device"),
                required_features: wgpu::Features::empty(),
                // Adopt the adapter's real limits rather than a preset. The
                // saver surface is sized to the output's current mode, which on
                // a real display exceeds the downlevel preset's 2048 max texture
                // dimension (e.g. a 5120x2160 monitor) and would fail
                // `Surface::configure`. The adapter's own max is what the GPU can
                // actually scan out, so it always covers the output size.
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))?;

        // The vertex stage is always the bundled WGSL full-screen triangle
        // (`vs_main`); the fragment stage is the chosen shader. A Shadertoy GLSL
        // file supplies only `mainImage` (wrapped into a fragment `main`), with
        // no vertex stage of its own, so the vertex module is kept separate and
        // unconditional. When the chosen shader is itself bundled WGSL the two
        // modules carry the same source, which is harmless.
        let vertex_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("howan-vertex-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });
        let (fragment_source, fragment_entry) = shader_source_for(input)?;
        let fragment_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("howan-fragment-shader"),
            source: fragment_source,
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
                module: &vertex_module,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &fragment_module,
                entry_point: fragment_entry,
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
    /// Number of frames rendered for this saver cycle, exposed to the shader as
    /// `iFrame`. Starts at 0 for the first frame and increments after each
    /// `render`; resets with the renderer each idle cycle.
    frame: i32,
    /// The `elapsed` value passed to the previous `render`, used to derive
    /// `iTimeDelta` (the seconds since the last frame). `None` before the first
    /// frame, where the delta is taken as zero.
    last_elapsed: Option<Duration>,
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
            frame: 0,
            last_elapsed: None,
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
    /// Uploads the full Shadertoy uniform set (see [`uniforms`]) to the uniform
    /// buffer, runs the shader over a full-screen triangle, and presents the
    /// swapchain frame. `iTimeDelta` is derived from the difference against the
    /// previous frame's `elapsed`; `iFrame` is this renderer's frame counter;
    /// `iDate` is read from the wall clock here. wgpu's `present()` commits the
    /// `wl_surface`, so the caller does not commit it separately.
    pub(crate) fn render(&mut self, elapsed: Duration) {
        if !self.configured {
            self.configure();
        }

        let delta = match self.last_elapsed {
            Some(prev) => elapsed.saturating_sub(prev),
            None => Duration::ZERO,
        };
        let uniforms = uniforms(
            elapsed,
            delta,
            self.frame,
            self.width,
            self.height,
            i_date_now(),
        );
        self.gpu
            .queue
            .write_buffer(&self.gpu.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let surface_texture = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                // The swapchain needs reconfiguring (e.g. after a resize the
                // compositor processed late). Reconfigure and skip this frame;
                // the next frame callback paints again. Do not advance the frame
                // counter or `last_elapsed`: nothing was presented.
                self.configure();
                return;
            }
            Err(err) => {
                warn!(error = %err, "failed to acquire wgpu surface frame");
                return;
            }
        };

        let view = surface_texture
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
        surface_texture.present();

        // A frame was presented: advance the counter and record this frame's
        // elapsed time so the next frame's `iTimeDelta` is the gap to it.
        self.frame = self.frame.wrapping_add(1);
        self.last_elapsed = Some(elapsed);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{uniforms, Uniforms};

    /// A fixed `iDate` so the timing/size assertions stay pure — the wall-clock
    /// `iDate` is supplied by the caller (`i_date_now`) and is not asserted here.
    const TEST_DATE: [f32; 4] = [2026.0, 5.0, 29.0, 12345.0];

    /// `iTime` is the elapsed seconds (`as_secs_f32`) and `iResolution` is
    /// `[width, height, width / height]`. Pure computation — no GPU/Wayland.
    /// Mirrors the `make_inhibitor` / `phase_of` unit-test pattern in `app.rs`.
    #[test]
    fn uniforms_map_elapsed_and_size() {
        let u = uniforms(
            Duration::from_millis(2500),
            Duration::ZERO,
            0,
            1920,
            1080,
            TEST_DATE,
        );
        assert_eq!(u.i_time, 2.5);
        assert_eq!(u.i_resolution[0], 1920.0);
        assert_eq!(u.i_resolution[1], 1080.0);
        assert!((u.i_resolution[2] - 1920.0 / 1080.0).abs() < f32::EPSILON);
    }

    /// A square surface has aspect 1.0, and zero elapsed maps to iTime 0.
    #[test]
    fn uniforms_square_surface_and_zero_time() {
        let u = uniforms(Duration::ZERO, Duration::ZERO, 0, 512, 512, TEST_DATE);
        assert_eq!(u.i_time, 0.0);
        assert_eq!(u.i_resolution, [512.0, 512.0, 1.0]);
    }

    /// A zero height degrades to a `0.0` aspect rather than a non-finite value,
    /// so a stray pre-configure size never feeds NaN/inf to the shader.
    #[test]
    fn uniforms_zero_height_is_finite() {
        let u = uniforms(Duration::from_secs(1), Duration::ZERO, 0, 800, 0, TEST_DATE);
        assert_eq!(u.i_resolution[2], 0.0);
        assert!(u.i_resolution[2].is_finite());
    }

    /// The extended (M7) uniform set: given an elapsed time, a previous-frame
    /// delta, a frame index, and a surface size, the struct carries `iTime` /
    /// `iTimeDelta` as the documented seconds, `iFrame` as the frame index,
    /// `iResolution = [w, h, w/h]`, and `iMouse` all-zero (howan never tracks
    /// the pointer).
    #[test]
    fn uniforms_extended_set_is_computed() {
        let u = uniforms(
            Duration::from_millis(1000),
            Duration::from_millis(16),
            42,
            1280,
            720,
            TEST_DATE,
        );
        assert_eq!(u.i_time, 1.0);
        assert!((u.i_time_delta - 0.016).abs() < 1e-6);
        assert_eq!(u.i_frame, 42);
        assert_eq!(u.i_resolution[0], 1280.0);
        assert_eq!(u.i_resolution[1], 720.0);
        assert_eq!(u.i_mouse, [0.0; 4]);
        assert_eq!(u.i_date, TEST_DATE);
    }

    /// The uniform struct size matches the WGSL/std140 layout documented on
    /// [`Uniforms`]: 64 bytes (vec3+float | float+int+pad2 | vec4 | vec4). If
    /// this changes, the WGSL and GLSL shader-side structs must change with it.
    #[test]
    fn uniforms_struct_matches_std140_layout() {
        assert_eq!(std::mem::size_of::<Uniforms>(), 64);
        // 16-byte aligned so a uniform buffer binding is valid.
        assert_eq!(std::mem::align_of::<Uniforms>() % 4, 0);
    }
}
