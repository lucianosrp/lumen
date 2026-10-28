struct Globals {
    view: vec4f,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var img: texture_2d<f32>;

struct Quad {
    @location(0) rect: vec4f,
    @location(1) color: vec4f,
    @location(2) uv: vec4f,
    @location(3) params: vec4f, // radius, has image
};

struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) local: vec2f,
    @location(1) @interpolate(flat) half: vec2f,
    @location(2) color: vec4f,
    @location(3) uv: vec2f,
    @location(4) @interpolate(flat) params: vec4f,
};

@vertex
fn vs_ui(@builtin(vertex_index) vi: u32, q: Quad) -> VOut {
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let px = q.rect.xy + c * q.rect.zw;
    var out: VOut;
    out.pos = vec4f(px.x / g.view.x * 2.0 - 1.0, 1.0 - px.y / g.view.y * 2.0, 0.0, 1.0);
    out.half = q.rect.zw * 0.5;
    out.local = (c - 0.5) * q.rect.zw;
    out.color = q.color;
    out.uv = mix(q.uv.xy, q.uv.zw, c);
    out.params = q.params;
    return out;
}

@fragment
fn fs_ui(v: VOut) -> @location(0) vec4f {
    let r = min(v.params.x, min(v.half.x, v.half.y));
    let d = length(max(abs(v.local) - v.half + r, vec2f(0.0))) - r;
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    var c = v.color;
    if v.params.y > 0.5 {
        c = c * textureSample(img, samp, v.uv);
    }
    return vec4f(c.rgb, c.a * coverage);
}
