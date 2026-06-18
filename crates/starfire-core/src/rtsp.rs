// SPDX-License-Identifier: Apache-2.0
//! RTSP stream setup — docs/protocol/05-rtsp.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! `OPTIONS → DESCRIBE → SETUP → ANNOUNCE → PLAY` over TCP 48010 (plaintext in
//! capture). Extracts per-stream ports, crypto (RI key/IV), and FEC params.
//! The dialect is custom — the captured transcript is authoritative grammar.

/// What the RTSP exchange yields: the binding info for control/video/audio.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamSetup {
    pub video_port: u16,
    pub audio_port: u16,
    pub control_port: u16,
    // RI key/IV + FEC params extracted from SDP/SETUP are added as captured.
}

/// Drive the RTSP exchange. Stub until the transcript is captured + fixtured.
pub fn negotiate(_address: &str) -> crate::Result<StreamSetup> {
    Err(crate::Error::NotImplemented(
        "rtsp::negotiate",
        "protocol/05-rtsp.md",
    ))
}
