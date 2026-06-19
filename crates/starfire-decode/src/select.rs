// SPDX-License-Identifier: Apache-2.0
//! Runtime backend selection — the `select.rs` pattern (docs/02-architecture.md).
//! No `#[cfg]` soup in callers: ask for a decoder, get the best available impl
//! for this OS + codec, with an explicit error when none exists.

use starfire_core::video::Codec;

use crate::{Decoder, DecodeError, DecodedFrame, VideoDecoder};

/// Whether to prefer hardware decode (the default; SW is fallback only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    PreferHardware,
    ForceSoftware,
}

/// Create the best [`Decoder`] (portable [`crate::VideoFrame`] output) for
/// `codec` on this platform.
///
/// - **macOS:** VideoToolbox (HEVC / H.264). AV1 is not yet wired.
/// - **Windows:** Media Foundation / D3D11VA — scaffolded, reports
///   [`DecodeError::NoBackend`] until the MFT path lands.
/// - **Other:** no permissive native decoder ⇒ [`DecodeError::NoBackend`]. We do
///   **not** ship an ffmpeg/x265/dav1d fallback (copyleft / non-permissive), and
///   no pure-Rust permissive HEVC decoder exists.
pub fn create_decoder(codec: Codec, accel: Accel) -> Result<Box<dyn Decoder>, DecodeError> {
    // Software is never silent: ForceSoftware can't be honored for HEVC/H.264
    // because no permissive SW decoder is available.
    if accel == Accel::ForceSoftware {
        return Err(DecodeError::NoBackend);
    }

    #[cfg(target_os = "macos")]
    {
        let dec = crate::backend::videotoolbox::VideoToolboxDecoder::new(codec)?;
        return Ok(Box::new(dec));
    }

    #[cfg(target_os = "windows")]
    {
        // The MFT decoder isn't wired yet: `new()` always reports NoBackend, so
        // this branch never constructs a `Box<dyn Decoder>`. Once it implements
        // `Decoder`, return `Ok(Box::new(dec))` here.
        crate::backend::mediafoundation::MediaFoundationDecoder::new(codec)?;
        unreachable!("MediaFoundationDecoder::new always errors until implemented");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = codec;
        Err(DecodeError::NoBackend)
    }
}

/// Legacy lower-level factory for the [`VideoDecoder`] (submit/poll/IDR) seam.
/// No functional backend implements it yet, so it reports `NoBackend`.
pub fn create_video_decoder(
    codec: Codec,
    _accel: Accel,
) -> Result<Box<dyn VideoDecoder>, DecodeError> {
    let _ = PlaceholderDecoder { codec };
    Err(DecodeError::NoBackend)
}

/// Compiles-and-constructs placeholder so the [`VideoDecoder`] trait surface is
/// real and testable before a backend implements it. It decodes nothing.
struct PlaceholderDecoder {
    #[allow(dead_code)]
    codec: Codec,
}

impl VideoDecoder for PlaceholderDecoder {
    fn submit(&mut self, _au: &starfire_core::video::AccessUnit) -> Result<(), DecodeError> {
        Err(DecodeError::NoBackend)
    }
    fn poll_frame(&mut self) -> Result<Option<DecodedFrame>, DecodeError> {
        Ok(None)
    }
    fn request_idr(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_software_has_no_permissive_backend() {
        assert!(matches!(
            create_decoder(Codec::Hevc, Accel::ForceSoftware),
            Err(DecodeError::NoBackend)
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_hevc_reports_no_backend_until_mft_lands() {
        assert!(matches!(
            create_decoder(Codec::Hevc, Accel::PreferHardware),
            Err(DecodeError::NoBackend)
        ));
    }

    #[test]
    fn legacy_video_decoder_factory_reports_no_backend() {
        assert!(matches!(
            create_video_decoder(Codec::Av1, Accel::PreferHardware),
            Err(DecodeError::NoBackend)
        ));
    }
}
