// SPDX-License-Identifier: Apache-2.0
//! App list & session launch — docs/protocol/04-applist-and-launch.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! `/applist`, `/launch`, `/resume`, `/cancel`. Query-param names, order, and the
//! RI key/IV encoding are [CAPTURE-LOCKED] to request-line fixtures.

/// A launchable app from `/applist`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub title: String,
}

/// Negotiated session configuration carried into `/launch` and RTSP `ANNOUNCE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bit_depth: u8,
    pub hdr: bool,
}

/// Build the `/launch` request line for `config`. Stub: the exact param set and
/// order are [CAPTURE-LOCKED] and asserted by a golden test against the capture.
pub fn launch_query(_app: &App, _config: &SessionConfig) -> crate::Result<String> {
    Err(crate::Error::NotImplemented(
        "launch::launch_query",
        "protocol/04-applist-and-launch.md",
    ))
}
