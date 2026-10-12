// RGB (any size) -> NV12 or P010 (limited range, BT.709 or BT.2020 matrix)
// at the output size.
//
// One invocation writes a 4x2 block. NV12: 4 luma bytes in each of 2 rows
// (one u32 each) and 2 chroma pairs (U, V, U, V: one u32). P010: each
// sample a 16-bit word with its 10 bits on top, so two u32 per luma row
// and two for the chroma pairs. Rows are `pitch` bytes apart; the UV plane
// follows the Y plane.
//
// The values are converted as they are: SDR desktops are sRGB-encoded,
// HDR desktops PQ-encoded BT.2020; the matrix follows the primaries.

struct Params {
    // Output size in pixels; width rounded up to a multiple of 4.
    width: u32,
    height: u32,
    pitch: u32,
    // Bit 0: 10 bit (P010).
    flags: u32,
    // Luma weights of the matrix.
    kr: f32,
    kb: f32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
var<immediate> p: Params;

fn deep() -> bool {
    return (p.flags & 1u) != 0u;
}

fn luma(c: vec3<f32>) -> f32 {
    return p.kr * c.r + (1.0 - p.kr - p.kb) * c.g + p.kb * c.b;
}

// A code value at the output depth, in the bits of its container: a byte,
// or the top 10 bits of a 16-bit word.
fn code(v: f32) -> u32 {
    if (deep()) {
        return u32(clamp(floor(v + 0.5), 0.0, 1023.0)) << 6u;
    }
    return u32(clamp(floor(v + 0.5), 0.0, 255.0));
}

// Limited range: black and span of luma, midpoint and span of chroma.
fn range() -> vec4<f32> {
    if (deep()) {
        return vec4<f32>(64.0, 876.0, 512.0, 896.0);
    }
    return vec4<f32>(16.0, 219.0, 128.0, 224.0);
}

fn rgb_at(x: i32, y: u32) -> vec3<f32> {
    // Clamped at the edges by the sampler.
    let uv = (vec2<f32>(f32(x), f32(y)) + 0.5) / vec2<f32>(f32(p.width), f32(p.height));
    return textureSampleLevel(src, smp, uv, 0.0).rgb;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let x0 = id.x * 4u;
    let y0 = id.y * 2u;
    if (x0 >= p.width || y0 >= p.height) {
        return;
    }
    let r = range();
    let bytes = select(1u, 2u, deep());
    for (var row = 0u; row < 2u; row++) {
        var y = array<u32, 4>();
        for (var i = 0u; i < 4u; i++) {
            y[i] = code(r.x + r.y * luma(rgb_at(i32(x0 + i), y0 + row)));
        }
        let at = ((y0 + row) * p.pitch + x0 * bytes) / 4u;
        if (deep()) {
            dst[at] = y[0] | (y[1] << 16u);
            dst[at + 1u] = y[2] | (y[3] << 16u);
        } else {
            dst[at] = y[0] | (y[1] << 8u) | (y[2] << 16u) | (y[3] << 24u);
        }
    }
    // Chroma sited left (H.264's and HEVC's default, which NVENC streams do
    // not override; AV1 calls it "vertical"): on the even column, between
    // the two rows. Filtered 1/4, 1/2, 1/4 across the neighbours.
    var pairs = array<u32, 2>();
    for (var pair = 0u; pair < 2u; pair++) {
        let cx = i32(x0 + pair * 2u);
        var c = vec3<f32>(0.0);
        for (var row = 0u; row < 2u; row++) {
            c += 0.25 * rgb_at(cx - 1, y0 + row) + 0.5 * rgb_at(cx, y0 + row)
                + 0.25 * rgb_at(cx + 1, y0 + row);
        }
        c *= 0.5;
        let y = luma(c);
        let cb = code(r.z + r.w * (c.b - y) / (2.0 * (1.0 - p.kb)));
        let cr = code(r.z + r.w * (c.r - y) / (2.0 * (1.0 - p.kr)));
        pairs[pair] = cb | (cr << (8u * bytes));
    }
    let at = (p.pitch * p.height + (y0 / 2u) * p.pitch + x0 * bytes) / 4u;
    if (deep()) {
        dst[at] = pairs[0];
        dst[at + 1u] = pairs[1];
    } else {
        dst[at] = pairs[0] | (pairs[1] << 16u);
    }
}
