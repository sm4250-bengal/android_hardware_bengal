#version 450

layout(push_constant) uniform Parameters {
    uint width;
    uint height;
    uint transform;
} params;

layout(location = 0) out vec4 outColor;

vec2 framebufferToLogical(vec2 p) {
    switch (params.transform) {
        case 0x00000002u: return vec2(p.y, 1.0 - p.x);
        case 0x00000004u: return vec2(1.0 - p.x, 1.0 - p.y);
        case 0x00000008u: return vec2(1.0 - p.y, p.x);
        case 0x00000010u: return vec2(1.0 - p.x, p.y);
        case 0x00000020u: return vec2(1.0 - p.y, 1.0 - p.x);
        case 0x00000040u: return vec2(p.x, 1.0 - p.y);
        case 0x00000080u: return vec2(p.y, p.x);
        default: return p;
    }
}

void main() {
    vec2 framebuffer = gl_FragCoord.xy / vec2(params.width, params.height);
    vec2 p = framebufferToLogical(framebuffer);
    vec4 color = vec4(0.02, 0.03, 0.08, 1.0);

    const float corner = 0.18;
    if (p.x < corner && p.y < corner) {
        color = vec4(1.0, 0.0, 0.0, 1.0);       // top-left red
    } else if (p.x > 1.0 - corner && p.y < corner) {
        color = vec4(0.0, 1.0, 0.0, 1.0);       // top-right green
    } else if (p.x < corner && p.y > 1.0 - corner) {
        color = vec4(0.0, 0.25, 1.0, 1.0);      // bottom-left blue
    } else if (p.x > 1.0 - corner && p.y > 1.0 - corner) {
        color = vec4(1.0, 0.85, 0.0, 1.0);      // bottom-right yellow
    } else {
        vec2 grid = abs(fract(p * 10.0) - 0.5);
        if (grid.x > 0.47 || grid.y > 0.47) {
            color = vec4(0.7, 0.7, 0.75, 1.0);
        }
        if (abs(p.x - 0.5) < 0.012) {
            color = vec4(1.0, 0.0, 1.0, 1.0);
        }
        if (abs(p.y - 0.5) < 0.012) {
            color = vec4(0.0, 1.0, 1.0, 1.0);
        }
    }
    outColor = color;
}
