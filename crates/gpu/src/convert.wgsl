// RGB (any size) -> NV12 (BT.709, limited range) at the output size.
//
// One invocation writes a 4x2 block: 4 luma bytes in each of 2 rows (one
// u32 each) and 2 chroma pairs (U, V, U, V: one u32). Rows are `pitch`
// bytes apart; the UV plane follows the Y plane.

struct Params {
    // Output size in pixels; width rounded up to a multiple of 4.
    width: u32,
    height: u32,
    pitch: u32,
    _pad: u32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
var<immediate> p: Params;

const KR: f32 = 0.2126;
const KB: f32 = 0.0722;

fn luma(c: vec3<f32>) -> f32 {
    return KR * c.r + (1.0 - KR - KB) * c.g + KB * c.b;
}

fn to_byte(v: f32) -> u32 {
    return u32(clamp(floor(v + 0.5), 0.0, 255.0));
}

fn rgb_at(x: u32, y: u32) -> vec3<f32> {
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
    var chroma = 0u;
    for (var row = 0u; row < 2u; row++) {
        var word = 0u;
        for (var i = 0u; i < 4u; i++) {
            let c = rgb_at(x0 + i, y0 + row);
            word |= to_byte(16.0 + 219.0 * luma(c)) << (8u * i);
        }
        dst[((y0 + row) * p.pitch + x0) / 4u] = word;
    }
    // Chroma of each 2x2 pair: the mean of its four pixels.
    for (var pair = 0u; pair < 2u; pair++) {
        var c = vec3<f32>(0.0);
        for (var j = 0u; j < 4u; j++) {
            c += rgb_at(x0 + pair * 2u + (j & 1u), y0 + (j >> 1u));
        }
        c *= 0.25;
        let y = luma(c);
        let cb = 128.0 + 224.0 * (c.b - y) / (2.0 * (1.0 - KB));
        let cr = 128.0 + 224.0 * (c.r - y) / (2.0 * (1.0 - KR));
        chroma |= (to_byte(cb) | (to_byte(cr) << 8u)) << (16u * pair);
    }
    dst[(p.pitch * p.height + (y0 / 2u) * p.pitch + x0) / 4u] = chroma;
}
