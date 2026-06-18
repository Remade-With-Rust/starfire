// SPDX-License-Identifier: Apache-2.0
//! Runtime backend selection — the `select.rs` pattern (docs/02-architecture.md).
//! No `#[cfg]` soup in callers: ask for a decoder, get the best available impl
//! for this OS + codec at runtime, with an explicit software fallback.

use starfire_core::video::Codec;

use crate::{DecodeError, DecodedFrame, VideoDecoder};

/// Whether to prefer hardware decode (the default; SW is fallback only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accel {
    PreferHardware,
    ForceSoftware,
}

/// Create the best decoder for `codec` on this platform.
///
/// Phase 0: only a non-functional placeholder exists, so this reports
/// `NoBackend` rather than pretending. Phase 1 adds VideoToolbox (Mac); Phase 2
/// adds Media Foundation/D3D11VA (Win) and the `dav1d` software fallback.
pub fn create_decoder(codec: Codec, _accel: Accel) -> Result<Box<dyn VideoDecoder>, DecodeError> {
    // [Phase 1+] match on target_os + Accel to pick a real backend here.
    let _ = PlaceholderDecoder { codec };
    Err(DecodeError::NoBackend)
}

/// Compiles-and-constructs placeholder so the trait + select surface are real
/// and testable before any backend exists. It decodes nothing.
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
    fn no_backend_until_phase_1() {
        assert!(matches!(
            create_decoder(Codec::Av1, Accel::PreferHardware),
            Err(DecodeError::NoBackend)
        ));
    }
}
