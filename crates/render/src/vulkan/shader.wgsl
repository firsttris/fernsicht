// NV12 (BT.709, limited range) to RGB, drawn as a quad that keeps the
// picture's aspect ratio inside the target (black bars around it).

struct Push {
    // Size of the quad in normalized device coordinates (1 = full width).
    scale: vec2<f32>,
    offset: vec2<f32>,
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
    out.uv = t;
    // Vulkan clip space: y = -1 is the top, like texture row 0.
    out.pos = vec4<f32>((t * 2.0 - 1.0) * pc.scale + pc.offset, 0.0, 1.0);
    return out;
}

@fragment
fn fs(in: VertexOut) -> @location(0) vec4<f32> {
    let y = textureSample(y_plane, samp, in.uv).r;
    let c = textureSample(uv_plane, samp, in.uv).rg;
    let luma = (y * 255.0 - 16.0) / 219.0;
    let cb = (c.r * 255.0 - 128.0) / 224.0;
    let cr = (c.g * 255.0 - 128.0) / 224.0;
    let rgb = vec3<f32>(
        luma + 1.5748 * cr,
        luma - 0.1873 * cb - 0.4681 * cr,
        luma + 1.8556 * cb,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

// The pointer: BGRA with premultiplied alpha, sampled as RGBA through a
// BGRA image; blended over the video by the pipeline.
@fragment
fn fs_cursor(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(y_plane, samp, in.uv);
}
