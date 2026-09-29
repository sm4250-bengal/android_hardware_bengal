#version 450

const vec2 positions[3] = vec2[3](
    vec2(-0.80, -0.80),
    vec2( 0.80, -0.80),
    vec2( 0.00,  0.80)
);

void main() {
    gl_Position = vec4(positions[gl_VertexIndex], 0.0, 1.0);
}
