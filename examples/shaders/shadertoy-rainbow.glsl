// A minimal single-pass Shadertoy shader for verifying howan's GLSL/Shadertoy
// path (see docs/guides/50-shader-player.md, Stage 1).
//
// It is a real Shadertoy `mainImage` shader: paste it into shadertoy.com and it
// runs unchanged there too. The rainbow drifts with `iTime` (so the live frame
// loop is visible as motion) over a vertical brightness gradient that is dark
// toward the bottom of the screen and bright toward the top (so the orientation
// is easy to confirm by eye — a vertical flip would put the dark end at the
// top).
//
// Load it with: howan start --shader examples/shaders/shadertoy-rainbow.glsl

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    // Normalized pixel coordinates in [0, 1], Shadertoy convention
    // (origin bottom-left). howan applies the y-flip before calling mainImage,
    // so uv.y == 0 is the bottom of the screen, as on Shadertoy.
    vec2 uv = fragCoord / iResolution.xy;

    // A drifting rainbow plus a vertical darkening gradient. The gradient makes
    // the orientation obvious: the bottom (uv.y -> 0) is dark, the top is bright.
    vec3 col = 0.5 + 0.5 * cos(iTime + uv.xyx + vec3(0.0, 2.0, 4.0));
    col *= uv.y;

    fragColor = vec4(col, 1.0);
}
