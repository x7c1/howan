// A minimal single-pass shader for verifying howan's GLSL / Shadertoy-convention
// path (see docs/guides/50-shader-player.md, Stage 1). Written for howan — it is
// NOT copied from Shadertoy, but it follows the Shadertoy `mainImage` convention,
// so it also runs unchanged if pasted into shadertoy.com.
//
// Soft color bands drift slowly with `iTime` (so the live frame loop is visible
// as motion) over a vertical brightness gradient that is dark toward the bottom
// of the screen and bright toward the top — making the orientation easy to
// confirm by eye (a vertical flip would put the dark end at the top).
//
// Load it with: howan start --shader examples/shaders/drifting-bands.glsl

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    // Normalized pixel coordinates in [0, 1], Shadertoy convention
    // (origin bottom-left). howan applies the y-flip before calling mainImage,
    // so uv.y == 0 is the bottom of the screen, as on Shadertoy.
    vec2 uv = fragCoord / iResolution.xy;

    // Slowly drifting color (small iTime coefficient) over a vertical darkening
    // gradient. The gradient makes the orientation obvious: the bottom
    // (uv.y -> 0) is dark, the top is bright.
    vec3 col = 0.5 + 0.5 * cos(iTime * 0.3 + uv.xyx + vec3(0.0, 2.0, 4.0));
    // Pull the color partway toward its luminance to mute the saturation.
    col = mix(vec3(dot(col, vec3(0.2126, 0.7152, 0.0722))), col, 0.7);
    col *= uv.y;

    fragColor = vec4(col, 1.0);
}
