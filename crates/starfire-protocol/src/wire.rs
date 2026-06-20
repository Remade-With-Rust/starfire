// SPDX-License-Identifier: Apache-2.0
//! The uniform wire-codec surface every protocol type implements.
//!
//! This is what makes the bit-exact methodology mechanical: a type's golden test
//! is always "decode the fixture, re-encode it, assert the bytes are identical".
//! See docs/03-bitexact-methodology.md.

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("unexpected end of input: needed {needed} more byte(s) at offset {offset}")]
    UnexpectedEof { offset: usize, needed: usize },
    #[error("invalid value at offset {offset}: {reason}")]
    Invalid { offset: usize, reason: &'static str },
}

/// Encode to / decode from the exact Sunshine wire bytes.
///
/// Implementors MUST be byte-exact: `encode` reproduces a captured fixture and
/// `decode` accepts it. The round-trip laws below are enforced by golden tests.
pub trait Wire: Sized {
    /// Append this value's wire bytes to `out`.
    fn encode(&self, out: &mut Vec<u8>);

    /// Parse one value from the front of `input`.
    fn decode(input: &[u8]) -> Result<Self, WireError>;

    /// Convenience: encode to a fresh `Vec`.
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

/// Read a big-endian `u16` at `off`, or a precise EOF error. Shared by the
/// packet codecs — keeps "no panic on short/malformed input" (docs/01-overview.md)
/// a one-liner rather than a footgun.
pub fn be_u16(input: &[u8], off: usize) -> Result<u16, WireError> {
    let end = off + 2;
    let slice = input.get(off..end).ok_or(WireError::UnexpectedEof {
        offset: off,
        needed: end.saturating_sub(input.len()),
    })?;
    Ok(u16::from_be_bytes([slice[0], slice[1]]))
}

/// Test helper: assert both round-trip directions for a value/bytes pair.
///   - `encode(value) == bytes`   (we reproduce the capture)
///   - `decode(bytes) == value`   (we accept the capture)
#[cfg(any(test, feature = "test-helpers"))]
#[track_caller]
#[allow(clippy::expect_used)] // a test helper: panicking *is* the failure signal
pub fn assert_roundtrip<T>(value: &T, bytes: &[u8])
where
    T: Wire + PartialEq + std::fmt::Debug,
{
    let encoded = value.to_bytes();
    assert_eq!(encoded, bytes, "encode(value) must equal the fixture bytes");
    let decoded = T::decode(bytes).expect("decode of fixture bytes must succeed");
    assert_eq!(&decoded, value, "decode(bytes) must equal the value");
}
