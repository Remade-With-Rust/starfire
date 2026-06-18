// SPDX-License-Identifier: Apache-2.0
//! Pairing & client crypto — docs/protocol/02-pairing-and-crypto.md.
//! Derived from the public GameStream pairing protocol; validated by live pairing
//! against Sunshine. Clean-room — no Moonlight source consulted.
//!
//! Owns: the PIN challenge crypto ([`crypto`]), the client identity, the `/pair`
//! ladder, auto-PIN, and the naming of the session RI key/IV (established later,
//! at launch/RTSP). Built up across F2:
//!   F2a — crypto primitives (this commit).
//!   F2b — client identity (RSA-2048 cert + signature extraction).
//!   F2c — the `/pair` ladder + auto-PIN, iterated live.

pub mod crypto;
pub mod identity;
pub mod ladder;

pub use identity::ClientIdentity;
pub use ladder::PairingClient;

/// Session crypto material surfaced by launch/RTSP and consumed by control/input.
/// (Established later in the lifecycle; named here as the canonical home.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKeys {
    pub ri_key: Vec<u8>,
    pub ri_iv: Vec<u8>,
}

// The `/pair` ladder lives in [`ladder::PairingClient`] (validated live against
// Sunshine). Auto-PIN submission to the host's web API is integrated in F2's
// follow-up (needs the TLS client also used for HTTPS `/serverinfo` in F3).
