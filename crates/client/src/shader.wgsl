// I420 (YCbCr 4:2:0) -> RGB in the shader. No CPU colour conversion.
// The colour matrix (BT.601 / BT.709) and range (limited / full) are a
// per-window uniform, not hard-coded: HD VP9 is normally BT.709 limited,
// SD is normally BT.601 limited. Defaults are chosen by resolution in
// remote_window.rs (Colorimetry::default_for) and can be overridden.

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

struct ColorParams {
    matrix: u32,     // 0 = BT.601, 1 = BT.709
    full_range: u32, // 0 = limited, 1 = full
    _pad0: u32,
    _pad1: u32,
};
@group(0) @binding(4) var<uniform> color: ColorParams;

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let y_raw = textureSample(tex_y, samp, in.uv).r;
    let u_raw = textureSample(tex_u, samp, in.uv).r;
    let v_raw = textureSample(tex_v, samp, in.uv).r;

    var yy: f32;
    var uu: f32;
    var vv: f32;
    if (color.full_range == 1u) {
        yy = y_raw;
        uu = u_raw - 0.5;
        vv = v_raw - 0.5;
    } else {
        yy = max((y_raw - 16.0 / 255.0) * (255.0 / 219.0), 0.0);
        uu = (u_raw - 128.0 / 255.0) * (255.0 / 224.0);
        vv = (v_raw - 128.0 / 255.0) * (255.0 / 224.0);
    }

    var r: f32;
    var g: f32;
    var b: f32;
    if (color.matrix == 1u) {
        // BT.709
        r = yy + 1.5748 * vv;
        g = yy - 0.1873 * uu - 0.4681 * vv;
        b = yy + 1.8556 * uu;
    } else {
        // BT.601
        r = yy + 1.596 * vv;
        g = yy - 0.813 * vv - 0.391 * uu;
        b = yy + 2.018 * uu;
    }
    return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
}
