#version 450
#extension GL_EXT_multiview : require

layout(location = 0) in vec3 color;
layout(location = 0) out vec4 out_color;

void main() {
    const vec3 view_color = gl_ViewIndex == 0 ? vec3(1.0, 0.0, 0.0)
                                               : vec3(0.0, 1.0, 0.0);
    out_color = vec4(mix(color, view_color, 0.5), 1.0);
}
