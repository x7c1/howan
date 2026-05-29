// Bundled fragment shader for the howan saver (M6).
//
// Compiled into the binary with `include_str!` (see `render.rs`) and built into
// a wgpu render pipeline at runtime. It is driven by the Shadertoy-style uniform
// set, which mirrors the well-known Shadertoy names so GLSL/Shadertoy
// compatibility (M7) shares one uniform buffer:
//
//   iResolution surface size as vec3(width, height, width / height)
//   iTime       seconds since the saver became visible (resets each cycle)
//   iTimeDelta  seconds since the previous frame
//   iFrame      frame counter since the saver became visible
//   iMouse      pointer state — ALWAYS zero in howan (idle, no pointer tracking)
//   iDate       wall clock as (year, month, day, seconds-in-day)
//
// The vertex stage emits a single full-screen triangle (three vertices, no
// vertex buffer) so the fragment stage runs once per surface pixel. The
// animation is a slow plasma field: it must visibly move over time so Stage 1
// verification can tell a live frame loop from a static frame.
//
// Output alpha is 1.0 (opaque pixels) purely for appearance. That is separate
// from declaring an opaque *region* on the wl_surface — howan never does the
// latter, which is what keeps the surface off Mutter's scanout fast path. See
// docs/guides/30-composited-surface.md.

// The field order and std140 padding MUST match `struct Uniforms` in `render.rs`
// and the Shadertoy GLSL uniform block in `shader.rs` (the GLSL prelude). A
// `vec3<f32>` and a `vec4<f32>` are both 16-byte aligned in a WGSL uniform:
// `i_time` fills the slot after the `vec3`, and `_pad` carries `iMouse` to the
// next 16-byte boundary. See render.rs's `Uniforms` doc for the byte offsets.
struct Uniforms {
    i_resolution: vec3<f32>,
    i_time: f32,
    i_time_delta: f32,
    i_frame: i32,
    // Two floats of padding so the following vec4 starts on a 16-byte boundary.
    _pad: vec2<f32>,
    i_mouse: vec4<f32>,
    i_date: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> u: Uniforms;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    // Oversized triangle covering the clip-space viewport [-1, 1]^2:
    //   index 0 -> (-1, -1), index 1 -> (3, -1), index 2 -> (-1, 3)
    let x = f32(i32(vertex_index) / 2) * 4.0 - 1.0;
    let y = f32(i32(vertex_index) & 1) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    // Normalize to [0, 1] then aspect-correct so the pattern is not stretched on
    // non-square surfaces. iResolution.z carries width / height.
    let uv = frag_coord.xy / u.i_resolution.xy;
    let p = vec2<f32>((uv.x - 0.5) * u.i_resolution.z, uv.y - 0.5);

    let t = u.i_time;
    // Sum of moving sine waves -> a smoothly shifting plasma. The time terms are
    // what make the frame move; a static frame would show a fixed pattern.
    var v = sin(p.x * 6.0 + t);
    v += sin((p.y * 6.0 + t) * 0.7);
    v += sin((p.x * 4.0 + p.y * 4.0 + t) * 1.3);
    let r = length(p) * 5.0;
    v += sin(r - t * 1.5);

    // Map the plasma field to a calm, low-brightness palette so the saver is
    // easy on the eyes: a smooth drift between a deep blue base and a soft
    // slate-teal, rather than a full-saturation rainbow. The motion still comes
    // from the time terms in `v`; only the color mapping is muted.
    let m = 0.5 + 0.5 * sin(v);
    let low = vec3<f32>(0.03, 0.05, 0.09);
    let high = vec3<f32>(0.16, 0.22, 0.30);
    var c = mix(low, high, m);
    // A gentle hue drift shows a little more color (blue <-> teal <-> violet)
    // without turning the field loud; the amplitude is kept small on purpose.
    c += 0.06 * cos(vec3<f32>(0.0, 2.094, 4.188) + v * 0.5 + t * 0.2);
    c = max(c, vec3<f32>(0.0));
    return vec4<f32>(c, 1.0);
}
