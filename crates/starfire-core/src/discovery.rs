// SPDX-License-Identifier: Apache-2.0
//! Discovery & host management — docs/protocol/01-discovery.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! Phase 1 (F1): mDNS `_nvstream._tcp` + manual hosts + `/serverinfo`
//! reachability/pair-status probe. Stub until live capture.

/// A known Sunshine host, persisted per-entry (no single-blob storage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub hostname: String,
    pub address: String,
    pub paired: bool,
}

/// Probe `/serverinfo` to learn reachability + pair status.
pub fn probe(_address: &str) -> crate::Result<Host> {
    Err(crate::Error::NotImplemented(
        "discovery::probe",
        "protocol/01-discovery.md",
    ))
}
