// SPDX-License-Identifier: Apache-2.0
//! Tiny hex encode/decode. GameStream carries certs, salts, and challenge blobs
//! as hex in query params and XML, so we need this everywhere in pairing (F2).
//! Hand-rolled to keep the dependency tree minimal (docs/08).

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HexError {
    #[error("odd-length hex string ({0} chars)")]
    OddLength(usize),
    #[error("invalid hex char {0:?} at offset {1}")]
    InvalidChar(char, usize),
}

/// Lowercase hex.
pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nibble(b >> 4, false));
        s.push(nibble(b & 0x0f, false));
    }
    s
}

/// Uppercase hex (GameStream responses use uppercase; some requests too).
pub fn encode_upper(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(nibble(b >> 4, true));
        s.push(nibble(b & 0x0f, true));
    }
    s
}

/// Decode hex, accepting either case. Surrounding whitespace is trimmed.
pub fn decode(s: &str) -> Result<Vec<u8>, HexError> {
    let s = s.trim().as_bytes();
    if !s.len().is_multiple_of(2) {
        return Err(HexError::OddLength(s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < s.len() {
        let hi = val(s[i], i)?;
        let lo = val(s[i + 1], i + 1)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn nibble(n: u8, upper: bool) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ if upper => (b'A' + n - 10) as char,
        _ => (b'a' + n - 10) as char,
    }
}

fn val(c: u8, offset: usize) -> Result<u8, HexError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(HexError::InvalidChar(c as char, offset)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_case() {
        let bytes = [0x00, 0x12, 0xab, 0xff];
        assert_eq!(encode(&bytes), "0012abff");
        assert_eq!(encode_upper(&bytes), "0012ABFF");
        assert_eq!(decode("0012ABFF").unwrap(), bytes);
        assert_eq!(decode("0012abff").unwrap(), bytes);
        assert_eq!(decode("  0012abff \n").unwrap(), bytes);
    }

    #[test]
    fn errors() {
        assert_eq!(decode("abc"), Err(HexError::OddLength(3)));
        assert!(matches!(decode("zz"), Err(HexError::InvalidChar('z', 0))));
    }
}
