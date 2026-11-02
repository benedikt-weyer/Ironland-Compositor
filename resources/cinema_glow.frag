//_DEFINES_

precision highp float;

uniform float alpha;
// The element's size in logical pixels (the dark output), the lit output's
// rectangle in this output's logical coordinates (x0, y0, x1, y1), how far
// the light reaches, and the lights themselves: LIGHTS_PER_SIDE per side
// (top, bottom, left, right), four to a matrix (one per column, rgb), with
// their strength already folded in. All supplied by `GlowRenderElement::draw`
// (see src/render/cinema/glow.rs, whose `Lights::at` does the same sum).
uniform vec2 size;
uniform vec4 lit;
uniform float reach;
uniform mat4 lights[SIDES_TIMES_MATRICES];

varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec2 pos = v_coords * size;
    vec3 sum = vec3(0.0);

    for (int side = 0; side < 4; side++) {
        bool horizontal = side < 2;
        float line = side == 0 ? lit.y : (side == 1 ? lit.w : (side == 2 ? lit.x : lit.z));
        float start = horizontal ? lit.x : lit.y;
        float len = horizontal ? lit.z - lit.x : lit.w - lit.y;
        float across = (horizontal ? pos.y : pos.x) - line;
        float along = horizontal ? pos.x : pos.y;

        // Every light is softened by half the distance between lights, so
        // the row blends into an even line of light instead of beads.
        float pitch = len / float(LIGHTS_PER_SIDE);
        float soft = SOFTENING * pitch;
        float across2 = across * across + soft * soft;
        if (across2 >= reach * reach) {
            continue;
        }

        for (int m = 0; m < MATRICES_PER_SIDE; m++) {
            for (int c = 0; c < 4; c++) {
                float x = start + (float(m * 4 + c) + 0.5) * pitch - along;
                float falloff = max(1.0 - sqrt(across2 + x * x) / reach, 0.0);
                sum += lights[side * MATRICES_PER_SIDE + m][c].rgb * (falloff * falloff);
            }
        }
    }

    vec4 color = vec4(clamp(sum, 0.0, 1.0), 1.0) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
