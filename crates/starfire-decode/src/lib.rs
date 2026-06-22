// SPDX-License-Identifier: Apache-2.0
//! Video decode behind a trait — docs/04-platform-backends.md.
//!
//! # Clean-room provenance
//! This crate decodes **Annex-B HEVC** (and, by design, H.264 / AV1) access
//! units using **OS-native decoders only**:
//! - **macOS:** VideoToolbox (raw `extern "C"` FFI to the system frameworks —
//!   no third-party binding crate, no GPL/LGPL code). See [`backend::videotoolbox`].
//! - **Windows:** Media Foundation / D3D11VA (planned; see [`backend::mediafoundation`]).
//!
//! There is intentionally **no ffmpeg / x265 / dav1d-via-ffmpeg / gstreamer**
//! dependency: `deny.toml` rejects copyleft, and no permissive *pure-Rust* HEVC
//! decoder exists, so unsupported platforms return [`DecodeError::NoBackend`]
//! rather than shipping a non-permissive fallback.
//!
//! # Two trait surfaces
//! - [`Decoder`] — the portable, CPU-frame API the renderer consumes: feed an
//!   [`AccessUnit`], get back an optional [`VideoFrame`] (planar NV12/I420). This
//!   is the seam used end-to-end with `starfire-render`.
//! - [`VideoDecoder`] — the lower-level submit/poll/IDR surface from
//!   docs/04, kept for the session-recovery contract. Hardware backends can
//!   implement either; [`Decoder`] is what most callers want.
//!
//! # Input mapping to `starfire-core`
//! Input is [`starfire_core::video::AccessUnit`] — `{ codec, frame_index,
//! is_keyframe, data }` where `data` is **Annex-B** bytes (start-code-prefixed
//! NAL units for HEVC/H.264). `starfire-core` has no decode/render deps, so this
//! dependency is acyclic; we re-export the relevant types below.

use starfire_core::video::AccessUnit;
pub use starfire_core::video::Codec;

pub mod annexb;
pub mod backend;
pub mod frame;
pub mod select;
/// Shared D3D11 device for the Windows zero-copy decode → present pipeline.
#[cfg(target_os = "windows")]
pub mod win_device;

pub use frame::{ColorSpace, PixelFormat, Plane, VideoFrame};

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("codec {0:?} not supported by this backend")]
    UnsupportedCodec(Codec),
    #[error("no decoder backend available for this platform/codec yet")]
    NoBackend,
    #[error("decode failed: {0}")]
    Failed(String),
}

/// The portable decode seam: Annex-B access units in, CPU [`VideoFrame`]s out.
///
/// Contract:
/// - [`push`](Decoder::push) submits one access unit and returns `Ok(Some(frame))`
///   when a frame is ready, or `Ok(None)` if the decoder needs more input
///   (reordering / pipeline latency). It must **never panic** on malformed
///   input — return [`DecodeError::Failed`] so the session can request an IDR.
/// - [`flush`](Decoder::flush) drains any frames still buffered in the decoder
///   (end of stream / format change), returning them in presentation order.
///
/// Frames are owned, validated CPU buffers; hardware backends copy their native
/// surface down into [`VideoFrame`] before returning (zero-copy GPU-surface
/// hand-off is a later optimization layered behind the same trait).
pub trait Decoder: Send {
    /// Submit one Annex-B access unit; maybe yields a decoded frame.
    fn push(&mut self, au: &AccessUnit) -> Result<Option<VideoFrame>, DecodeError>;

    /// Drain buffered frames at end-of-stream. Default: nothing buffered.
    fn flush(&mut self) -> Result<Vec<VideoFrame>, DecodeError> {
        Ok(Vec::new())
    }
}

/// A decoded frame *reference* in the lower-level [`VideoDecoder`] surface. Real
/// backends hand back a platform surface (IOSurface / D3D11 texture) for
/// zero-copy present; the field set is filled per backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub frame_index: u32,
}

/// The platform seam from docs/04. Submit access units, poll decoded frames,
/// request an IDR on recovery. Impls must never panic on bad input — surface a
/// `DecodeError` so the session can recover.
pub trait VideoDecoder: Send {
    fn submit(&mut self, au: &AccessUnit) -> Result<(), DecodeError>;
    fn poll_frame(&mut self) -> Result<Option<DecodedFrame>, DecodeError>;
    fn request_idr(&mut self);
}
