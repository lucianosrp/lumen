struct Globals {
    view: vec4f,   // width, height, header height, scale factor
    th: vec4f,     // cell px, cells per row, atlas width, atlas height
    small: vec4f,  // slot px, slots per row, atlas size, mip count
    large: vec4f,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var th_tex: texture_2d<f32>;
@group(0) @binding(3) var small_tex: texture_2d<f32>;
@group(0) @binding(4) var large_tex: texture_2d<f32>;

struct Tile {
    @location(0) rect: vec4f,
    @location(1) th: u32,
    @location(2) slot: u32,
    @location(3) fade: f32,
};

struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) @interpolate(flat) th: u32,
    @location(2) @interpolate(flat) slot: u32,
    @location(3) fade: f32,
    @location(4) size: f32,
};

fn to_ndc(px: vec2f) -> vec4f {
    return vec4f(px.x / g.view.x * 2.0 - 1.0, 1.0 - px.y / g.view.y * 2.0, 0.0, 1.0);
}

@vertex
fn vs_tile(@builtin(vertex_index) vi: u32, t: Tile) -> VOut {
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    var out: VOut;
    out.pos = to_ndc(t.rect.xy + c * t.rect.zw);
    out.uv = c;
    out.th = t.th;
    out.slot = t.slot;
    out.fade = t.fade;
    out.size = t.rect.z;
    return out;
}

// Samples one square cell of an atlas at the mip matching its on-screen size, clamped so
// bilinear filtering never bleeds into the neighbouring cell.
fn cell_sample(tex: texture_2d<f32>, index: u32, info: vec4f, tex_size: vec2f, mips: f32, uv: vec2f, size: f32) -> vec4f {
    let cell = info.x;
    let per_row = u32(info.y);
    let origin = vec2f(f32(index % per_row), f32(index / per_row)) * cell;
    let lod = clamp(log2(cell / max(size, 0.001)), 0.0, mips - 1.0);
    let ht = 0.5 * exp2(ceil(lod));
    let p = origin + clamp(uv * cell, vec2f(ht), vec2f(cell - ht));
    return textureSampleLevel(tex, samp, p / tex_size, lod);
}

@fragment
fn fs_tile(v: VOut) -> @location(0) vec4f {
    var c = cell_sample(th_tex, v.th, g.th, g.th.zw, log2(g.th.x) + 1.0, v.uv, v.size);
    if v.slot != 0xffffffffu {
        let s = v.slot & 0x7fffffffu;
        var img: vec4f;
        if (v.slot & 0x80000000u) != 0u {
            img = cell_sample(large_tex, s, g.large, vec2f(g.large.z), g.large.w, v.uv, v.size);
        } else {
            img = cell_sample(small_tex, s, g.small, vec2f(g.small.z), g.small.w, v.uv, v.size);
        }
        c = mix(c, img, v.fade);
    }
    return vec4f(c.rgb, 1.0);
}

// Header scrim: content scrolls underneath a soft dark gradient.
struct OOut {
    @builtin(position) pos: vec4f,
    @location(0) y: f32,
};

@vertex
fn vs_header(@builtin(vertex_index) vi: u32) -> OOut {
    let c = vec2f(f32(vi & 1u), f32(vi >> 1u));
    let h = g.view.z * 1.6;
    var out: OOut;
    out.pos = to_ndc(c * vec2f(g.view.x, h));
    out.y = c.y;
    return out;
}

@fragment
fn fs_header(o: OOut) -> @location(0) vec4f {
    let a = 0.86 * (1.0 - smoothstep(0.0, 1.0, o.y));
    return vec4f(0.0, 0.0, 0.0, a);
}
