// SPDX-License-Identifier: Apache-2.0
// NV12 -> RGB, BT.709, selectable range. Metal Shading Language port of yuv.wgsl.
// Clean-room: standard ITU-R BT.709 matrix math; no third-party shader source.
//
// Only NV12 is handled here (VideoToolbox always emits biplanar 4:2:0): a full-res
// Y plane (r8) and an interleaved half-res CbCr plane (rg8). The output is
// gamma-encoded R'G'B'; the CAMetalLayer/pipeline uses a *_sRGB drawable so the
// hardware applies the final sRGB encode on write — matching the wgpu path.

#include <metal_stdlib>
using namespace metal;

struct Params {
    uint format;     // unused (always NV12) — kept for layout parity with the wgpu Params
    uint full_range; // 0 = limited/studio range, 1 = full range
    uint pad0;
    uint pad1;
};

struct VsOut {
    float4 pos [[position]];
    float2 uv;
};

// Fullscreen triangle: 3 verts covering the viewport, no vertex buffer.
vertex VsOut vs_main(uint vid [[vertex_id]]) {
    VsOut out;
    float2 uv = float2(float((vid << 1) & 2), float(vid & 2));
    out.uv = uv;
    out.pos = float4(uv * 2.0 - 1.0, 0.0, 1.0);
    // Flip Y so texture row 0 is at the top of the output.
    out.pos.y = -out.pos.y;
    return out;
}

fragment float4 fs_main(VsOut in [[stage_in]],
                        texture2d<float> y_tex    [[texture(0)]],
                        texture2d<float> cbcr_tex [[texture(1)]],
                        constant Params& params   [[buffer(0)]]) {
    constexpr sampler samp(filter::linear, address::clamp_to_edge);

    float y = y_tex.sample(samp, in.uv).r;
    float2 cbcr = cbcr_tex.sample(samp, in.uv).rg;
    float u = cbcr.r;
    float v = cbcr.g;

    float yf, uf, vf;
    if (params.full_range == 1) {
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
    float r = yf + 1.5748 * vf;
    float g = yf - 0.1873 * uf - 0.4681 * vf;
    float b = yf + 1.8556 * uf;

    return float4(clamp(float3(r, g, b), 0.0, 1.0), 1.0);
}
