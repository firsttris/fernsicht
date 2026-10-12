// 4:2:0 YCbCr (NV12 or P010, limited range, the matrix of the picture's
// primaries) to RGB, drawn as a quad that keeps the picture's aspect ratio
// inside the target (black bars around it).

struct Push {
    // Size of the quad in normalized device coordinates (1 = full width).
    scale: vec2<f32>,
    offset: vec2<f32>,
    // The part of the picture's image shown (a decoder's images may be
    // larger than the picture: 1088 rows for 1080).
    crop: vec2<f32>,
    // A sample (UNORM) times this is its code value: 255, or 65535 / 64
    // for 16-bit words with the 10 bits on top.
    code: f32,
    // The transfer: 0 SDR.
    transfer: u32,
    // Limited range: luma black and span, chroma midpoint and span.
    range: vec4<f32>,
    // R = Y + x·Cr, G = Y − y·Cb − z·Cr, B = Y + w·Cb.
    matrix: vec4<f32>,
};

var<immediate> pc: Push;

@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var uv_plane: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VertexOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Triangle strip, 4 vertices: (0,0) (1,0) (0,1) (1,1).
@vertex
fn vs(@builtin(vertex_index) i: u32) -> VertexOut {
    let t = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    var out: VertexOut;
    out.uv = t * pc.crop;
    // Vulkan clip space: y = -1 is the top, like texture row 0.
    out.pos = vec4<f32>((t * 2.0 - 1.0) * pc.scale + pc.offset, 0.0, 1.0);
    return out;
}

@fragment
fn fs(in: VertexOut) -> @location(0) vec4<f32> {
    let y = textureSample(y_plane, samp, in.uv).r;
    // Chroma is sited left (on the even columns, as the encoders write
    // it): a quarter chroma texel right of the luma position.
    let chroma = vec2<f32>(textureDimensions(uv_plane));
    let c = textureSample(uv_plane, samp, in.uv + vec2<f32>(0.25 / chroma.x, 0.0)).rg;
    let luma = (y * pc.code - pc.range.x) / pc.range.y;
    let cb = (c.r * pc.code - pc.range.z) / pc.range.w;
    let cr = (c.g * pc.code - pc.range.z) / pc.range.w;
    let m = pc.matrix;
    let rgb = vec3<f32>(
        luma + m.x * cr,
        luma - m.y * cb - m.z * cr,
        luma + m.w * cb,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

// The pointer: BGRA with premultiplied alpha, sampled as RGBA through a
// BGRA image; blended over the video by the pipeline.
@fragment
fn fs_cursor(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(y_plane, samp, in.uv);
}
