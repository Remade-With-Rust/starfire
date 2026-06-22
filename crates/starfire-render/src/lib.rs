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

/// macOS zero-copy Metal present backend (selected by [`new_for_window`]).
#[cfg(target_os = "macos")]
pub mod metal;

/// Windows zero-copy D3D11 present backend (selected by `new_d3d11_for_window`).
#[cfg(target_os = "windows")]
pub mod d3d11;

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

/// The active renderer for this platform/session. On macOS the zero-copy Metal
/// backend is selected by default (override with `STARFIRE_RENDER=wgpu`);
/// everywhere else this is always the `wgpu` [`VideoRenderer`].
pub enum ActiveRenderer {
    Wgpu(VideoRenderer),
    #[cfg(target_os = "macos")]
    Metal(metal::MetalRenderer),
    #[cfg(target_os = "windows")]
    D3d11(d3d11::D3d11Renderer),
}

impl ActiveRenderer {
    /// Reconfigure for a new surface size (physical pixels).
    pub fn resize(&mut self, width: u32, height: u32) {
        match self {
            ActiveRenderer::Wgpu(r) => r.resize(width, height),
            #[cfg(target_os = "macos")]
            ActiveRenderer::Metal(r) => r.resize(width, height),
            #[cfg(target_os = "windows")]
            ActiveRenderer::D3d11(r) => r.resize(width, height),
        }
    }
}

impl Renderer for ActiveRenderer {
    fn set_color_mode(&mut self, mode: ColorMode) {
        match self {
            ActiveRenderer::Wgpu(r) => r.set_color_mode(mode),
            #[cfg(target_os = "macos")]
            ActiveRenderer::Metal(r) => r.set_color_mode(mode),
            #[cfg(target_os = "windows")]
            ActiveRenderer::D3d11(r) => r.set_color_mode(mode),
        }
    }
    fn present(&mut self, frame: &VideoFrame) -> Result<(), RenderError> {
        match self {
            ActiveRenderer::Wgpu(r) => r.present(frame),
            #[cfg(target_os = "macos")]
            ActiveRenderer::Metal(r) => r.present(frame),
            #[cfg(target_os = "windows")]
            ActiveRenderer::D3d11(r) => r.present(frame),
        }
    }
}

/// Build the Windows zero-copy D3D11 renderer on the shared decode device. The
/// app uses this (instead of [`new_for_window`]) when the decoder produces D3D11
/// textures. `STARFIRE_VSYNC=0` → immediate present.
#[cfg(target_os = "windows")]
pub fn new_d3d11_for_window<W: winit::raw_window_handle::HasWindowHandle>(
    window: &W,
    shared: starfire_decode::win_device::SharedDevice,
    width: u32,
    height: u32,
) -> Result<ActiveRenderer, RenderError> {
    let vsync = !matches!(
        std::env::var("STARFIRE_VSYNC").ok().as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no")
    );
    d3d11::D3d11Renderer::new(window, shared, width, height, vsync).map(ActiveRenderer::D3d11)
}

/// Build the renderer for `window`. On macOS, the zero-copy Metal backend unless
/// `STARFIRE_RENDER=wgpu`; `STARFIRE_VSYNC=0` makes the Metal path present
/// immediately (lowest latency). Non-macOS always returns the `wgpu` backend, so
/// that path is unchanged.
pub fn new_for_window<W>(window: W, width: u32, height: u32) -> Result<ActiveRenderer, RenderError>
where
    W: wgpu::WindowHandle + 'static,
{
    #[cfg(target_os = "macos")]
    {
        let force_wgpu = std::env::var("STARFIRE_RENDER").ok().as_deref() == Some("wgpu");
        if !force_wgpu {
            let vsync = !matches!(
                std::env::var("STARFIRE_VSYNC").ok().as_deref(),
                Some("0") | Some("false") | Some("off") | Some("no")
            );
            return metal::MetalRenderer::new(&window, width, height, vsync)
                .map(ActiveRenderer::Metal);
        }
    }
    VideoRenderer::new(window, width, height).map(ActiveRenderer::Wgpu)
}
