// SPDX-License-Identifier: Apache-2.0
//! Video ingest, FEC & reassembly — docs/protocol/07-video-rtp-fec.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! **The long pole.** RTP framing + Reed-Solomon geometry must match Sunshine
//! bit-for-bit or recovery silently corrupts frames. Everything here is
//! [CAPTURE-LOCKED] and gets the most capture budget.

/// Video codecs we ingest. AV1 primary; HEVC/H.264 fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    Hevc,
    H264,
}

/// One coded frame handed to the decoder (AV1 OBUs / HEVC|H.264 NALs).
/// The exact AU framing per codec is [CAPTURE-LOCKED].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub codec: Codec,
    pub frame_index: u32,
    pub is_keyframe: bool,
    pub data: Vec<u8>,
}

/// RTP depacketization — docs/protocol/07 §1. Parses RTP + the Sunshine-specific
/// payload header. [CAPTURE-LOCKED] header layout. Stub until captured.
pub mod rtp {}

/// Reed-Solomon FEC — docs/protocol/07 §2, the bit-exact core.
/// `reed-solomon-erasure` with Sunshine's exact `k`/`m`/shard geometry + matrix
/// convention, proven by deterministic loss-injection golden tests
/// (`starfire_testkit::drop_indices`). [CAPTURE-LOCKED]. Stub until captured.
pub mod fec {}

/// Frame reassembly — docs/protocol/07 §3. Reorders fragments, assembles frames,
/// requests IDR on unrecoverable loss, never emits a corrupt AU. Stub.
pub mod reassembly {}
