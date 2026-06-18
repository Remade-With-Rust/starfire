// SPDX-License-Identifier: Apache-2.0
//! Crate-wide error type. Loss, reorder, and malformed input are *normal*
//! operating conditions (docs/01-overview.md) — they are errors to handle, never
//! reasons to panic. No `unwrap`/`panic` on the hot path.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wire format error: {0}")]
    Wire(#[from] crate::wire::WireError),

    #[error("not yet implemented: {0} (docs/{1})")]
    NotImplemented(&'static str, &'static str),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}
