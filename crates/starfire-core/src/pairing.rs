// SPDX-License-Identifier: Apache-2.0
//! Pairing & client crypto — docs/protocol/02-pairing-and-crypto.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! Owns: P-256 client identity (rcgen), the `/pair` ladder, the PIN KDF
//! (`SHA-256(salt ‖ pin)` → AES-128 ECB), auto-PIN, pre-provisioning, and naming
//! of the session RI key/IV (established at launch/RTSP). All byte layouts are
//! [CAPTURE-LOCKED] — implement against fixtures + known-answer crypto vectors.

/// Session crypto material surfaced by launch/RTSP and consumed by control/input.
/// (Established later in the lifecycle; named here as the canonical home.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKeys {
    pub ri_key: Vec<u8>,
    pub ri_iv: Vec<u8>,
}

/// Run the `/pair` ladder (auto-PIN). Stub until the ladder is captured.
pub fn pair(_address: &str) -> crate::Result<()> {
    Err(crate::Error::NotImplemented(
        "pairing::pair",
        "protocol/02-pairing-and-crypto.md",
    ))
}
