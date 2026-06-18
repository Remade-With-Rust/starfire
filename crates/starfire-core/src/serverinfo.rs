// SPDX-License-Identifier: Apache-2.0
//! Server capabilities & negotiation — docs/protocol/03-serverinfo-and-negotiation.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! Parses `/serverinfo` GameStream XML and negotiates the best mutual config.
//! XML element names + flag bits are [CAPTURE-LOCKED] to a verbatim fixture.

/// Decoded `ServerCodecModeSupport` bitfield. AV1 = 0x40000 (confirmed); other
/// bits are [CAPTURE-LOCKED] — confirm from the fixture before trusting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodecCaps {
    pub av1: bool,
    pub hevc: bool,
    pub h264: bool,
    pub main10: bool,
}

/// AV1 support bit in `ServerCodecModeSupport`.
pub const SERVER_CODEC_AV1: u32 = 0x40000;

impl CodecCaps {
    /// Decode the subset of bits we can confirm today. Extend as the fixture
    /// pins the remaining bit positions.
    pub fn from_server_codec_mode_support(mask: u32) -> Self {
        Self {
            av1: mask & SERVER_CODEC_AV1 != 0,
            // [CAPTURE-LOCKED] hevc/h264/main10 bit positions — do not guess.
            ..Self::default()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn av1_bit_decodes() {
        assert!(CodecCaps::from_server_codec_mode_support(SERVER_CODEC_AV1).av1);
        assert!(!CodecCaps::from_server_codec_mode_support(0).av1);
    }
}
