// SPDX-License-Identifier: Apache-2.0
// YUV (NV12 / I420) -> RGB, BT.709, selectable range.
// Clean-room: standard ITU-R BT.709 matrix math; no third-party shader source.

struct Params {
    // 0 = NV12 (y_tex + cbcr_tex), 1 = I420 (y_tex + cb in cbcr_tex.r path).
    format: u32,
    // 0 = limited/studio range, 1 = full range.
    full_range: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var y_tex: texture_2d<f32>;
// For NV12 this is the RG8 interleaved CbCr plane; for I420 it's Cb (R8).
@group(0) @binding(2) var u_tex: texture_2d<f32>;
// Unused for NV12; for I420 it's Cr (R8).
@group(0) @binding(3) var v_tex: texture_2d<f32>;
@group(0) @binding(4) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Fullscreen triangle: 3 verts covering the viewport, no vertex buffer.
@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    var out: VsOut;
    // (0,0),(2,0),(0,2) in UV -> clip coords covering [-1,1].
    let uv = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u));
    out.uv = uv;
    out.pos = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    // Flip Y so texture row 0 is at the top of the output.
    out.pos.y = -out.pos.y;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(y_tex, samp, in.uv).r;
    var u: f32;
    var v: f32;
    if (params.format == 0u) {
        // NV12: CbCr interleaved in an RG8 texture.
        let cbcr = textureSample(u_tex, samp, in.uv).rg;
        u = cbcr.r;
        v = cbcr.g;
    } else {
        // I420: separate Cb / Cr R8 textures.
        u = textureSample(u_tex, samp, in.uv).r;
        v = textureSample(v_tex, samp, in.uv).r;
    }

    var yf: f32;
    var uf: f32;
    var vf: f32;
    if (params.full_range == 1u) {
        yf = y;
        uf = u - 0.5;
        vf = v - 0.5;
    } else {
        // Limited range: Y in [16,235], C in [16,240] over 8-bit.
        yf = (y - 16.0 / 255.0) * (255.0 / 219.0);
        uf = (u - 128.0 / 255.0) * (255.0 / 224.0);
        vf = (v - 128.0 / 255.0) * (255.0 / 224.0);
    }

    // ITU-R BT.709 YCbCr -> R'G'B'.
    let r = yf + 1.5748 * vf;
    let g = yf - 0.1873 * uf - 0.4681 * vf;
    let b = yf + 1.8556 * uf;

    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
