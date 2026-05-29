//! Shader source selection and the GLSL (Shadertoy) frontend.
//!
//! M6 played a single bundled WGSL fragment shader; M7 adds a second input
//! language — **GLSL written to the Shadertoy convention** — so a shader
//! copy-pasted from [Shadertoy] runs in howan. This module owns:
//!
//! - **language detection by file extension** ([`ShaderLanguage::from_path`]):
//!   `.wgsl` → WGSL, `.glsl` / `.frag` → GLSL;
//! - **the Shadertoy `mainImage` wrapper** ([`wrap_shadertoy_glsl`]): Shadertoy
//!   shaders define `void mainImage(out vec4 fragColor, in vec2 fragCoord)`
//!   rather than a GLSL `main`, so howan prepends the Shadertoy uniform block +
//!   output declaration and a synthesized `main` that calls `mainImage` with the
//!   pixel coordinate (y-flipped, see below) and writes its `fragColor`;
//! - **the single-pass guard** ([`reject_multipass`]): Shadertoy multi-buffer
//!   shaders (Buffer A/B/C/D) and texture/audio channels (`iChannel0..3`) are
//!   out of scope, so a source that references a channel is rejected with a
//!   clear, typed error rather than failing deep inside naga;
//! - **parse + validate** ([`compile_glsl`]): the wrapped source is parsed by
//!   `naga::front::glsl` to a `naga::Module` and validated with
//!   `naga::valid::Validator` — the **same** naga IR + validation the WGSL path
//!   goes through, so GLSL never reaches the driver as raw text. The validated
//!   module is handed to wgpu via `wgpu::ShaderSource::Naga`.
//!
//! # y-flip
//!
//! Shadertoy's `fragCoord` origin is bottom-left; wgpu / Vulkan framebuffer
//! coordinates are top-left. The wrapper flips y when computing the coordinate
//! passed to `mainImage` (`iResolution.y - gl_FragCoord.y`), so a pasted shader
//! is not displayed upside-down. The orientation is verified in Stage 1 (see
//! `docs/guides/50-shader-player.md`).
//!
//! # Uniform layout
//!
//! The Shadertoy uniform block declared by the wrapper must match the Rust
//! `Uniforms` struct and the WGSL `Uniforms` struct field-for-field (same order,
//! same std140 padding). That single layout lives in `super::render`; the GLSL
//! block text here mirrors it and the two are kept in lockstep by the uniform
//! unit test. See `super::render::Uniforms`.
//!
//! [Shadertoy]: https://www.shadertoy.com/

use std::fmt;
use std::path::Path;

/// Which shader language a `--shader <path>` file is, chosen by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShaderLanguage {
    /// A WGSL shader (`.wgsl`) — the same language as the bundled default.
    Wgsl,
    /// A Shadertoy-convention GLSL shader (`.glsl` / `.frag`).
    Glsl,
}

impl ShaderLanguage {
    /// Detect the shader language from a file path's extension.
    ///
    /// `.wgsl` → [`ShaderLanguage::Wgsl`]; `.glsl` / `.frag` →
    /// [`ShaderLanguage::Glsl`]. The match is case-insensitive. An unknown or
    /// missing extension yields `None` so the caller can report a clear error
    /// instead of guessing.
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "wgsl" => Some(Self::Wgsl),
            "glsl" | "frag" => Some(Self::Glsl),
            _ => None,
        }
    }
}

/// A typed error from loading or compiling a GLSL shader.
///
/// Distinct from the renderer's transient surface errors: these are
/// load-time failures that the daemon reports clearly and then falls back to
/// the bundled WGSL shader (see `super::render`), rather than crashing.
#[derive(Debug)]
pub(crate) enum ShaderError {
    /// The source references a Shadertoy texture/audio channel (`iChannel0..3`),
    /// which requires multi-pass / texture inputs that M7 does not support
    /// (single-pass `mainImage` only). Carries the channel name that triggered
    /// the rejection.
    MultipassUnsupported {
        /// The first `iChannelN` reference found in the source.
        channel: String,
    },
    /// The source has no `mainImage` entry point, so it is not a Shadertoy
    /// single-pass shader howan knows how to wrap.
    MissingMainImage,
    /// naga's GLSL frontend failed to parse the (wrapped) source. Carries the
    /// human-readable, source-annotated diagnostic.
    Parse(String),
    /// The parsed module failed naga validation. Carries the diagnostic.
    Validation(String),
}

impl fmt::Display for ShaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MultipassUnsupported { channel } => write!(
                f,
                "shader references {channel}: multi-pass / texture channels \
                 (iChannel0..3) are not supported; only a single-pass mainImage is"
            ),
            Self::MissingMainImage => write!(
                f,
                "shader has no `void mainImage(out vec4 fragColor, in vec2 fragCoord)` \
                 entry point; only single-pass Shadertoy shaders are supported"
            ),
            Self::Parse(msg) => write!(f, "GLSL parse error: {msg}"),
            Self::Validation(msg) => write!(f, "shader validation error: {msg}"),
        }
    }
}

impl std::error::Error for ShaderError {}

/// The GLSL declarations howan prepends to a Shadertoy `mainImage` shader.
///
/// This is the half of the Shadertoy ABI that must match the renderer's uniform
/// buffer: a single uniform block at `set = 0, binding = 0` whose fields mirror
/// `super::render::Uniforms` in the same order and std140 padding. The trailing
/// `main` is appended separately (see [`wrap_shadertoy_glsl`]) so the y-flip
/// expression stays next to the rationale for it.
///
/// `iMouse` is always zero in howan (the saver is idle, with no pointer
/// tracking); it is declared so a pasted shader that reads it still links.
const SHADERTOY_PRELUDE: &str = "\
#version 450 core

layout(set = 0, binding = 0) uniform Uniforms {
    vec3 iResolution;
    float iTime;
    float iTimeDelta;
    int iFrame;
    vec4 iMouse;
    vec4 iDate;
};

layout(location = 0) out vec4 howan_fragColor;
";

/// The synthesized fragment `main` that adapts Shadertoy's `mainImage` to a
/// standard GLSL entry point, applying the y-flip (see the module doc).
const SHADERTOY_MAIN: &str = "\
void main() {
    vec2 howan_fragCoord = vec2(gl_FragCoord.x, iResolution.y - gl_FragCoord.y);
    mainImage(howan_fragColor, howan_fragCoord);
}
";

/// Wrap a Shadertoy `mainImage` GLSL source into a complete fragment shader.
///
/// Prepends the Shadertoy uniform block + output declaration ([`SHADERTOY_PRELUDE`])
/// and appends a synthesized `main` ([`SHADERTOY_MAIN`]) that calls the user's
/// `mainImage` with the y-flipped pixel coordinate. naga only ever sees standard
/// GLSL with a real `main`, so the Shadertoy convention is handled entirely at
/// the source level.
fn wrap_shadertoy_glsl(user_source: &str) -> String {
    format!("{SHADERTOY_PRELUDE}\n{user_source}\n{SHADERTOY_MAIN}")
}

/// Reject single-pass-only-incompatible sources before parsing.
///
/// Shadertoy multi-buffer shaders sample texture/audio channels through
/// `iChannel0..3`; howan supports only a single-pass `mainImage`, so a source
/// that references any channel is rejected here with a clear, typed error rather
/// than failing with an opaque "undeclared identifier" deep inside naga.
fn reject_multipass(user_source: &str) -> Result<(), ShaderError> {
    for n in 0..4 {
        let channel = format!("iChannel{n}");
        if user_source.contains(&channel) {
            return Err(ShaderError::MultipassUnsupported { channel });
        }
    }
    Ok(())
}

/// Compile a Shadertoy-convention GLSL source into a validated `naga::Module`.
///
/// Pipeline: reject multi-pass channels → require a `mainImage` entry point →
/// wrap into a standard fragment shader → parse with `naga::front::glsl` →
/// validate with `naga::valid::Validator`. The returned module is fed to wgpu
/// via `wgpu::ShaderSource::Naga`, so this is exactly the parse + validate path
/// the renderer uses; the CPU-only tests call it directly with no GPU or Wayland
/// connection.
///
/// The fragment entry point of the returned module is always named `main`.
pub(crate) fn compile_glsl(user_source: &str) -> Result<naga::Module, ShaderError> {
    reject_multipass(user_source)?;
    if !user_source.contains("mainImage") {
        return Err(ShaderError::MissingMainImage);
    }

    let wrapped = wrap_shadertoy_glsl(user_source);

    let mut frontend = naga::front::glsl::Frontend::default();
    let options = naga::front::glsl::Options::from(naga::ShaderStage::Fragment);
    let module = frontend
        .parse(&options, &wrapped)
        .map_err(|errors| ShaderError::Parse(errors.emit_to_string(&wrapped)))?;

    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    validator
        .validate(&module)
        .map_err(|err| ShaderError::Validation(err.emit_to_string(&wrapped)))?;

    Ok(module)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A minimal but real single-pass Shadertoy shader: writes a time-varying
    /// color, references the supported uniforms, and uses `fragCoord` /
    /// `iResolution` exactly as a pasted shader would.
    const SAMPLE_MAIN_IMAGE: &str = "\
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec3 col = 0.5 + 0.5 * cos(iTime + uv.xyx + vec3(0.0, 2.0, 4.0));
    fragColor = vec4(col, 1.0);
}
";

    #[test]
    fn extension_detects_wgsl() {
        assert_eq!(
            ShaderLanguage::from_path(Path::new("/x/foo.wgsl")),
            Some(ShaderLanguage::Wgsl)
        );
    }

    #[test]
    fn extension_detects_glsl_and_frag_case_insensitively() {
        assert_eq!(
            ShaderLanguage::from_path(Path::new("a.glsl")),
            Some(ShaderLanguage::Glsl)
        );
        assert_eq!(
            ShaderLanguage::from_path(Path::new("a.frag")),
            Some(ShaderLanguage::Glsl)
        );
        assert_eq!(
            ShaderLanguage::from_path(Path::new("a.FRAG")),
            Some(ShaderLanguage::Glsl)
        );
    }

    #[test]
    fn extension_unknown_is_none() {
        assert_eq!(ShaderLanguage::from_path(Path::new("a.txt")), None);
        assert_eq!(ShaderLanguage::from_path(Path::new("noext")), None);
    }

    /// The gate that proves GLSL is accepted: a real Shadertoy-style
    /// `mainImage` source compiles through the same parse + validate path the
    /// renderer uses, with no GPU or Wayland connection.
    #[test]
    fn shadertoy_main_image_compiles_and_validates() {
        let module = compile_glsl(SAMPLE_MAIN_IMAGE)
            .expect("a single-pass Shadertoy mainImage shader must validate");
        // The wrapper synthesizes a `main` fragment entry point.
        assert!(
            module.entry_points.iter().any(|ep| ep.name == "main"),
            "the wrapped module must expose a `main` entry point"
        );
    }

    /// A source that references a multi-pass channel (`iChannel0`) is rejected
    /// with the typed single-pass guard error, not a panic or an opaque parse
    /// failure.
    #[test]
    fn ichannel_reference_is_rejected_as_multipass() {
        let src = "\
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    fragColor = texture(iChannel0, fragCoord / iResolution.xy);
}
";
        let err = compile_glsl(src).expect_err("an iChannel0 reference must be rejected");
        match err {
            ShaderError::MultipassUnsupported { channel } => assert_eq!(channel, "iChannel0"),
            other => panic!("expected MultipassUnsupported, got {other:?}"),
        }
    }

    /// A source with no `mainImage` is rejected with a clear error rather than
    /// a confusing naga "no entry point" failure.
    #[test]
    fn missing_main_image_is_rejected() {
        let src = "vec3 f() { return vec3(1.0); }\n";
        let err = compile_glsl(src).expect_err("a source without mainImage must be rejected");
        assert!(matches!(err, ShaderError::MissingMainImage));
    }

    /// The example shader shipped for Stage 1 verification
    /// (`examples/shaders/drifting-bands.glsl`, referenced by the guide)
    /// must compile through the real path, so the documented reproduction
    /// cannot silently rot.
    #[test]
    fn shipped_example_shader_compiles() {
        let src = include_str!("../../../../examples/shaders/drifting-bands.glsl");
        compile_glsl(src).expect("the shipped example shader must validate");
    }

    /// The GLSL prelude's uniform block is byte-for-byte in lockstep with the
    /// Rust `Uniforms` / WGSL `Uniforms` std140 layout.
    ///
    /// The other GLSL tests only prove the prelude *compiles + validates*; that
    /// does not catch a padding-order desync, because naga computes std140
    /// offsets from whatever fields the prelude declares and the bind group uses
    /// `min_binding_size: None`, so a shifted offset would still validate and
    /// then render with the wrong values. This walks the compiled module's
    /// uniform struct and asserts each field's name and byte offset against the
    /// single layout documented on `super::render::Uniforms`, so a future edit
    /// to the prelude (or to that struct) that breaks the lockstep fails here
    /// rather than silently producing wrong colors on the GPU.
    #[test]
    fn glsl_prelude_uniform_layout_matches_std140() {
        use naga::TypeInner;

        // (field name, std140 byte offset) — must mirror `render::Uniforms`.
        const EXPECTED: &[(&str, u32)] = &[
            ("iResolution", 0),
            ("iTime", 12),
            ("iTimeDelta", 16),
            ("iFrame", 20),
            ("iMouse", 32),
            ("iDate", 48),
        ];

        let module = compile_glsl(SAMPLE_MAIN_IMAGE)
            .expect("the sample Shadertoy shader must validate");

        // Locate the uniform block by its fields rather than by type name: naga's
        // GLSL frontend may not preserve the block's tag, but the member names
        // (iResolution, …) are the Shadertoy ABI and are stable.
        let members = module
            .types
            .iter()
            .find_map(|(_, ty)| match &ty.inner {
                TypeInner::Struct { members, .. }
                    if members.iter().any(|m| m.name.as_deref() == Some("iResolution")) =>
                {
                    Some(members)
                }
                _ => None,
            })
            .expect("the wrapped GLSL must contain the Shadertoy uniform block");

        let actual: Vec<(String, u32)> = members
            .iter()
            .map(|m| {
                (
                    m.name.clone().unwrap_or_default(),
                    m.offset,
                )
            })
            .collect();
        let expected: Vec<(String, u32)> = EXPECTED
            .iter()
            .map(|(n, o)| ((*n).to_string(), *o))
            .collect();
        assert_eq!(
            actual, expected,
            "GLSL prelude uniform layout drifted from render::Uniforms std140 layout"
        );
    }

    /// A syntactically broken `mainImage` body surfaces as a typed parse error
    /// (readable, not a crash).
    #[test]
    fn malformed_glsl_is_a_parse_error() {
        let src = "\
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    fragColor = vec4(  // unterminated expression
}
";
        let err = compile_glsl(src).expect_err("malformed GLSL must be a parse error");
        assert!(matches!(err, ShaderError::Parse(_)));
    }
}
