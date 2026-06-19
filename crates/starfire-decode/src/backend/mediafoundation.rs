// SPDX-License-Identifier: Apache-2.0
//! Windows hardware decode via **Media Foundation / D3D11VA** (planned).
//!
//! # Clean-room provenance
//! When implemented, this binds directly to the **Media Foundation** decoder
//! MFTs (`CLSID_MSH265DecoderMFT` / `CLSID_MSH264DecoderMFT`) and/or
//! **D3D11VA** — all first-party Windows system APIs. No ffmpeg, no codec
//! library. Output is an NV12 `IMFSample` (or a D3D11 `ID3D11Texture2D` for the
//! later zero-copy path) copied down into a portable [`crate::VideoFrame`].
//!
//! # Status
//! The Mac VideoToolbox backend is the reference implementation for this phase;
//! the Windows path is scaffolded here so the seam compiles and the runtime
//! factory has a concrete branch. Constructing it reports
//! [`DecodeError::NoBackend`] until the MFT plumbing lands, so callers degrade
//! cleanly instead of getting a silent stub.

use starfire_core::video::Codec;

use crate::DecodeError;

/// Placeholder for the Media Foundation decoder. Implementing [`crate::Decoder`]
/// here will mirror the VideoToolbox flow: build the decoder MFT from the
/// codec's parameter sets, feed length-prefixed samples, copy the NV12 output
/// into a [`crate::VideoFrame`].
pub struct MediaFoundationDecoder {
    _codec: Codec,
}

impl MediaFoundationDecoder {
    /// Not yet implemented. Returns [`DecodeError::NoBackend`] so the factory can
    /// fall through to a clear "no decoder" rather than a silent no-op.
    pub fn new(codec: Codec) -> Result<Self, DecodeError> {
        let _ = MediaFoundationDecoder { _codec: codec };
        Err(DecodeError::NoBackend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_yet_implemented_reports_no_backend() {
        assert!(matches!(
            MediaFoundationDecoder::new(Codec::Hevc),
            Err(DecodeError::NoBackend)
        ));
    }
}
