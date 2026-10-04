// SPDX-License-Identifier: Apache-2.0
//! Windows HEVC/H.264 decode via **Media Foundation** (the OS decoder MFT,
//! DXVA/D3D11-accelerated).
//!
//! # Clean-room provenance
//! Binds directly to the **Media Foundation** decoder MFT + **D3D11** — all
//! first-party Windows system APIs via the MIT/Apache `windows` crate. No ffmpeg
//! (we explicitly skip any `FFmpeg*` MFT), no codec library. Like VideoToolbox on
//! macOS, we drive the OS's own codec through its public API.
//!
//! # Why synchronous
//! On this class of box there is no `HARDWARE`-flagged async HEVC MFT; the OS
//! HEVC decoder (`HEVCVideoExtension`) is a **synchronous** MFT that uses DXVA
//! internally when handed a D3D11 device manager. So we drive it directly:
//! `ProcessInput` one Annex-B access unit, then `ProcessOutput` until it wants
//! more. The decoded NV12 surface is handed up as a D3D11 texture (zero-copy,
//! the default) or copied into CPU planes (`STARFIRE_ZEROCOPY=0`).
//!
//! # Latency
//! Left to itself this MFT keeps **two frames** in hand: the frame submitted
//! by `ProcessInput` N only comes out with input N+2 (measured with
//! `examples/decode_fixture`: every `push` left 2 frames inside the decoder,
//! i.e. +33 ms at 60 fps). A stream with no B-frames needs no such buffer, so
//! we ask for low-latency output (`CODECAPI_AVLowLatencyMode` / `MF_LOW_LATENCY`)
//! and [`push`](crate::Decoder::push) returns the frame it was just given.
//! Each output's `pts` is the frame index of the access unit it came from, so
//! a caller can verify that (lag = submitted index - returned `pts`).
//!
//! # No per-frame copy
//! The decoder is asked (`MF_SA_D3D11_BINDFLAGS`) to allocate its output
//! textures so a shader can sample them, and each frame is then handed up as
//! *that* texture -- the slice of the decoder's own array -- with the sample
//! kept alive until the frame leaves the screen. The alternative, copying every
//! decoded frame into a private texture, costs real GPU time on integrated
//! graphics. Measured on Intel UHD at 1080p with `examples/decode_fixture` and
//! `STARFIRE_DECODE_GPU_WAIT=1` (submit -> picture usable on the GPU, four
//! interleaved pairs): copy 3.43-3.50 ms, direct 2.03-2.05 ms. The copy alone,
//! in isolation, is 0.5 ms at 1080p and 2.6 ms at 4K there (0.04 / 0.11 ms on
//! an RTX 4060), so the win is for integrated-graphics clients. If the decoder
//! or driver does not allow sampling its textures, the copy path is used
//! automatically; `STARFIRE_DECODE_COPY=1` forces it (the A/B baseline).
//!
//! A CPU timer cannot see any of this: `push` returns in ~0.7 ms on either path
//! because it only *queues* GPU work. Use the GPU-wait knob to compare.
#![allow(non_upper_case_globals)]

use std::collections::VecDeque;
use std::ffi::c_void;

use starfire_core::video::{AccessUnit, Codec};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11ShaderResourceView, ID3D11Texture2D,
    D3D11_BIND_DECODER, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_ARRAY_SRV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate, IMFDXGIBuffer,
    IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFTransform, MFCreateDXGIDeviceManager,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown,
    MFStartup, MFTEnumEx, MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_H264_ES,
    MFVideoFormat_HEVC, MFVideoFormat_HEVC_ES, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFSTARTUP_NOSOCKET, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SA_D3D11_BINDFLAGS, MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT,
    MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT_PROGRESSIVE, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

use crate::frame::{ColorSpace, PixelFormat, Plane, VideoFrame};
use crate::{Decoder, DecodeError};

/// Pack two u32s into the UINT64 form MF uses for `MF_MT_FRAME_SIZE` (hi ‖ lo).
fn packed(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn mferr(e: windows::core::Error) -> DecodeError {
    DecodeError::Failed(format!("media foundation: {e}"))
}

/// Sample-time units (100 ns) per frame index. Sample times carry the access
/// unit's frame index through the decoder so each output can be matched to its
/// input; the absolute rate is irrelevant to an in-order, no-B-frame stream.
const TICKS_PER_FRAME: i64 = 100_000;

/// Output samples requested from the decoder when frames are presented straight
/// from its textures: the renderer keeps two frames referenced and the client
/// one more, and the decoder must still have a free one to decode into.
const DIRECT_SPARE_SAMPLES: u32 = 8;

/// Side of the square the GPU-wait instrument reads back out of each frame.
const GPU_WAIT_PROBE: u32 = 16;

/// One step of draining the MFT's output.
enum Step {
    Frame(VideoFrame),
    FormatChanged,
    NeedMoreInput,
}

/// Media Foundation (DXVA) decoder for HEVC / H.264 → NV12.
pub struct MediaFoundationDecoder {
    transform: IMFTransform,
    provides_samples: bool,
    width: u32,
    height: u32,
    out_queue: VecDeque<VideoFrame>,
    /// Whether the MFT accepted the low-latency request (for the startup log).
    low_latency: bool,
    /// Zero-copy: hand the decoded D3D11 NV12 texture up instead of a CPU readback.
    zero_copy: bool,
    /// Hand up the decoder's own texture slice (no GPU copy). `None` until the
    /// first frame shows whether the decoder's textures can be sampled.
    direct: Option<bool>,
    /// Measurement aid (`STARFIRE_DECODE_GPU_WAIT=1`): make `push` wait until the
    /// GPU has finished the frame, so its duration is the real time to a usable
    /// picture instead of the time to queue the work. Holds the event query and
    /// a tiny scratch texture (see [`wait_gpu`](Self::wait_gpu)).
    gpu_wait: Option<(ID3D11Query, ID3D11Texture2D)>,
    /// Owned single-slice NV12 textures the decoder copies each frame into (a ring
    /// so the renderer can hold a frame or two while the decoder writes the next).
    present_ring: Vec<ID3D11Texture2D>,
    ring_idx: usize,
    ring_dims: (u32, u32),
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    // Kept alive for the decoder's lifetime.
    _manager: IMFDXGIDeviceManager,
}

// The COM objects are owned solely by this struct and only touched behind
// `&mut self`; the decoder is moved onto the session thread (trait requires Send).
unsafe impl Send for MediaFoundationDecoder {}

impl MediaFoundationDecoder {
    /// Create + configure the OS decoder MFT for `codec` with its own D3D11
    /// device (for DXVA). Errors [`DecodeError::NoBackend`] if no decoder exists.
    pub fn new(codec: Codec) -> Result<Self, DecodeError> {
        let shared = crate::win_device::SharedDevice::create().map_err(mferr)?;
        Self::with_device(codec, shared)
    }

    /// Create the decoder on a caller-provided shared D3D11 device — so the native
    /// D3D11 renderer can sample the decoded textures on the same device (the
    /// zero-copy windowed path). Headless callers use [`new`](Self::new).
    pub fn with_device(
        codec: Codec,
        shared: crate::win_device::SharedDevice,
    ) -> Result<Self, DecodeError> {
        let subtype = match codec {
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::H264 => MFVideoFormat_H264,
            Codec::Av1 => return Err(DecodeError::UnsupportedCodec(codec)),
        };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).map_err(mferr)?;
            let device = shared.device;
            let context = shared.context;

            // Zero-copy (default on): hand the decoded D3D11 texture up instead of
            // a GPU→CPU readback. STARFIRE_ZEROCOPY=0 forces the CPU-plane path.
            let zero_copy = !matches!(
                std::env::var("STARFIRE_ZEROCOPY").ok().as_deref(),
                Some("0") | Some("false") | Some("off") | Some("no")
            );

            let mut reset_token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager).map_err(mferr)?;
            let manager = manager.ok_or(DecodeError::NoBackend)?;
            manager.ResetDevice(&device, reset_token).map_err(mferr)?;

            // Find the OS decoder MFT (not a HARDWARE-flagged async one — those
            // don't exist for HEVC here; the DXVA-backed sync MFT does).
            let (transform, in_subtype) =
                activate_decoder(subtype).ok_or(DecodeError::NoBackend)?;

            // Hand it the D3D manager to enable DXVA hardware decode (best-effort:
            // if it declines, it falls back to software but still decodes).
            let _ = transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize);

            // Ask for output textures a shader can sample, so frames can be
            // presented straight from the decoder's texture with no copy. Best
            // effort: a decoder that ignores this just gets the copy path.
            let force_copy = matches!(
                std::env::var("STARFIRE_DECODE_COPY").ok().as_deref(),
                Some("1") | Some("true") | Some("on") | Some("yes")
            );
            if zero_copy && !force_copy {
                if let Ok(attrs) = transform.GetOutputStreamAttributes(0) {
                    let _ = attrs.SetUINT32(
                        &MF_SA_D3D11_BINDFLAGS,
                        (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_DECODER.0) as u32,
                    );
                    // Frames stay referenced while they are on screen, so ask
                    // for spare output samples beyond what decoding itself needs.
                    let _ =
                        attrs.SetUINT32(&MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT, DIRECT_SPARE_SAMPLES);
                    let _ = attrs.SetUINT32(
                        &MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT_PROGRESSIVE,
                        DIRECT_SPARE_SAMPLES,
                    );
                }
            }
            if zero_copy && force_copy {
                eprintln!("[mf-decode] present path: copy (STARFIRE_DECODE_COPY)");
            }
            let gpu_wait = if std::env::var("STARFIRE_DECODE_GPU_WAIT").is_ok() {
                let desc = D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                };
                let mut q: Option<ID3D11Query> = None;
                let _ = device.CreateQuery(&desc, Some(&mut q));
                let scratch_desc = D3D11_TEXTURE2D_DESC {
                    Width: GPU_WAIT_PROBE,
                    Height: GPU_WAIT_PROBE,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut scratch: Option<ID3D11Texture2D> = None;
                let _ = device.CreateTexture2D(&scratch_desc, None, Some(&mut scratch));
                q.zip(scratch)
            } else {
                None
            };

            // Low-latency output: emit each frame as soon as it is decoded
            // instead of holding it for reordering (the stream has no B-frames).
            // Must be set before the media types. `STARFIRE_DECODE_BUFFERED=1`
            // skips it -- the A/B baseline for `examples/decode_fixture`.
            let buffered = matches!(
                std::env::var("STARFIRE_DECODE_BUFFERED").ok().as_deref(),
                Some("1") | Some("true") | Some("on") | Some("yes")
            );
            let mut low_latency = false;
            if !buffered {
                if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
                    low_latency |= codec_api
                        .SetValue(
                            &CODECAPI_AVLowLatencyMode,
                            &windows::core::VARIANT::from(true),
                        )
                        .is_ok();
                }
                if let Ok(attrs) = transform.GetAttributes() {
                    low_latency |= attrs.SetUINT32(&MF_LOW_LATENCY, 1).is_ok();
                }
            }
            eprintln!(
                "[mf-decode] low-latency output: {}",
                if low_latency {
                    "on"
                } else if buffered {
                    "off (STARFIRE_DECODE_BUFFERED)"
                } else {
                    "not accepted by this decoder"
                }
            );

            // Input type (HEVC/H.264). Frame size is a placeholder; the decoder
            // derives real dimensions from the stream (via STREAM_CHANGE).
            let in_type = MFCreateMediaType().map_err(mferr)?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(mferr)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &in_subtype).map_err(mferr)?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(mferr)?;
            in_type
                .SetUINT64(&MF_MT_FRAME_SIZE, packed(1920, 1080))
                .map_err(mferr)?;
            transform.SetInputType(0, &in_type, 0).map_err(mferr)?;

            // NOTE: GetOutputStreamInfo crashes inside HEVCVideoExtension — skip it.
            let provides_samples = true;
            let _ = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES;

            let mut dec = Self {
                transform,
                provides_samples,
                width: 0,
                height: 0,
                out_queue: VecDeque::new(),
                low_latency,
                zero_copy,
                direct: if force_copy { Some(false) } else { None },
                gpu_wait,
                present_ring: Vec::new(),
                ring_idx: 0,
                ring_dims: (0, 0),
                device,
                context,
                _manager: manager,
            };
            dec.set_output_nv12()?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(mferr)?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(mferr)?;
            Ok(dec)
        }
    }

    /// Whether the decoder accepted the low-latency request at construction.
    pub fn low_latency(&self) -> bool {
        self.low_latency
    }

    /// Find the MFT's NV12 output type, set it, and latch the frame dimensions.
    fn set_output_nv12(&mut self) -> Result<(), DecodeError> {
        unsafe {
            let mut i = 0u32;
            loop {
                let ty: IMFMediaType = match self.transform.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => return Ok(()), // enumerated all; keep current type
                };
                if ty.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    self.transform.SetOutputType(0, &ty, 0).map_err(mferr)?;
                    if let Ok(fs) = ty.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.width = (fs >> 32) as u32;
                        self.height = (fs & 0xFFFF_FFFF) as u32;
                    }
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    /// Drain all currently-available output frames into the queue.
    fn drain_output(&mut self) -> Result<(), DecodeError> {
        loop {
            match unsafe { self.process_output()? } {
                Step::Frame(f) => self.out_queue.push_back(f),
                Step::FormatChanged => {} // dims refreshed; keep draining
                Step::NeedMoreInput => break,
            }
        }
        Ok(())
    }

    /// One `ProcessOutput` call → a frame, a format change, or "needs more input".
    unsafe fn process_output(&mut self) -> Result<Step, DecodeError> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: if self.provides_samples {
                std::mem::ManuallyDrop::new(None)
            } else {
                std::mem::ManuallyDrop::new(Some(self.alloc_output_sample()?))
            },
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
        match r {
            Ok(()) => {
                let sample =
                    sample.ok_or(DecodeError::Failed("MFT returned no output sample".into()))?;
                Ok(Step::Frame(self.sample_to_frame(&sample)?))
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Step::NeedMoreInput),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.set_output_nv12()?;
                Ok(Step::FormatChanged)
            }
            Err(e) => Err(mferr(e)),
        }
    }

    /// Allocate a system-memory output sample sized to the current frame (only
    /// used when the MFT doesn't provide its own samples).
    unsafe fn alloc_output_sample(&self) -> Result<IMFSample, DecodeError> {
        let size = ((self.width.max(2) * self.height.max(2)) * 3 / 2).max(64);
        let buffer = MFCreateMemoryBuffer(size).map_err(mferr)?;
        let sample = MFCreateSample().map_err(mferr)?;
        sample.AddBuffer(&buffer).map_err(mferr)?;
        Ok(sample)
    }

    /// Ensure the present-texture ring exists and matches the current dimensions.
    unsafe fn ensure_present_ring(&mut self) -> Result<(), DecodeError> {
        if !self.present_ring.is_empty() && self.ring_dims == (self.width, self.height) {
            return Ok(());
        }
        self.present_ring.clear();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width.max(2),
            Height: self.height.max(2),
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..4 {
            let mut t: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .map_err(mferr)?;
            self.present_ring
                .push(t.ok_or(DecodeError::Failed("CreateTexture2D returned null".into()))?);
        }
        self.ring_dims = (self.width, self.height);
        self.ring_idx = 0;
        Ok(())
    }

    /// Zero-copy: pull the decoded D3D11 NV12 texture out of the MFT output sample
    /// and copy it (on-GPU, no CPU readback) into an owned single-slice texture the
    /// renderer can sample directly.
    unsafe fn sample_to_frame_zerocopy(
        &mut self,
        sample: &IMFSample,
    ) -> Result<VideoFrame, DecodeError> {
        let buffer = sample.GetBufferByIndex(0).map_err(mferr)?;
        let dxgi: IMFDXGIBuffer = buffer.cast().map_err(mferr)?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).map_err(mferr)?;
        if raw.is_null() {
            return Err(DecodeError::Failed("MFT output is not a D3D11 texture".into()));
        }
        let src = ID3D11Texture2D::from_raw(raw);
        let subresource = dxgi.GetSubresourceIndex().map_err(mferr)?;

        // Direct path: present the decoder's own texture slice. Decided once,
        // on the first frame, by actually building a shader view on it.
        let direct = match self.direct {
            Some(d) => d,
            None => {
                let ok = self.can_sample(&src, subresource);
                eprintln!(
                    "[mf-decode] present path: {}",
                    if ok {
                        "direct (decoder texture, no copy)"
                    } else {
                        "copy (decoder textures are not shader-readable here)"
                    }
                );
                self.direct = Some(ok);
                ok
            }
        };
        if direct {
            return Ok(VideoFrame {
                width: self.width,
                height: self.height,
                format: PixelFormat::Nv12,
                color_space: ColorSpace::Bt709Limited,
                pts: sample_frame_index(sample),
                planes: Vec::new(),
                native_d3d11: Some(crate::frame::native_win::D3d11Frame {
                    texture: src,
                    slice: subresource,
                    // The decoder must not reuse this slice while it is on screen.
                    keep_alive: Some(sample.clone()),
                }),
            });
        }

        self.ensure_present_ring()?;
        self.ring_idx = (self.ring_idx + 1) % self.present_ring.len();
        let dest = self.present_ring[self.ring_idx].clone();
        // GPU→GPU copy of the decoded slice into our owned texture; decouples us
        // from the MFT's array recycling and gives the renderer one clean texture.
        self.context
            .CopySubresourceRegion(&dest, 0, 0, 0, 0, &src, subresource, None);

        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Bt709Limited,
            pts: sample_frame_index(sample),
            planes: Vec::new(),
            native_d3d11: Some(crate::frame::native_win::D3d11Frame {
                texture: dest,
                slice: 0,
                keep_alive: None,
            }),
        })
    }

    /// Whether `tex` (slice `slice`) can be bound as a shader resource -- tried
    /// for real, because a decoder may ignore the bind-flag request and a driver
    /// may refuse views on decoder arrays.
    unsafe fn can_sample(&self, tex: &ID3D11Texture2D, slice: u32) -> bool {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0 {
            return false;
        }
        let view = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8_UNORM,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: slice,
                    ArraySize: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        self.device
            .CreateShaderResourceView(tex, Some(&view), Some(&mut srv))
            .is_ok()
            && srv.is_some()
    }

    /// Block until `frame`'s picture is really there (measurement aid; no-op
    /// unless `STARFIRE_DECODE_GPU_WAIT` is set).
    ///
    /// An event query alone is not enough: video decode runs on its own GPU
    /// engine, and the query only covers the engine that executes draws and
    /// copies. So first read a few pixels of the frame on that engine -- which
    /// cannot run until the decode (and any copy into the frame) has finished --
    /// and wait for *that*.
    unsafe fn wait_gpu(&self, frame: &VideoFrame) {
        let (Some((query, scratch)), Some(native)) = (&self.gpu_wait, &frame.native_d3d11) else {
            return;
        };
        let probe = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: GPU_WAIT_PROBE,
            bottom: GPU_WAIT_PROBE,
            back: 1,
        };
        self.context.CopySubresourceRegion(
            scratch,
            0,
            0,
            0,
            0,
            &native.texture,
            native.slice,
            Some(&probe),
        );
        self.context.End(query);
        self.context.Flush();
        let mut done = windows::Win32::Foundation::BOOL(0);
        loop {
            let _ = self.context.GetData(
                query,
                Some(&mut done as *mut windows::Win32::Foundation::BOOL as *mut c_void),
                std::mem::size_of::<windows::Win32::Foundation::BOOL>() as u32,
                0,
            );
            if done.as_bool() {
                return;
            }
            std::hint::spin_loop();
        }
    }

    /// Copy a decoded NV12 surface into a portable [`VideoFrame`] (a
    /// stride-preserving CPU readback via `IMF2DBuffer::Lock2D`).
    unsafe fn sample_to_frame(&mut self, sample: &IMFSample) -> Result<VideoFrame, DecodeError> {
        if self.zero_copy {
            return self.sample_to_frame_zerocopy(sample);
        }
        let buffer = sample.GetBufferByIndex(0).map_err(mferr)?;
        let two_d: IMF2DBuffer = buffer.cast().map_err(mferr)?;
        let mut scan0: *mut u8 = std::ptr::null_mut();
        let mut pitch: i32 = 0;
        two_d.Lock2D(&mut scan0, &mut pitch).map_err(mferr)?;
        let stride = pitch.unsigned_abs() as usize;
        let h = self.height.max(1) as usize;
        let ch = h.div_ceil(2);

        // NV12 in a single surface: Y plane (h rows), then interleaved CbCr
        // (h/2 rows), both at `stride`.
        let mut y = vec![0u8; stride * h];
        std::ptr::copy_nonoverlapping(scan0, y.as_mut_ptr(), stride * h);
        let mut cbcr = vec![0u8; stride * ch];
        std::ptr::copy_nonoverlapping(scan0.add(stride * h), cbcr.as_mut_ptr(), stride * ch);
        let _ = two_d.Unlock2D();

        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            format: PixelFormat::Nv12,
            color_space: ColorSpace::Bt709Limited,
            pts: sample_frame_index(sample),
            planes: vec![Plane::new(y, stride), Plane::new(cbcr, stride)],
            native_d3d11: None,
        })
    }
}

impl Decoder for MediaFoundationDecoder {
    fn push(&mut self, au: &AccessUnit) -> Result<Option<VideoFrame>, DecodeError> {
        let sample = unsafe { make_input_sample(&au.data, au.frame_index as i64)? };
        unsafe {
            // A sync MFT may refuse input while output is pending — drain, then feed.
            if let Err(e) = self.transform.ProcessInput(0, &sample, 0) {
                self.drain_output()?;
                self.transform.ProcessInput(0, &sample, 0).map_err(|_| mferr(e))?;
            }
        }
        self.drain_output()?;
        if let Some(frame) = self.out_queue.back() {
            unsafe { self.wait_gpu(frame) };
        }
        // Hand back the NEWEST decoded frame. Normally there is exactly one; if
        // the decoder ever releases several at once, showing an older one would
        // put the picture permanently behind, so the stale ones are dropped.
        let newest = self.out_queue.pop_back();
        self.out_queue.clear();
        Ok(newest)
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, DecodeError> {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        let _ = self.drain_output();
        Ok(self.out_queue.drain(..).collect())
    }
}

impl Drop for MediaFoundationDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Wrap an Annex-B access unit in an `IMFSample` (copy into a memory buffer).
unsafe fn make_input_sample(data: &[u8], frame_index: i64) -> Result<IMFSample, DecodeError> {
    let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(mferr)?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buffer.Lock(&mut ptr, None, None).map_err(mferr)?;
    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    buffer.Unlock().map_err(mferr)?;
    buffer.SetCurrentLength(data.len() as u32).map_err(mferr)?;
    let sample = MFCreateSample().map_err(mferr)?;
    sample.AddBuffer(&buffer).map_err(mferr)?;
    sample
        .SetSampleTime(frame_index * TICKS_PER_FRAME)
        .map_err(mferr)?;
    sample.SetSampleDuration(TICKS_PER_FRAME).map_err(mferr)?;
    Ok(sample)
}

/// The frame index an output sample was decoded from (carried in its sample
/// time by [`make_input_sample`]); `-1` if the decoder dropped the timestamp.
unsafe fn sample_frame_index(sample: &IMFSample) -> i64 {
    match sample.GetSampleTime() {
        Ok(t) => (t + TICKS_PER_FRAME / 2).div_euclid(TICKS_PER_FRAME),
        Err(_) => -1,
    }
}

/// Enumerate OS decoder MFTs accepting `subtype` (HEVC or HEVC_ES, etc.) and
/// activate the first non-ffmpeg one. Returns the transform + the input subtype
/// it registered under. `None` if no permissive decoder exists.
unsafe fn activate_decoder(subtype: GUID) -> Option<(IMFTransform, GUID)> {
    let candidates: &[GUID] = if subtype == MFVideoFormat_HEVC {
        &[MFVideoFormat_HEVC, MFVideoFormat_HEVC_ES]
    } else {
        &[MFVideoFormat_H264, MFVideoFormat_H264_ES]
    };
    for &cand in candidates {
        let info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: cand,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        let ok = MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&info as *const _),
            None,
            &mut activates,
            &mut count,
        );
        if ok.is_err() || activates.is_null() || count == 0 {
            continue;
        }
        let slice = std::slice::from_raw_parts(activates, count as usize);
        let mut found = None;
        for act in slice.iter().flatten() {
            // Skip any ffmpeg-backed MFT (GPL/LGPL) — permissive system codecs only.
            let mut name = PWSTR::null();
            let mut len = 0u32;
            if act
                .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
                .is_ok()
            {
                let nm = name.to_string().unwrap_or_default();
                CoTaskMemFree(Some(name.0 as *const _));
                if nm.to_lowercase().contains("ffmpeg") {
                    continue;
                }
                eprintln!("[mf-decode] using decoder MFT: {nm}");
            }
            if let Ok(t) = act.ActivateObject::<IMFTransform>() {
                found = Some((t, cand));
                break;
            }
        }
        CoTaskMemFree(Some(activates as *const _));
        if found.is_some() {
            return found;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder instantiates against the real OS HEVC MFT on this machine.
    /// `#[ignore]` so CI without a decoder doesn't fail; run with `--ignored`.
    #[test]
    #[ignore]
    fn constructs_hevc_decoder() {
        match MediaFoundationDecoder::new(Codec::Hevc) {
            Ok(_) => {}
            Err(e) => panic!("decoder construction failed: {e:?}"),
        }
    }

    #[test]
    fn av1_is_unsupported() {
        assert!(matches!(
            MediaFoundationDecoder::new(Codec::Av1),
            Err(DecodeError::UnsupportedCodec(Codec::Av1))
        ));
    }
}
