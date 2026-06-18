// SPDX-License-Identifier: Apache-2.0
//! Test harness for Starfire — the consuming side of the bit-exact methodology
//! (see docs/03-bitexact-methodology.md).
//!
//! Three jobs:
//!   1. Load committed capture fixtures + their `.meta.toml` sidecars.
//!   2. Assert our bytes equal a fixture **exactly** (golden tests).
//!   3. Inject deterministic loss for FEC / reassembly tests.
//!
//! Fixtures ARE the spec. A golden test going red means our bytes drifted from a
//! real Sunshine capture — that is the signal that protects "bit-for-bit".

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Version-stamped metadata committed alongside every fixture as `<name>.meta.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Meta {
    /// Sunshine release the capture was taken from. A green test against a stale
    /// version is a false positive — re-capture on host upgrade.
    pub sunshine_version: String,
    /// Capture date (YYYY-MM-DD).
    pub captured: String,
    /// Logical layer, e.g. "pairing/getservercert".
    pub layer: String,
    /// Negotiated codec for media captures (optional).
    #[serde(default)]
    pub codec: Option<String>,
    /// Free-form notes (host, resolution, redactions…).
    #[serde(default)]
    pub notes: Option<String>,
}

/// A loaded fixture: the raw captured bytes plus their metadata.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub bytes: Vec<u8>,
    pub meta: Meta,
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("reading fixture {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing meta {path}: {source}")]
    Meta {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
}

impl Fixture {
    /// Load `<bin_path>` and its sibling `<bin_path-with-.meta.toml>`.
    ///
    /// e.g. `load("tests/fixtures/pairing/getservercert.bin")` also reads
    /// `tests/fixtures/pairing/getservercert.meta.toml`.
    pub fn load(bin_path: impl AsRef<Path>) -> Result<Self, FixtureError> {
        let bin_path = bin_path.as_ref();
        let bytes = std::fs::read(bin_path).map_err(|source| FixtureError::Io {
            path: bin_path.to_path_buf(),
            source,
        })?;
        let meta_path = bin_path.with_extension("meta.toml");
        let meta_str = std::fs::read_to_string(&meta_path).map_err(|source| FixtureError::Io {
            path: meta_path.clone(),
            source,
        })?;
        let meta = toml::from_str(&meta_str).map_err(|source| FixtureError::Meta {
            path: meta_path,
            source: Box::new(source),
        })?;
        Ok(Self { bytes, meta })
    }
}

/// Golden assertion: `actual` must equal `expected` byte-for-byte. On mismatch,
/// panics with the first differing offset and a short hex window around it — the
/// detail you need to find where our encoder drifted from the capture.
#[track_caller]
pub fn assert_bytes_eq(actual: &[u8], expected: &[u8]) {
    if actual == expected {
        return;
    }
    let first_diff = actual
        .iter()
        .zip(expected.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(actual.len().min(expected.len()));
    panic!(
        "golden mismatch: lengths actual={} expected={}, first diff at byte {}\n  actual   {}\n  expected {}",
        actual.len(),
        expected.len(),
        first_diff,
        hex_window(actual, first_diff),
        hex_window(expected, first_diff),
    );
}

fn hex_window(bytes: &[u8], around: usize) -> String {
    let start = around.saturating_sub(4);
    let end = (around + 8).min(bytes.len());
    let mut s = String::new();
    for (i, b) in bytes[start..end].iter().enumerate() {
        let marker = if start + i == around { ">" } else { " " };
        s.push_str(&format!("{marker}{b:02x}"));
    }
    s
}

/// Take a list of shards and `None` out the indices in `drop` — a deterministic
/// loss pattern for FEC / reassembly golden tests (docs/protocol/07).
pub fn drop_indices<T: Clone>(shards: &[T], drop: &[usize]) -> Vec<Option<T>> {
    shards
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if drop.contains(&i) {
                None
            } else {
                Some(s.clone())
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bytes_eq_passes_on_match() {
        assert_bytes_eq(&[1, 2, 3], &[1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "golden mismatch")]
    fn bytes_eq_panics_on_diff() {
        assert_bytes_eq(&[1, 2, 3], &[1, 9, 3]);
    }

    #[test]
    #[should_panic(expected = "first diff at byte 3")]
    fn bytes_eq_reports_length_diff() {
        assert_bytes_eq(&[1, 2, 3], &[1, 2, 3, 4]);
    }

    #[test]
    fn drop_indices_nulls_the_right_shards() {
        let shards = vec![10u8, 20, 30, 40];
        let with_loss = drop_indices(&shards, &[1, 3]);
        assert_eq!(with_loss, vec![Some(10), None, Some(30), None]);
    }
}
