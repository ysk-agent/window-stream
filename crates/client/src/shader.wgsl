// I420 (YCbCr 4:2:0) -> RGB conversion in the shader (BT.601, limited range).
// No CPU colour conversion anywhere in the client.
// Vertex positions arrive in clip space (computed on CPU from window
// geometry); uv is in [0,1] over the remote window's frame.

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VertexOut {
    var out: VertexOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv).r;
    let u = textureSample(tex_u, samp, in.uv).r;
    let v = textureSample(tex_v, samp, in.uv).r;
    // BT.601 limited range: Y in [16,235], U/V centred at 128.
    let yy = max((y - 16.0 / 255.0) * (255.0 / 219.0), 0.0);
    let uu = u - 128.0 / 255.0;
    let vv = v - 128.0 / 255.0;
    let r = yy + 1.596 * vv;
    let g = yy - 0.813 * vv - 0.391 * uu;
    let b = yy + 2.018 * uu;
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
