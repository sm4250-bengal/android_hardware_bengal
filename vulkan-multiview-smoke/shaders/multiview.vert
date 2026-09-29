#version 450
#extension GL_EXT_multiview : require

layout(location = 0) out vec3 color;

void main() {
    const vec2 positions[3] = vec2[3](
        vec2(-0.95, -0.60),
        vec2(-0.35, -0.60),
        vec2(-0.65,  0.60));
    const float x_shift = gl_ViewIndex == 0 ? 0.0 : 1.30;
    gl_Position = vec4(positions[gl_VertexIndex] + vec2(x_shift, 0.0), 0.0, 1.0);
    color = gl_ViewIndex == 0 ? vec3(1.0, 0.0, 0.0) : vec3(0.0, 1.0, 0.0);
}
