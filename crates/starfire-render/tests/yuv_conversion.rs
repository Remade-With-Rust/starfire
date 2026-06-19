// SPDX-License-Identifier: Apache-2.0
//! Headless YUV→RGB correctness test.
//!
//! Renders a known YUV test pattern offscreen via the `wgpu` pipeline and reads
//! the pixels back, verifying the BT.709 limited-range conversion against a
//! CPU reference. Runs on Windows (and anywhere wgpu finds an adapter).
//!
//! Clean-room: exercises only `starfire-render`'s public pipeline + a
//! first-party CPU reference of the same standard BT.709 math.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use starfire_decode::{ColorSpace, PixelFormat, Plane, VideoFrame};
use starfire_render::gpu;

/// CPU reference: BT.709 limited-range YUV (8-bit) → sRGB-target RGB8.
/// Mirrors `yuv.wgsl`. The render target is `Rgba8Unorm` (non-sRGB) so the
/// shader's linear RGB lands in the buffer 1:1 — no transfer curve to undo.
fn ref_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yf = (y as f32 - 16.0) * (255.0 / 219.0) / 255.0;
    let uf = (u as f32 - 128.0) * (255.0 / 224.0) / 255.0;
    let vf = (v as f32 - 128.0) * (255.0 / 224.0) / 255.0;
    let r = yf + 1.5748 * vf;
    let g = yf - 0.1873 * uf - 0.4681 * vf;
    let b = yf + 1.8556 * uf;
    [
        (r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (b.clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

/// Build a solid-color NV12 frame of `w`x`h` with the given YUV sample.
fn solid_nv12(w: u32, h: u32, y: u8, u: u8, v: u8) -> VideoFrame {
    let cw = w.div_ceil(2) as usize;
    let ch = h.div_ceil(2) as usize;
    let mut cbcr = Vec::with_capacity(cw * 2 * ch);
    for _ in 0..(cw * ch) {
        cbcr.push(u);
        cbcr.push(v);
    }
    VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Nv12,
        color_space: ColorSpace::Bt709Limited,
        pts: 0,
        planes: vec![
            Plane::new(vec![y; (w * h) as usize], w as usize),
            Plane::new(cbcr, cw * 2),
        ],
    }
}

/// Build a solid-color I420 frame.
fn solid_i420(w: u32, h: u32, y: u8, u: u8, v: u8) -> VideoFrame {
    let cw = w.div_ceil(2) as usize;
    let ch = h.div_ceil(2) as usize;
    VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::I420,
        color_space: ColorSpace::Bt709Limited,
        pts: 0,
        planes: vec![
            Plane::new(vec![y; (w * h) as usize], w as usize),
            Plane::new(vec![u; cw * ch], cw),
            Plane::new(vec![v; cw * ch], cw),
        ],
    }
}

/// Render a solid frame and return the center pixel's RGB.
fn render_center(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &gpu::YuvPipeline,
    frame: &VideoFrame,
) -> [u8; 3] {
    let (w, h, rgba) = gpu::render_to_rgba(device, queue, pipeline, frame, 16, 16).unwrap();
    let cx = w / 2;
    let cy = h / 2;
    let idx = ((cy * w + cx) * 4) as usize;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2]]
}

fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
    (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= tol)
}

#[test]
fn nv12_bt709_limited_matches_reference() {
    let (_instance, _adapter, device, queue) = match gpu::request_headless_device() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: no GPU adapter available ({e})");
            return;
        }
    };
    // Non-sRGB target so the shader's RGB lands 1:1 for deterministic compare.
    let pipeline = gpu::YuvPipeline::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    // (name, Y, U, V) reference points.
    let cases = [
        ("black", 16u8, 128u8, 128u8),
        ("white", 235, 128, 128),
        ("mid-gray", 126, 128, 128),
        ("red-ish", 63, 102, 240),
        ("green-ish", 173, 42, 26),
        ("blue-ish", 32, 240, 118),
    ];

    for (name, y, u, v) in cases {
        let frame = solid_nv12(64, 64, y, u, v);
        let got = render_center(&device, &queue, &pipeline, &frame);
        let want = ref_rgb(y, u, v);
        assert!(
            close(got, want, 3),
            "NV12 {name}: got {got:?}, want {want:?} (Y={y} U={u} V={v})"
        );
    }
}

#[test]
fn nv12_known_anchor_colors() {
    let (_i, _a, device, queue) = match gpu::request_headless_device() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: no GPU adapter available ({e})");
            return;
        }
    };
    let pipeline = gpu::YuvPipeline::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    // Limited-range black -> (0,0,0), white -> (255,255,255).
    let black = render_center(&device, &queue, &pipeline, &solid_nv12(32, 32, 16, 128, 128));
    assert!(close(black, [0, 0, 0], 2), "black -> {black:?}");

    let white = render_center(
        &device,
        &queue,
        &pipeline,
        &solid_nv12(32, 32, 235, 128, 128),
    );
    assert!(close(white, [255, 255, 255], 2), "white -> {white:?}");
}

#[test]
fn i420_matches_nv12_for_same_samples() {
    let (_i, _a, device, queue) = match gpu::request_headless_device() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: no GPU adapter available ({e})");
            return;
        }
    };
    let pipeline = gpu::YuvPipeline::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    for (y, u, v) in [(126u8, 128u8, 128u8), (63, 102, 240), (173, 42, 26)] {
        let nv12 = render_center(&device, &queue, &pipeline, &solid_nv12(48, 48, y, u, v));
        let i420 = render_center(&device, &queue, &pipeline, &solid_i420(48, 48, y, u, v));
        assert!(
            close(nv12, i420, 2),
            "NV12 {nv12:?} vs I420 {i420:?} for Y={y} U={u} V={v}"
        );
    }
}

/// Padded-stride upload path: a Y plane with row padding must still convert
/// correctly (the pipeline repacks rows before upload).
#[test]
fn handles_padded_stride() {
    let (_i, _a, device, queue) = match gpu::request_headless_device() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("skipping: no GPU adapter available ({e})");
            return;
        }
    };
    let pipeline = gpu::YuvPipeline::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    let (w, h) = (32u32, 32u32);
    let (y, u, v) = (180u8, 90u8, 150u8);
    let y_stride = w as usize + 13; // padded
    let cw = w.div_ceil(2) as usize;
    let ch = h.div_ceil(2) as usize;
    let c_stride = cw * 2 + 7; // padded interleaved CbCr

    let mut y_plane = vec![0u8; y_stride * h as usize];
    for row in 0..h as usize {
        for col in 0..w as usize {
            y_plane[row * y_stride + col] = y;
        }
    }
    let mut c_plane = vec![0u8; c_stride * ch];
    for row in 0..ch {
        for col in 0..cw {
            c_plane[row * c_stride + col * 2] = u;
            c_plane[row * c_stride + col * 2 + 1] = v;
        }
    }
    let frame = VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Nv12,
        color_space: ColorSpace::Bt709Limited,
        pts: 0,
        planes: vec![Plane::new(y_plane, y_stride), Plane::new(c_plane, c_stride)],
    };

    let got = render_center(&device, &queue, &pipeline, &frame);
    let want = ref_rgb(y, u, v);
    assert!(close(got, want, 3), "padded: got {got:?}, want {want:?}");
}
