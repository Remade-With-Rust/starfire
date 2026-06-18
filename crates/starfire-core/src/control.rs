// SPDX-License-Identifier: Apache-2.0
//! Control stream (ENet over UDP) — docs/protocol/06-control-enet.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! Reliable-UDP (rusty_enet), AES-GCM with the session RI key. Carries input,
//! IDR requests, keepalive, stats, and host→client events. Message type ids and
//! AES-GCM nonce/AAD are [CAPTURE-LOCKED].

/// Logical control messages. Wire ids + payload layouts are [CAPTURE-LOCKED].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    /// Request an IDR/keyframe after unrecoverable video loss (docs/protocol/07).
    RequestIdr,
    /// Liveness + RTT.
    Keepalive,
    /// Graceful termination.
    Terminate,
}

/// Encode + AES-GCM a control message for the ENet channel. Stub until captured.
pub fn encode(_msg: &ControlMessage) -> crate::Result<Vec<u8>> {
    Err(crate::Error::NotImplemented(
        "control::encode",
        "protocol/06-control-enet.md",
    ))
}
