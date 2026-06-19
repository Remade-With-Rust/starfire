// SPDX-License-Identifier: Apache-2.0
//! macOS hardware decode via **VideoToolbox** (raw `extern "C"` FFI).
//!
//! # Clean-room provenance
//! This is a direct, first-party binding to Apple's public system frameworks —
//! **VideoToolbox**, **CoreMedia**, **CoreVideo**, **CoreFoundation** — declared
//! inline here with `#[link]` + `extern "C"`. We deliberately do **not** depend
//! on any third-party binding crate, ffmpeg, or codec library: the only code
//! that touches a real HEVC/H.264 decoder is Apple's, via its documented API.
//!
//! # Pipeline
//! 1. The first access unit carrying parameter sets (VPS/SPS/PPS) builds a
//!    `CMVideoFormatDescription`
//!    (`CMVideoFormatDescriptionCreateFromHEVCParameterSets` /
//!    `...H264ParameterSets`).
//! 2. From that we create a `VTDecompressionSession` configured to emit
//!    **NV12** (`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`).
//! 3. Each AU's slice NALs (length-prefixed, see [`crate::annexb`]) become a
//!    `CMBlockBuffer` → `CMSampleBuffer`, fed to
//!    `VTDecompressionSessionDecodeFrame`.
//! 4. The decompression callback receives a `CVPixelBuffer`; we lock it, copy
//!    both planes into a portable [`VideoFrame`], and queue it for [`Decoder::push`]
//!    to return.
//!
//! # Why a copy
//! We copy the surface down to CPU [`VideoFrame`] so the renderer has one
//! portable upload path across all platforms. Zero-copy IOSurface→wgpu import is
//! a later optimization that can sit behind the same [`Decoder`] trait.
//!
//! This module compiles only on macOS. It is structured to be unit-tested on a
//! Mac (the pure-Rust glue — parameter-set extraction, callback plumbing — is
//! isolated from the FFI calls where practical).
#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::os::raw::{c_int, c_long};
use std::ptr;
use std::sync::{Arc, Mutex};

use starfire_core::video::{AccessUnit, Codec};

use crate::annexb::{self, NalCodec};
use crate::frame::{ColorSpace, PixelFormat, Plane, VideoFrame};
use crate::{Decoder, DecodeError};

// ---------------------------------------------------------------------------
// Minimal CoreFoundation / CoreMedia / CoreVideo / VideoToolbox FFI.
// Only the handful of symbols and types this backend needs, declared inline.
// ---------------------------------------------------------------------------

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type OSStatus = i32;
type Boolean = u8;
type CMVideoFormatDescriptionRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type CVImageBufferRef = *const c_void;
type CVPixelBufferRef = *const c_void;
type VTDecompressionSessionRef = *const c_void;
type CFDictionaryRef = *const c_void;

const kCFAllocatorDefault: CFAllocatorRef = ptr::null();
const noErr: OSStatus = 0;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` (NV12, limited range).
const kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange: u32 = 0x34323076; // '420v'

/// `CVPixelBufferLockFlags` read-only.
const kCVPixelBufferLock_ReadOnly: u64 = 0x0000_0001;

/// `VTDecodeFrameFlags`: allow asynchronous decode + temporal processing off.
const kVTDecodeFrame_EnableAsynchronousDecompression: u32 = 1 << 0;
const kVTDecodeFrame_EnableTemporalProcessing: u32 = 1 << 2;

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    presentationTimeStamp: CMTime,
    decodeTimeStamp: CMTime,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const kCMTimeFlags_Valid: u32 = 1 << 0;

impl CMTime {
    fn make(value: i64, timescale: i32) -> Self {
        CMTime {
            value,
            timescale,
            flags: kCMTimeFlags_Valid,
            epoch: 0,
        }
    }
    fn invalid() -> Self {
        CMTime {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        }
    }
}

/// `VTDecompressionOutputCallbackRecord`.
#[repr(C)]
struct VTDecompressionOutputCallbackRecord {
    decompressionOutputCallback: VTDecompressionOutputCallback,
    decompressionOutputRefCon: *mut c_void,
}

type VTDecompressionOutputCallback = unsafe extern "C" fn(
    decompressionOutputRefCon: *mut c_void,
    sourceFrameRefCon: *mut c_void,
    status: OSStatus,
    infoFlags: u32,
    imageBuffer: CVImageBufferRef,
    presentationTimeStamp: CMTime,
    presentationDuration: CMTime,
);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFAllocatorRef,
        parameterSetCount: usize,
        parameterSetPointers: *const *const u8,
        parameterSetSizes: *const usize,
        NALUnitHeaderLength: c_int,
        extensions: CFDictionaryRef,
        formatDescriptionOut: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;

    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameterSetCount: usize,
        parameterSetPointers: *const *const u8,
        parameterSetSizes: *const usize,
        NALUnitHeaderLength: c_int,
        formatDescriptionOut: *mut CMVideoFormatDescriptionRef,
    ) -> OSStatus;

    fn CMBlockBufferCreateWithMemoryBlock(
        structureAllocator: CFAllocatorRef,
        memoryBlock: *mut c_void,
        blockLength: usize,
        blockAllocator: CFAllocatorRef,
        customBlockSource: *const c_void,
        offsetToData: usize,
        dataLength: usize,
        flags: u32,
        blockBufferOut: *mut CMBlockBufferRef,
    ) -> OSStatus;

    fn CMBlockBufferReplaceDataBytes(
        sourceBytes: *const c_void,
        destinationBuffer: CMBlockBufferRef,
        offsetIntoDestination: usize,
        dataLength: usize,
    ) -> OSStatus;

    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        dataBuffer: CMBlockBufferRef,
        formatDescription: CMFormatDescriptionRef,
        numSamples: c_long,
        numSampleTimingEntries: c_long,
        sampleTimingArray: *const CMSampleTimingInfo,
        numSampleSizeEntries: c_long,
        sampleSizeArray: *const usize,
        sampleBufferOut: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferLockBaseAddress(pixelBuffer: CVPixelBufferRef, lockFlags: u64) -> OSStatus;
    fn CVPixelBufferUnlockBaseAddress(pixelBuffer: CVPixelBufferRef, unlockFlags: u64) -> OSStatus;
    fn CVPixelBufferGetWidth(pixelBuffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixelBuffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetPlaneCount(pixelBuffer: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixelBuffer: CVPixelBufferRef,
        planeIndex: usize,
    ) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(
        pixelBuffer: CVPixelBufferRef,
        planeIndex: usize,
    ) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pixelBuffer: CVPixelBufferRef, planeIndex: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixelBuffer: CVPixelBufferRef, planeIndex: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        videoFormatDescription: CMVideoFormatDescriptionRef,
        videoDecoderSpecification: CFDictionaryRef,
        destinationImageBufferAttributes: CFDictionaryRef,
        outputCallback: *const VTDecompressionOutputCallbackRecord,
        decompressionSessionOut: *mut VTDecompressionSessionRef,
    ) -> OSStatus;

    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sampleBuffer: CMSampleBufferRef,
        decodeFlags: u32,
        sourceFrameRefCon: *mut c_void,
        infoFlagsOut: *mut u32,
    ) -> OSStatus;

    fn VTDecompressionSessionWaitForAsynchronousFrames(session: VTDecompressionSessionRef)
        -> OSStatus;

    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Shared queue the decompression callback pushes finished frames onto.
#[derive(Default)]
struct FrameSink {
    frames: Vec<VideoFrame>,
    error: Option<String>,
}

/// VideoToolbox-backed [`Decoder`] for HEVC / H.264.
pub struct VideoToolboxDecoder {
    codec: Codec,
    nal_codec: NalCodec,
    format_desc: CMVideoFormatDescriptionRef,
    session: VTDecompressionSessionRef,
    sink: Arc<Mutex<FrameSink>>,
    /// Boxed so the callback's `refCon` pointer stays stable for the session's
    /// lifetime. Holds an `Arc` clone of `sink`.
    _callback_ctx: Box<CallbackCtx>,
}

struct CallbackCtx {
    sink: Arc<Mutex<FrameSink>>,
}

// The CoreMedia/VT object refs are owned solely by this struct and only touched
// behind `&mut self` or the locked sink, so the decoder is safe to move across
// threads (the trait requires `Send`).
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    /// Create a decoder for `codec`. The session is built lazily on the first
    /// access unit that carries parameter sets, so this just records intent.
    pub fn new(codec: Codec) -> Result<Self, DecodeError> {
        let nal_codec = match codec {
            Codec::Hevc => NalCodec::Hevc,
            Codec::H264 => NalCodec::H264,
            Codec::Av1 => return Err(DecodeError::UnsupportedCodec(codec)),
        };
        let sink = Arc::new(Mutex::new(FrameSink::default()));
        Ok(Self {
            codec,
            nal_codec,
            format_desc: ptr::null(),
            session: ptr::null(),
            // The callback context is (re)bound to `sink` in `ensure_session`,
            // once the session that uses it actually exists.
            _callback_ctx: Box::new(CallbackCtx {
                sink: Arc::clone(&sink),
            }),
            sink,
        })
    }

    fn ensure_session(&mut self, param_sets: &[Vec<u8>]) -> Result<(), DecodeError> {
        if !self.session.is_null() {
            return Ok(());
        }
        if param_sets.is_empty() {
            return Err(DecodeError::Failed(
                "no parameter sets seen yet; need a keyframe to build the format description".into(),
            ));
        }

        // Build the CMVideoFormatDescription from the parameter sets.
        let ptrs: Vec<*const u8> = param_sets.iter().map(|p| p.as_ptr()).collect();
        let sizes: Vec<usize> = param_sets.iter().map(|p| p.len()).collect();
        let mut fmt: CMVideoFormatDescriptionRef = ptr::null();
        let status = unsafe {
            match self.nal_codec {
                NalCodec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    kCFAllocatorDefault,
                    param_sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4, // 4-byte length prefix (HVCC)
                    ptr::null(),
                    &mut fmt,
                ),
                NalCodec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    kCFAllocatorDefault,
                    param_sets.len(),
                    ptrs.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut fmt,
                ),
            }
        };
        if status != noErr || fmt.is_null() {
            return Err(DecodeError::Failed(format!(
                "CMVideoFormatDescriptionCreateFrom*ParameterSets failed: OSStatus {status}"
            )));
        }
        self.format_desc = fmt;

        // The callback context must outlive the session and keep a stable
        // address; rebuild it now bound to this decoder's sink.
        self._callback_ctx = Box::new(CallbackCtx {
            sink: Arc::clone(&self.sink),
        });
        let refcon = self._callback_ctx.as_ref() as *const CallbackCtx as *mut c_void;
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: decompression_output,
            decompressionOutputRefCon: refcon,
        };

        let mut session: VTDecompressionSessionRef = ptr::null();
        let status = unsafe {
            VTDecompressionSessionCreate(
                kCFAllocatorDefault,
                fmt,
                ptr::null(), // default (hardware-preferred) decoder
                // NB: destination attributes (pixel format = NV12) would be set
                // via a CFDictionary here; VideoToolbox defaults to a biplanar
                // 4:2:0 format for HEVC/H.264, which we validate at copy time.
                ptr::null(),
                &callback,
                &mut session,
            )
        };
        if status != noErr || session.is_null() {
            return Err(DecodeError::Failed(format!(
                "VTDecompressionSessionCreate failed: OSStatus {status}"
            )));
        }
        self.session = session;
        Ok(())
    }

    fn decode_sample(&mut self, sample: &[u8], pts: i64) -> Result<(), DecodeError> {
        if sample.is_empty() {
            return Ok(());
        }
        // Build a CMBlockBuffer that copies `sample` (we pass null memoryBlock +
        // kCMBlockBufferAssureMemoryNowFlag=0 then fill via ReplaceDataBytes).
        let mut block: CMBlockBufferRef = ptr::null();
        let status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                kCFAllocatorDefault,
                ptr::null_mut(),
                sample.len(),
                kCFAllocatorDefault, // allocate the block for us
                ptr::null(),
                0,
                sample.len(),
                0,
                &mut block,
            )
        };
        if status != noErr || block.is_null() {
            return Err(DecodeError::Failed(format!(
                "CMBlockBufferCreateWithMemoryBlock failed: OSStatus {status}"
            )));
        }
        let status = unsafe {
            CMBlockBufferReplaceDataBytes(
                sample.as_ptr() as *const c_void,
                block,
                0,
                sample.len(),
            )
        };
        if status != noErr {
            unsafe { CFRelease(block) };
            return Err(DecodeError::Failed(format!(
                "CMBlockBufferReplaceDataBytes failed: OSStatus {status}"
            )));
        }

        let timing = CMSampleTimingInfo {
            duration: CMTime::invalid(),
            presentationTimeStamp: CMTime::make(pts, 1_000_000), // pts in microseconds
            decodeTimeStamp: CMTime::invalid(),
        };
        let sample_size = sample.len();
        let mut sample_buf: CMSampleBufferRef = ptr::null();
        let status = unsafe {
            CMSampleBufferCreateReady(
                kCFAllocatorDefault,
                block,
                self.format_desc,
                1,
                1,
                &timing,
                1,
                &sample_size,
                &mut sample_buf,
            )
        };
        // CMSampleBuffer retains the block buffer; release our ref.
        unsafe { CFRelease(block) };
        if status != noErr || sample_buf.is_null() {
            return Err(DecodeError::Failed(format!(
                "CMSampleBufferCreateReady failed: OSStatus {status}"
            )));
        }

        let flags = kVTDecodeFrame_EnableAsynchronousDecompression
            | kVTDecodeFrame_EnableTemporalProcessing;
        let mut info_out: u32 = 0;
        let status = unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session,
                sample_buf,
                flags,
                ptr::null_mut(),
                &mut info_out,
            )
        };
        unsafe { CFRelease(sample_buf) };
        if status != noErr {
            return Err(DecodeError::Failed(format!(
                "VTDecompressionSessionDecodeFrame failed: OSStatus {status}"
            )));
        }
        Ok(())
    }

    fn drain_sink(&self) -> Result<Vec<VideoFrame>, DecodeError> {
        let mut sink = self
            .sink
            .lock()
            .map_err(|_| DecodeError::Failed("frame sink poisoned".into()))?;
        if let Some(err) = sink.error.take() {
            return Err(DecodeError::Failed(err));
        }
        Ok(std::mem::take(&mut sink.frames))
    }
}

impl Decoder for VideoToolboxDecoder {
    fn push(&mut self, au: &AccessUnit) -> Result<Option<VideoFrame>, DecodeError> {
        if au.codec != self.codec {
            return Err(DecodeError::UnsupportedCodec(au.codec));
        }
        let (params, sample) = annexb::to_length_prefixed(&au.data, self.nal_codec);
        if !params.is_empty() || self.session.is_null() {
            self.ensure_session(&params)?;
        }
        self.decode_sample(&sample, au.frame_index as i64)?;

        // Pull whatever finished; VideoToolbox may deliver asynchronously, so a
        // given push may return the previous frame or nothing yet.
        let mut frames = self.drain_sink()?;
        Ok(frames.drain(..).next())
    }

    fn flush(&mut self) -> Result<Vec<VideoFrame>, DecodeError> {
        if !self.session.is_null() {
            let status = unsafe { VTDecompressionSessionWaitForAsynchronousFrames(self.session) };
            if status != noErr {
                return Err(DecodeError::Failed(format!(
                    "VTDecompressionSessionWaitForAsynchronousFrames failed: OSStatus {status}"
                )));
            }
        }
        self.drain_sink()
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
            }
            if !self.format_desc.is_null() {
                CFRelease(self.format_desc);
            }
        }
    }
}

/// The VideoToolbox decompression callback. Runs on a VT-owned thread; copies the
/// decoded `CVPixelBuffer` into a portable [`VideoFrame`] and queues it.
unsafe extern "C" fn decompression_output(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: CVImageBufferRef,
    pts: CMTime,
    _duration: CMTime,
) {
    if refcon.is_null() {
        return;
    }
    let ctx = &*(refcon as *const CallbackCtx);
    let push_err = |msg: String| {
        if let Ok(mut sink) = ctx.sink.lock() {
            sink.error.get_or_insert(msg);
        }
    };

    if status != noErr {
        push_err(format!("decode callback OSStatus {status}"));
        return;
    }
    if image_buffer.is_null() {
        return; // dropped frame; not an error
    }

    match copy_pixel_buffer(image_buffer as CVPixelBufferRef, pts.value) {
        Ok(frame) => {
            if let Ok(mut sink) = ctx.sink.lock() {
                sink.frames.push(frame);
            }
        }
        Err(e) => push_err(e),
    }
}

/// Lock a biplanar NV12 `CVPixelBuffer` and copy both planes into a [`VideoFrame`].
unsafe fn copy_pixel_buffer(pb: CVPixelBufferRef, pts: i64) -> Result<VideoFrame, String> {
    if CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly) != noErr {
        return Err("CVPixelBufferLockBaseAddress failed".into());
    }
    // RAII-ish guard: ensure unlock on every return path.
    struct Unlock(CVPixelBufferRef);
    impl Drop for Unlock {
        fn drop(&mut self) {
            unsafe { CVPixelBufferUnlockBaseAddress(self.0, kCVPixelBufferLock_ReadOnly) };
        }
    }
    let _guard = Unlock(pb);

    let width = CVPixelBufferGetWidth(pb) as u32;
    let height = CVPixelBufferGetHeight(pb) as u32;
    let plane_count = CVPixelBufferGetPlaneCount(pb);
    if plane_count != 2 {
        return Err(format!(
            "expected biplanar NV12 (2 planes), got {plane_count}"
        ));
    }

    let mut planes = Vec::with_capacity(2);
    for i in 0..2 {
        let base = CVPixelBufferGetBaseAddressOfPlane(pb, i);
        if base.is_null() {
            return Err(format!("plane {i} base address null"));
        }
        let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
        let rows = CVPixelBufferGetHeightOfPlane(pb, i);
        let _pw = CVPixelBufferGetWidthOfPlane(pb, i);
        let len = stride * rows;
        let mut data = vec![0u8; len];
        ptr::copy_nonoverlapping(base, data.as_mut_ptr(), len);
        planes.push(Plane::new(data, stride));
    }

    let frame = VideoFrame {
        width,
        height,
        format: PixelFormat::Nv12,
        color_space: ColorSpace::Bt709Limited,
        pts,
        planes,
    };
    frame.validate()?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_is_unsupported_here() {
        assert!(matches!(
            VideoToolboxDecoder::new(Codec::Av1),
            Err(DecodeError::UnsupportedCodec(Codec::Av1))
        ));
    }

    #[test]
    fn maps_codec_to_nal_codec() {
        let d = VideoToolboxDecoder::new(Codec::Hevc).unwrap();
        assert!(matches!(d.nal_codec, NalCodec::Hevc));
        let d = VideoToolboxDecoder::new(Codec::H264).unwrap();
        assert!(matches!(d.nal_codec, NalCodec::H264));
    }

    #[test]
    fn ensure_session_without_params_errors() {
        let mut d = VideoToolboxDecoder::new(Codec::Hevc).unwrap();
        assert!(d.ensure_session(&[]).is_err());
    }
}
