// SPDX-License-Identifier: Apache-2.0
//! Render/present a decoded [`VideoFrame`] — docs/04-platform-backends.md.
//!
//! # Clean-room provenance
//! Rendering is built on **`wgpu`** + **`winit`** (both MIT/Apache-2.0) and a
//! first-party WGSL shader (`yuv.wgsl`). It uploads a planar [`VideoFrame`]
//! (NV12 / I420) from [`starfire-decode`] as GPU textures and converts YUV→RGB
//! with the ITU-R **BT.709** matrix (limited range by default, as Sunshine game
//! streaming uses). No Moonlight, no native Metal/D3D shader source, no ffmpeg.
//!
//! # What's here
//! - [`gpu::YuvPipeline`] — the portable wgpu upload + YUV→RGB draw pipeline,
//!   shared by windowed present and the headless test.
//! - [`VideoRenderer`] — a `winit`-window renderer that presents frames to a
//!   swapchain with low-latency present modes.
//! - A real `#[test]` (`tests/yuv_conversion.rs`) that renders a known YUV test
//!   pattern offscreen and reads back pixels to verify the conversion — runs on
//!   Windows via wgpu.

use starfire_decode::VideoFrame;

pub mod gpu;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("no renderer backend available for this platform yet")]
    NoBackend,
    #[error("present failed: {0}")]
    Failed(String),
}

/// Color pipeline the renderer must honor end-to-end. Carried for HDR routing;
/// the SDR path uses the [`VideoFrame`]'s own `color_space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Bt709Sdr,
    Hdr10Bt2020Pq,
}

/// The platform seam: present decoded frames with correct color + pacing.
pub trait Renderer {
    fn set_color_mode(&mut self, mode: ColorMode);
    /// Present one decoded frame. Returns when the frame has been submitted.
    fn present(&mut self, frame: &VideoFrame) -> Result<(), RenderError>;
}

/// A `winit` + `wgpu` window renderer. Owns a surface and the [`gpu::YuvPipeline`];
/// `present` uploads the frame and draws it to the swapchain.
///
/// Construct with [`VideoRenderer::new`] from a `winit` window inside the app's
/// `ApplicationHandler::resumed` (where the window first exists). The render core
/// ([`gpu`]) is window-agnostic, so headless tests exercise the same conversion
/// path without a window.
pub struct VideoRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: gpu::YuvPipeline,
    color_mode: ColorMode,
}

impl VideoRenderer {
    /// Create a renderer bound to `window`. `(width, height)` is the initial
    /// surface size in physical pixels.
    ///
    /// `window` must outlive the renderer; pass an `Arc<winit::window::Window>`
    /// (which satisfies wgpu's `'static` surface requirement).
    pub fn new<W>(window: W, width: u32, height: u32) -> Result<Self, RenderError>
    where
        W: wgpu::WindowHandle + 'static,
    {
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window)
            .map_err(|e| RenderError::Failed(format!("create_surface: {e}")))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|e| RenderError::Failed(format!("no GPU adapter: {e}")))?;

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("starfire-render"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            }))
            .map_err(|e| RenderError::Failed(format!("request_device: {e}")))?;

        let caps = surface.get_capabilities(&adapter);
        // Prefer an sRGB swapchain format so our linear-ish RGB output is encoded
        // correctly for display.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Low-latency present: Mailbox if available, else Fifo (always present).
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let pipeline = gpu::YuvPipeline::new(&device, format);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            color_mode: ColorMode::Bt709Sdr,
        })
    }

    /// Handle a window resize; reconfigures the swapchain.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }
}

impl Renderer for VideoRenderer {
    fn set_color_mode(&mut self, mode: ColorMode) {
        self.color_mode = mode;
    }

    fn present(&mut self, frame: &VideoFrame) -> Result<(), RenderError> {
        let surface_tex = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                self.surface
                    .get_current_texture()
                    .map_err(|e| RenderError::Failed(format!("acquire after reconfigure: {e}")))?
            }
            Err(e) => return Err(RenderError::Failed(format!("acquire frame: {e}"))),
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present"),
            });
        self.pipeline
            .draw(&self.device, &self.queue, &mut encoder, &view, frame)?;
        self.queue.submit(Some(encoder.finish()));
        surface_tex.present();
        Ok(())
    }
}
