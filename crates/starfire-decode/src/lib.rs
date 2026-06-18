// SPDX-License-Identifier: Apache-2.0
//! Video decode behind the `VideoDecoder` trait — docs/04-platform-backends.md.
//!
//! One of the four platform seams. Backends are selected at **runtime**
//! ([`select`]), never via `#[cfg]` scattered through logic. Every `unsafe` FFI
//! boundary (VideoToolbox / Media Foundation / D3D11VA) is isolated to its own
//! backend module. `dav1d` is the software fallback — used explicitly, never
//! silently (docs/07-performance-budgets.md).

use starfire_core::video::{AccessUnit, Codec};

pub mod select;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("codec {0:?} not supported by this backend")]
    UnsupportedCodec(Codec),
    #[error("no decoder backend available for this platform/codec yet")]
    NoBackend,
    #[error("decode failed: {0}")]
    Failed(String),
}

/// A decoded frame. Real backends hand back a platform surface (IOSurface /
/// D3D11 texture) for zero-copy present; the field set is filled per backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub frame_index: u32,
}

/// The platform seam. Submit access units (docs/protocol/07 emits these), poll
/// decoded frames, request an IDR on recovery. Impls must never panic on bad
/// input — surface a `DecodeError` so the session can recover.
pub trait VideoDecoder: Send {
    fn submit(&mut self, au: &AccessUnit) -> Result<(), DecodeError>;
    fn poll_frame(&mut self) -> Result<Option<DecodedFrame>, DecodeError>;
    fn request_idr(&mut self);
}
