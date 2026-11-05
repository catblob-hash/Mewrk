// The glass worked out on the GPU, for as long as the bake it stands in for isn't ready. It follows what `mewrk::glass::bake` does on the
// CPU step for step, so that the bake taking over shows nothing: shrink in linear light, blur, saturate.

struct Pass {
    // A source texel's size, or the step along the blur's direction.
    step: vec2f,
    sigma: f32,
    saturate: f32,
};

@group(0) @binding(0) var<uniform> pass_: Pass;
@group(1) @binding(0) var source: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

struct PlateOut {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn plate_vs(@builtin(vertex_index) vi: u32) -> PlateOut {
    let p = vec2f(f32((vi << 1u) & 2u), f32(vi & 2u));
    var out: PlateOut;
    out.position = vec4f(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0);
    out.uv = p;
    return out;
}

fn to_linear(c: vec3f) -> vec3f {
    return select(pow((c + 0.055) / 1.055, vec3f(2.4)), c / 12.92, c <= vec3f(0.04045));
}

fn to_srgb(c: vec3f) -> vec3f {
    return select(1.055 * pow(max(c, vec3f(0.0)), vec3f(1.0 / 2.4)) - 0.055, c * 12.92, c <= vec3f(0.0031308));
}

// Four samples spread over the output pixel, averaged the way `area_resize` averages: linear and premultiplied. At exactly half size they
// fall on the centres of the four texels underneath, so the sampler's own filtering, which isn't linear, doesn't come into it.
@fragment
fn shrink_fs(in: PlateOut) -> @location(0) vec4f {
    var sum = vec4f(0.0);
    for (var i = 0; i < 4; i++) {
        let offset = vec2f(f32(i & 1) - 0.5, f32((i >> 1) & 1) - 0.5) * pass_.step;
        let s = textureSampleLevel(source, source_sampler, in.uv + offset, 0.0);
        sum += vec4f(to_linear(s.rgb) * s.a, s.a);
    }
    if sum.a <= 0.0 {
        return vec4f(0.0);
    }
    return vec4f(to_srgb(sum.rgb / sum.a), sum.a * 0.25);
}

// One direction of the blur, on the encoded values as `blur_pixels` does it. The end of the texture repeats outwards.
fn blur(uv: vec2f) -> vec3f {
    if pass_.sigma <= 0.0 {
        return textureSampleLevel(source, source_sampler, uv, 0.0).rgb;
    }
    let radius = i32(ceil(pass_.sigma * 3.0));
    var sum = vec3f(0.0);
    var total = 0.0;
    for (var i = -radius; i <= radius; i++) {
        let at = f32(i) / pass_.sigma;
        let weight = exp(-0.5 * at * at);
        sum += textureSampleLevel(source, source_sampler, uv + pass_.step * f32(i), 0.0).rgb * weight;
        total += weight;
    }
    return sum / total;
}

@fragment
fn blur_fs(in: PlateOut) -> @location(0) vec4f {
    return vec4f(blur(in.uv), textureSampleLevel(source, source_sampler, in.uv, 0.0).a);
}

// The other direction, and `saturate()`, which `filter_color` also runs: the same matrix.
@fragment
fn finish_fs(in: PlateOut) -> @location(0) vec4f {
    let c = blur(in.uv);
    let s = pass_.saturate;
    let out = vec3f(
        (0.213 + 0.787 * s) * c.r + (0.715 - 0.715 * s) * c.g + (0.072 - 0.072 * s) * c.b,
        (0.213 - 0.213 * s) * c.r + (0.715 + 0.285 * s) * c.g + (0.072 - 0.072 * s) * c.b,
        (0.213 - 0.213 * s) * c.r + (0.715 - 0.715 * s) * c.g + (0.072 + 0.928 * s) * c.b,
    );
    return vec4f(clamp(out, vec3f(0.0), vec3f(1.0)), textureSampleLevel(source, source_sampler, in.uv, 0.0).a);
}
