// SPDX-License-Identifier: Apache-2.0
//! # starfire-core
//!
//! The clean-room protocol core for the Sunshine GameStream wire protocol.
//! OS-agnostic and UI-agnostic: this crate compiles and tests with no platform
//! framework linked, so it can be both the standalone OSS artifact and the
//! embedded dependency (docs/02-architecture.md).
//!
//! ## Clean-room
//! Derived from protocol observation against Sunshine. No Moonlight /
//! moonlight-common-c source was consulted. See docs/clean-room-policy.md.
//!
//! ## Layout
//! Modules follow the connection lifecycle, one per protocol doc:
//! discovery → pairing → serverinfo → launch → rtsp → control → video → audio →
//! input, orchestrated by [`session`]. Every wire type implements [`wire::Wire`]
//! so it has a uniform golden-test surface (docs/03-bitexact-methodology.md).

pub mod error;
pub mod wire;

pub mod audio;
pub mod control;
pub mod discovery;
pub mod input;
pub mod launch;
pub mod pairing;
pub mod rtsp;
pub mod serverinfo;
pub mod session;
pub mod video;

pub use error::{Error, Result};
