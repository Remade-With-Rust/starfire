// SPDX-License-Identifier: Apache-2.0
//! # starfire-protocol
//!
//! The shared, direction-agnostic GameStream **wire** layer, extracted from
//! `starfire-core` so the Starfire **client** and the Comet **host** depend on
//! the SAME bytes — and therefore pair flawlessly by construction
//! (see ../../../comet/PLAN.md §3).
//!
//! ## Clean-room
//! Derived from protocol observation against Sunshine; no Moonlight /
//! moonlight-common-c source was consulted. See ../../docs/clean-room-policy.md.
//!
//! ## Migration
//! Foundation slice today: [`error`], [`wire`], [`hex`], [`xml`]. The format
//! modules (pairing crypto, RTSP/SDP, video packet headers + FEC, input,
//! serverinfo) migrate here incrementally; `starfire-core` re-exports each so
//! its callers stay unchanged throughout.

pub mod error;
pub mod hex;
pub mod wire;
pub mod xml;

pub use error::{Error, Result};
