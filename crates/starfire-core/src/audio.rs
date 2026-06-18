// SPDX-License-Identifier: Apache-2.0
//! Audio ingest — docs/protocol/08-audio-rtp.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! RTP audio → FEC → Opus decode → channel layout → A/V sync. RTP framing, FEC
//! scheme, and channel ordering are [CAPTURE-LOCKED].

/// Negotiated audio channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLayout {
    Stereo,
    Surround51,
    Surround71,
}

/// Decode one Opus audio packet to PCM. Stub until captured + Opus wired.
pub fn decode_packet(_payload: &[u8]) -> crate::Result<Vec<i16>> {
    Err(crate::Error::NotImplemented(
        "audio::decode_packet",
        "protocol/08-audio-rtp.md",
    ))
}
