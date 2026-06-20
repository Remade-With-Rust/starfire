// SPDX-License-Identifier: Apache-2.0
//! Shared GameStream pairing crypto — the PIN-challenge primitives ([`crypto`])
//! and the self-signed [`identity`] that BOTH ends need: the Starfire client and
//! the Comet host. The client `/pair` ladder *driver* stays in `starfire-core`
//! (it drives the mTLS HTTP client); Comet builds the server side of the same
//! ladder on these primitives. Clean-room per docs/protocol/02-pairing-and-crypto.md.

pub mod crypto;
pub mod identity;

pub use identity::ClientIdentity;
