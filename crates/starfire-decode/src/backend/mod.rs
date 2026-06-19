// SPDX-License-Identifier: Apache-2.0
//! Platform decode backends, each cfg-gated to its OS.
//!
//! Clean-room: every backend talks to the **OS-native** decoder API directly.
//! No backend links ffmpeg, x264/x265, dav1d, or gstreamer (see `deny.toml`).
//!
//! - [`videotoolbox`] — macOS, raw `extern "C"` FFI to VideoToolbox /
//!   CoreMedia / CoreVideo. Compiles only on `target_os = "macos"`.
//! - [`mediafoundation`] — Windows, Media Foundation / D3D11VA (planned).
//!
//! The runtime factory in [`crate::select`] picks the right one (or returns
//! [`crate::DecodeError::NoBackend`]).

#[cfg(target_os = "macos")]
pub mod videotoolbox;

#[cfg(target_os = "windows")]
pub mod mediafoundation;
