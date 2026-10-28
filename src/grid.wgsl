struct Globals {
    view: vec4f,   // width, height, header height, scale factor
    th: vec4f,     // cell px, cells per row, atlas width, atlas height
    small: vec4f,  // slot px, slots per row, atlas size, mip count
    large: vec4f,
    xl: vec4f,
    xl_size: vec4f, // atlas width, height
    sizes: vec4f,   // small atlas width, height; large atlas width, height
    accent: vec4f,  // theme colours, linear: selection, the tick on it, background/scrim
    on_accent: vec4f,
    bg: vec4f,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var th_tex: texture_2d<f32>;
@group(0) @binding(3) var small_tex: texture_2d<f32>;
@group(0) @binding(4) var large_tex: texture_2d<f32>;
@group(0) @binding(5) var xl_tex: texture_2d<f32>;

struct Tile {
    @location(0) rect: vec4f,
    @location(1) th: u32,
    @location(2) slot: u32,
    @location(3) fade: f32,
    @location(4) flags: u32,
};

struct VOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) @interpolate(flat) th: u32,
    @location(2) @interpolate(flat) slot: u32,
    @location(3) fade: f32,
    @location(4) size: f32,
    @location(5) @interpolate(flat) flags: u32,
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
    out.flags = t.flags;
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
        let s = v.slot & 0x3fffffffu;
        var img: vec4f;
        if (v.slot & 0x40000000u) != 0u {
            img = cell_sample(xl_tex, s, g.xl, g.xl_size.xy, g.xl.w, v.uv, v.size);
        } else if (v.slot & 0x80000000u) != 0u {
            img = cell_sample(large_tex, s, g.large, g.sizes.zw, g.large.w, v.uv, v.size);
        } else {
            img = cell_sample(small_tex, s, g.small, g.sizes.xy, g.small.w, v.uv, v.size);
        }
        c = mix(c, img, v.fade);
    }
    if (v.flags & 1u) != 0u {
        c = vec4f(selected(c.rgb, v.uv, v.size), 1.0);
    }
    // Trashed/archived tiles dim away before the grid closes the gap.
    let leaving = f32((v.flags >> 8u) & 255u) / 255.0;
    return vec4f(mix(c.rgb, g.bg.rgb, leaving), 1.0);
}

// Apple-style selection: the photo dims slightly inside a thin accent inset, with an accent
// check badge in the bottom-right corner (white ring, tick in the accent's text colour).
// All SDFs in tile pixels.
fn selected(rgb: vec3f, uv: vec2f, size: f32) -> vec3f {
    let px = uv * size;
    let s = g.view.w;
    let accent = g.accent.rgb;
    var c = rgb * 0.72;
    let inset = min(min(px.x, px.y), min(size - px.x, size - px.y));
    let ring = clamp(min(2.5 * s, size * 0.04), 1.0, 3.0 * s);
    c = mix(c, accent, clamp(ring + 0.5 - inset, 0.0, 1.0));
    if size < 24.0 * s {
        return c;
    }
    let r = clamp(size * 0.09, 7.0 * s, 11.0 * s);
    let center = vec2f(size - r - clamp(size * 0.05, 4.0 * s, 8.0 * s));
    let d = length(px - center);
    c = mix(c, vec3f(1.0), clamp(r + 0.5 - d, 0.0, 1.0));
    c = mix(c, accent, clamp(r - 1.5 * s + 0.5 - d, 0.0, 1.0));
    // Tick: two strokes through (-0.45, 0.0) → (-0.12, 0.33) → (0.45, -0.3), in units of r.
    let q = (px - center) / r;
    let tick = min(segment(q, vec2f(-0.45, 0.02), vec2f(-0.12, 0.33)), segment(q, vec2f(-0.12, 0.33), vec2f(0.45, -0.3)));
    let w = 0.11;
    c = mix(c, g.on_accent.rgb, clamp((w - tick) * r + 0.5, 0.0, 1.0));
    return c;
}

fn segment(p: vec2f, a: vec2f, b: vec2f) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

// Header scrim: content scrolls underneath a soft gradient of the background colour.
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
    return vec4f(g.bg.rgb, a);
}
