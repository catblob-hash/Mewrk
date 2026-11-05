// Everything on screen is one of a few instanced quads. Colours are authored and blended in gamma space,
// because that is the space Fluent's translucent design tokens are specified in.

struct Globals {
    viewport: vec2f,
    text_gamma: f32,
    noise_alpha: f32,
    mica_base: vec4f,
    mica_tint_a: vec4f,
    mica_tint_b: vec4f,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var mask_atlas: texture_2d<f32>;
@group(0) @binding(2) var color_atlas: texture_2d<f32>;

fn to_clip(p: vec2f) -> vec4f {
    return vec4f(p.x / globals.viewport.x * 2.0 - 1.0, 1.0 - p.y / globals.viewport.y * 2.0, 0.0, 1.0);
}

fn corner(vi: u32) -> vec2f {
    return vec2f(f32(vi & 1u), f32((vi >> 1u) & 1u));
}

// Cheap, stable screen-space noise: enough to dither gradients and give materials their grain.
fn noise(p: vec2f) -> f32 {
    let q = fract(p * vec2f(0.1031, 0.1030));
    let r = q + dot(q, q.yx + 33.33);
    return fract((r.x + r.y) * r.x);
}

// ------------------------------------------------------------------------------- shapes

const KIND_RECT: f32 = 0.0;
const KIND_SHADOW: f32 = 1.0;
const KIND_ARC: f32 = 2.0;
const KIND_LINE: f32 = 3.0;
const KIND_ACRYLIC: f32 = 4.0;

struct RectIn {
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4f,
    @location(1) clip: vec4f,
    @location(2) radii: vec4f,
    @location(3) fill: vec4f,
    @location(4) fill2: vec4f,
    @location(5) border: vec4f,
    @location(6) border2: vec4f,
    @location(7) params: vec4f, // border width or stroke width, blur sigma, kind, how far a shadow sits below its surface
};

struct RectOut {
    @builtin(position) position: vec4f,
    @location(0) local: vec2f,
    @location(1) half_size: vec2f,
    @location(2) radii: vec4f,
    @location(3) fill: vec4f,
    @location(4) fill2: vec4f,
    @location(5) border: vec4f,
    @location(6) border2: vec4f,
    @location(7) params: vec4f,
    @location(8) pixel: vec2f,
};

@vertex
fn rect_vs(in: RectIn) -> RectOut {
    let kind = in.params.z;
    // Grow the quad so that antialiasing and shadow falloff have pixels to land on.
    var grow = 1.0;
    if kind == KIND_SHADOW {
        grow = in.params.y * 3.0 + 1.0;
    }
    let half_size = in.rect.zw * 0.5;
    let center = in.rect.xy + half_size;
    let c = corner(in.vi) * 2.0 - 1.0;
    var p = center + c * (half_size + grow);
    // Clamping the quad instead of discarding in the fragment shader means clipped pixels never run at all.
    p = clamp(p, in.clip.xy, in.clip.zw);

    var out: RectOut;
    out.position = to_clip(p);
    out.local = p - center;
    out.half_size = half_size;
    out.radii = in.radii;
    out.fill = in.fill;
    out.fill2 = in.fill2;
    out.border = in.border;
    out.border2 = in.border2;
    out.params = in.params;
    out.pixel = p;
    return out;
}

fn sd_round_rect(p: vec2f, half_size: vec2f, radii: vec4f) -> f32 {
    // radii: top-left, top-right, bottom-right, bottom-left
    var r = radii.x;
    if p.x > 0.0 && p.y < 0.0 {
        r = radii.y;
    } else if p.x > 0.0 && p.y > 0.0 {
        r = radii.z;
    } else if p.x < 0.0 && p.y > 0.0 {
        r = radii.w;
    }
    let q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2f(0.0))) - r;
}

fn premul(c: vec4f) -> vec4f {
    return vec4f(c.rgb * c.a, c.a);
}

@fragment
fn rect_fs(in: RectOut) -> @location(0) vec4f {
    let kind = in.params.z;

    if kind == KIND_LINE {
        // A capsule from (radii.xy) to (radii.zw), in the same local space as `local`.
        let a = in.radii.xy;
        let b = in.radii.zw;
        let pa = in.local - a;
        let ba = b - a;
        let t = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-4), 0.0, 1.0);
        let d = length(pa - ba * t) - in.params.x * 0.5;
        return premul(in.fill) * clamp(0.5 - d, 0.0, 1.0);
    }

    if kind == KIND_ARC {
        // radii.x = start angle, radii.y = sweep, both in radians, clockwise from 12 o'clock.
        let radius = min(in.half_size.x, in.half_size.y) - in.params.x * 0.5;
        let thickness = in.params.x * 0.5;
        let d_ring = abs(length(in.local) - radius) - thickness;
        let tau = 6.28318530718;
        let angle = fract((atan2(in.local.x, -in.local.y) - in.radii.x) / tau) * tau;
        var alpha = clamp(0.5 - d_ring, 0.0, 1.0);
        if angle > in.radii.y {
            // Round end caps, measured from the nearer end of the arc.
            let end0 = vec2f(sin(in.radii.x), -cos(in.radii.x)) * radius;
            let end1 = vec2f(sin(in.radii.x + in.radii.y), -cos(in.radii.x + in.radii.y)) * radius;
            let d_cap = min(length(in.local - end0), length(in.local - end1)) - thickness;
            alpha = clamp(0.5 - d_cap, 0.0, 1.0);
        }
        return premul(in.fill) * alpha;
    }

    let d = sd_round_rect(in.local, in.half_size, in.radii);

    if kind == KIND_SHADOW {
        // Close enough to a blurred box for soft elevation, and a fraction of the cost of the real thing.
        let sigma = max(in.params.y, 0.5);
        let x = clamp(d / (sigma * 2.2), -1.0, 1.0);
        var alpha = 0.5 - x * (0.75 - 0.25 * x * x);
        // Translucent surfaces must not show their own shadow through them.
        let surface = sd_round_rect(in.local + vec2f(0.0, in.params.w), in.half_size, in.radii);
        alpha *= clamp(surface + 0.5, 0.0, 1.0);
        return premul(in.fill) * alpha;
    }

    let v = clamp((in.local.y + in.half_size.y) / max(in.half_size.y * 2.0, 1.0), 0.0, 1.0);
    var fill = mix(in.fill, in.fill2, v);
    if kind == KIND_ACRYLIC {
        fill = vec4f(fill.rgb + (noise(in.pixel) - 0.5) * globals.noise_alpha * 2.0, fill.a);
    }
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    var color = premul(fill);

    let bw = in.params.x;
    if bw > 0.0 {
        // The stroke's colour runs down the shape, which is how Fluent controls get their lit top edge.
        let edge = mix(in.border, in.border2, step(in.half_size.y * 2.0 - bw - 0.5, in.local.y + in.half_size.y));
        let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
        // Stroke over fill, with the stroke only outside `inner`.
        let stroke_alpha = (1.0 - inner) * edge.a;
        color = premul(vec4f(edge.rgb, stroke_alpha)) + color * (1.0 - stroke_alpha);
    }
    return color * coverage;
}

// ------------------------------------------------------------------------------- glyphs

struct GlyphIn {
    @builtin(vertex_index) vi: u32,
    @location(0) pos: vec2f,
    @location(1) size_uv: vec4u,
    @location(2) color: vec4f,
    @location(3) clip: vec4i,
    @location(4) flags: u32,
};

struct GlyphOut {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
    @location(2) @interpolate(flat) flags: u32,
};

@vertex
fn glyph_vs(in: GlyphIn) -> GlyphOut {
    let size = vec2f(f32(in.size_uv.x), f32(in.size_uv.y));
    let uv0 = vec2f(f32(in.size_uv.z), f32(in.size_uv.w));
    let p = in.pos + corner(in.vi) * size;
    let q = clamp(p, vec2f(in.clip.xy), vec2f(in.clip.zw));
    var out: GlyphOut;
    out.position = to_clip(q);
    out.uv = uv0 + (q - in.pos);
    out.color = in.color;
    out.flags = in.flags;
    return out;
}

@fragment
fn glyph_fs(in: GlyphOut) -> @location(0) vec4f {
    // Glyphs sit on whole pixels, so an exact texel fetch is both correct and faster than a sampler.
    let texel = vec2i(floor(in.uv));
    if (in.flags & 1u) != 0u {
        // Colour bitmaps come with straight alpha, and blending here is premultiplied.
        let c = textureLoad(color_atlas, texel, 0);
        return vec4f(c.rgb * c.a, c.a) * in.color.a;
    }
    let coverage = pow(textureLoad(mask_atlas, texel, 0).r, globals.text_gamma);
    return vec4f(in.color.rgb * in.color.a, in.color.a) * coverage;
}

// ------------------------------------------------------------------------------- images

struct ImageUniform {
    rect: vec4f,
    clip: vec4f,
    uv: vec4f,
    radius_opacity: vec4f,
};

@group(1) @binding(0) var image_tex: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

struct ImageIn {
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4f,
    @location(1) clip: vec4f,
    @location(2) uv: vec4f,
    @location(3) radii: vec4f,
    @location(4) opacity: vec4f,
};

struct ImageOut {
    @builtin(position) position: vec4f,
    @location(0) local: vec2f,
    @location(1) half_size: vec2f,
    @location(2) uv: vec2f,
    @location(3) radii: vec4f,
    @location(4) opacity: f32,
};

@vertex
fn image_vs(in: ImageIn) -> ImageOut {
    let half_size = in.rect.zw * 0.5;
    let center = in.rect.xy + half_size;
    let c = corner(in.vi);
    let p = clamp(in.rect.xy + c * in.rect.zw, in.clip.xy, in.clip.zw);
    let t = (p - in.rect.xy) / max(in.rect.zw, vec2f(1.0));
    var out: ImageOut;
    out.position = to_clip(p);
    out.local = p - center;
    out.half_size = half_size;
    out.uv = in.uv.xy + t * in.uv.zw;
    out.radii = in.radii;
    out.opacity = in.opacity.x;
    return out;
}

@fragment
fn image_fs(in: ImageOut) -> @location(0) vec4f {
    let c = textureSample(image_tex, image_sampler, in.uv);
    let d = sd_round_rect(in.local, in.half_size, in.radii);
    let a = clamp(0.5 - d, 0.0, 1.0) * in.opacity;
    return vec4f(c.rgb * c.a, c.a) * a;
}

// ------------------------------------------------------------------------------- Mica backdrop

struct MicaOut {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn mica_vs(@builtin(vertex_index) vi: u32) -> MicaOut {
    let p = vec2f(f32((vi << 1u) & 2u), f32(vi & 2u));
    var out: MicaOut;
    out.position = vec4f(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    out.uv = p;
    return out;
}

// What Mica really is: a base colour with a heavily blurred, desaturated hint of what lies behind it. A
// window cannot see the desktop from here, so two soft tints stand in for the wallpaper's colour.
@fragment
fn mica_fs(in: MicaOut) -> @location(0) vec4f {
    let aspect = globals.viewport.x / max(globals.viewport.y, 1.0);
    let p = vec2f(in.uv.x * aspect, in.uv.y);
    let a = smoothstep(1.25, 0.0, distance(p, vec2f(0.15 * aspect, -0.05)));
    let b = smoothstep(1.4, 0.0, distance(p, vec2f(0.95 * aspect, 1.1)));
    var c = globals.mica_base.rgb;
    c = mix(c, globals.mica_tint_a.rgb, a * globals.mica_tint_a.a);
    c = mix(c, globals.mica_tint_b.rgb, b * globals.mica_tint_b.a);
    c += (noise(in.position.xy) - 0.5) * globals.noise_alpha;
    return vec4f(c, 1.0);
}
