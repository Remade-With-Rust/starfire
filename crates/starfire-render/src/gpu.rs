// SPDX-License-Identifier: Apache-2.0
//! wgpu YUV→RGB upload + draw pipeline, shared by window present and the
//! headless test.
//!
//! Clean-room: built entirely on the public `wgpu` API (MIT/Apache-2.0) and a
//! first-party WGSL shader (`yuv.wgsl`). No Moonlight, no ffmpeg, no native
//! Metal/D3D shader source.

use bytemuck::{Pod, Zeroable};
use starfire_decode::{PixelFormat, VideoFrame};
use wgpu::util::DeviceExt;

use crate::RenderError;

/// Uniform handed to the fragment shader: layout + range selector.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    format: u32,     // 0 = NV12, 1 = I420
    full_range: u32, // 0 = limited, 1 = full
    _pad0: u32,
    _pad1: u32,
}

/// The YUV→RGB GPU pipeline: a sampler, a bind-group layout, and the render
/// pipeline. Per-frame textures are (re)created to match the frame's size.
pub struct YuvPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Format the pipeline's color target was built for.
    target_format: wgpu::TextureFormat,
}

impl YuvPipeline {
    /// Build the pipeline for a given output color format (swapchain or
    /// offscreen target). `Rgba8UnormSrgb`/`Bgra8UnormSrgb` for display;
    /// `Rgba8Unorm` for deterministic readback in tests.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv-to-rgb"),
            source: wgpu::ShaderSource::Wgsl(include_str!("yuv.wgsl").into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("yuv-bind-layout"),
                entries: &[
                    // sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    // y / u / v textures
                    tex_entry(1),
                    tex_entry(2),
                    tex_entry(3),
                    // params uniform
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("yuv-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            target_format,
        }
    }

    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    /// Upload `frame`'s planes to GPU textures and build the bind group for one
    /// draw. Textures are sized to the frame; chroma is half-res.
    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &VideoFrame,
    ) -> Result<wgpu::BindGroup, RenderError> {
        frame
            .validate()
            .map_err(|e| RenderError::Failed(format!("invalid frame: {e}")))?;

        let full_w = frame.width;
        let full_h = frame.height;
        let cw = full_w.div_ceil(2);
        let ch = full_h.div_ceil(2);

        // Y plane: R8.
        let y_view = self.make_texture(
            device,
            queue,
            wgpu::TextureFormat::R8Unorm,
            full_w,
            full_h,
            &frame.planes[0].data,
            frame.planes[0].stride,
            1,
        );

        let (u_view, v_view) = match frame.format {
            PixelFormat::Nv12 => {
                // Interleaved CbCr as RG8 at half-res.
                let uv = self.make_texture(
                    device,
                    queue,
                    wgpu::TextureFormat::Rg8Unorm,
                    cw,
                    ch,
                    &frame.planes[1].data,
                    frame.planes[1].stride,
                    2,
                );
                // v_tex is unused for NV12; bind a 1x1 dummy.
                let dummy = self.make_texture(
                    device,
                    queue,
                    wgpu::TextureFormat::R8Unorm,
                    1,
                    1,
                    &[128],
                    1,
                    1,
                );
                (uv, dummy)
            }
            PixelFormat::I420 => {
                let u = self.make_texture(
                    device,
                    queue,
                    wgpu::TextureFormat::R8Unorm,
                    cw,
                    ch,
                    &frame.planes[1].data,
                    frame.planes[1].stride,
                    1,
                );
                let v = self.make_texture(
                    device,
                    queue,
                    wgpu::TextureFormat::R8Unorm,
                    cw,
                    ch,
                    &frame.planes[2].data,
                    frame.planes[2].stride,
                    1,
                );
                (u, v)
            }
        };

        let params = Params {
            format: match frame.format {
                PixelFormat::Nv12 => 0,
                PixelFormat::I420 => 1,
            },
            full_range: u32::from(matches!(
                frame.color_space,
                starfire_decode::ColorSpace::Bt709Full
            )),
            _pad0: 0,
            _pad1: 0,
        };
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("yuv-params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&u_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });
        Ok(bind_group)
    }

    /// Create a 2D texture, upload `src` honoring `stride`, and return a view.
    /// `bytes_per_sample` is 1 (R8) or 2 (RG8).
    #[allow(clippy::too_many_arguments)]
    fn make_texture(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        src: &[u8],
        src_stride: usize,
        bytes_per_sample: usize,
    ) -> wgpu::TextureView {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("yuv-plane"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // wgpu's write_texture requires the source rows be tightly packed at the
        // given bytes_per_row, so repack if the plane stride has padding.
        let row_bytes = width as usize * bytes_per_sample;
        let packed: Vec<u8> = if src_stride == row_bytes {
            // Still slice to exactly height rows in case the buffer is larger.
            src[..row_bytes * height as usize].to_vec()
        } else {
            let mut out = Vec::with_capacity(row_bytes * height as usize);
            for row in 0..height as usize {
                let start = row * src_stride;
                out.extend_from_slice(&src[start..start + row_bytes]);
            }
            out
        };

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &packed,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_bytes as u32),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Draw `frame` into `target_view` (swapchain or offscreen). The caller owns
    /// submission ordering; this records one render pass.
    pub fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        frame: &VideoFrame,
    ) -> Result<(), RenderError> {
        let bind_group = self.upload(device, queue, frame)?;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("yuv-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1); // fullscreen triangle
        Ok(())
    }
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Request a wgpu device/queue with no surface (headless). Used by the offscreen
/// test and as the basis for the windowed renderer.
pub fn request_headless_device() -> Result<(wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue), RenderError>
{
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .map_err(|e| RenderError::Failed(format!("no suitable GPU adapter: {e}")))?;

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("starfire-render-device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| RenderError::Failed(format!("request_device failed: {e}")))?;

    Ok((instance, adapter, device, queue))
}

/// Render `frame` to an offscreen RGBA8 texture and read the pixels back to CPU.
/// Returns `(width, height, rgba_bytes)` with tightly-packed rows. This is the
/// testable core of the YUV→RGB conversion.
pub fn render_to_rgba(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &YuvPipeline,
    frame: &VideoFrame,
    out_w: u32,
    out_h: u32,
) -> Result<(u32, u32, Vec<u8>), RenderError> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen-rgba"),
        size: wgpu::Extent3d {
            width: out_w,
            height: out_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: pipeline.target_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("offscreen") });
    pipeline.draw(device, queue, &mut encoder, &view, frame)?;

    // Copy the target into a buffer; wgpu requires bytes_per_row aligned to 256.
    let unpadded = out_w as usize * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
    let padded = unpadded.div_ceil(align) * align;
    let buf_size = (padded * out_h as usize) as u64;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: buf_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(out_h),
            },
        },
        wgpu::Extent3d {
            width: out_w,
            height: out_h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    // Map and copy out the unpadded RGBA rows.
    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).ok();
    rx.recv()
        .map_err(|_| RenderError::Failed("readback channel closed".into()))?
        .map_err(|e| RenderError::Failed(format!("buffer map failed: {e:?}")))?;

    let mapped = slice.get_mapped_range();
    let mut out = Vec::with_capacity(unpadded * out_h as usize);
    for row in 0..out_h as usize {
        let start = row * padded;
        out.extend_from_slice(&mapped[start..start + unpadded]);
    }
    drop(mapped);
    readback.unmap();

    Ok((out_w, out_h, out))
}
