// SPDX-License-Identifier: Apache-2.0
//! Minimal classic-pcap (libpcap) container parser — the format `tcpdump -w`
//! emits. Std-only, no dependencies. pcapng is intentionally out of scope for
//! now (capture with `tcpdump -w session.pcap`, not Wireshark's default).

use std::fmt;

/// A parsed pcap file: its link-layer type + the raw frames in capture order.
pub struct PcapFile {
    pub datalink: u32,
    pub records: Vec<Record>,
}

/// One captured frame (link-layer bytes). Timestamps are dropped — capture order
/// is preserved by `Vec` order, which is all the slicer needs.
pub struct Record {
    pub data: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PcapError {
    TooShort,
    BadMagic(u32),
    Truncated { offset: usize },
}

impl fmt::Display for PcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcapError::TooShort => write!(f, "file shorter than a pcap global header (24 bytes)"),
            PcapError::BadMagic(m) => write!(
                f,
                "not a classic pcap file (magic {m:#010x}); pcapng is not supported — \
                 capture with `tcpdump -w session.pcap`"
            ),
            PcapError::Truncated { offset } => {
                write!(f, "record header/body truncated at offset {offset}")
            }
        }
    }
}

impl std::error::Error for PcapError {}

/// Parse a classic pcap file. Handles both byte orders and µs/ns timestamp
/// magics; the link-layer payload is always big-endian network data regardless.
pub fn parse(bytes: &[u8]) -> Result<PcapFile, PcapError> {
    if bytes.len() < 24 {
        return Err(PcapError::TooShort);
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    // Reading the on-disk magic little-endian disambiguates the writer's order.
    let little = match magic {
        0xa1b2_c3d4 | 0xa1b2_3c4d => true, // µs / ns, little-endian writer
        0xd4c3_b2a1 | 0x4d3c_b2a1 => false, // µs / ns, big-endian writer
        other => return Err(PcapError::BadMagic(other)),
    };
    let rd_u32 = |b: &[u8], o: usize| -> u32 {
        let a = [b[o], b[o + 1], b[o + 2], b[o + 3]];
        if little {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };

    let datalink = rd_u32(bytes, 20);
    let mut records = Vec::new();
    let mut pos = 24;
    while pos < bytes.len() {
        // A truncated trailing record is normal for a still-being-written live
        // capture — keep every complete record and stop, rather than erroring.
        if pos + 16 > bytes.len() {
            break;
        }
        let incl_len = rd_u32(bytes, pos + 8) as usize;
        let start = pos + 16;
        let Some(end) = start.checked_add(incl_len) else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        records.push(Record {
            data: bytes[start..end].to_vec(),
        });
        pos = end;
    }
    Ok(PcapFile { datalink, records })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global_header_le(datalink: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&65535u32.to_le_bytes());
        v.extend_from_slice(&datalink.to_le_bytes());
        v
    }

    fn record_le(data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
        v.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
        v.extend_from_slice(&(data.len() as u32).to_le_bytes()); // incl_len
        v.extend_from_slice(&(data.len() as u32).to_le_bytes()); // orig_len
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn parses_two_records() {
        let mut f = global_header_le(1);
        f.extend(record_le(b"abc"));
        f.extend(record_le(b"defg"));
        let parsed = parse(&f).unwrap();
        assert_eq!(parsed.datalink, 1);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].data, b"abc");
        assert_eq!(parsed.records[1].data, b"defg");
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = [0u8; 24];
        assert!(matches!(parse(&bytes), Err(PcapError::BadMagic(_))));
    }

    #[test]
    fn tolerates_truncated_trailing_record() {
        let mut f = global_header_le(1);
        f.extend(record_le(b"abc")); // one complete record
                                     // Then a truncated record: claims 10 bytes, provides 2.
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&10u32.to_le_bytes());
        f.extend_from_slice(&10u32.to_le_bytes());
        f.extend_from_slice(b"xy");
        // The complete record is kept; the truncated tail is dropped (no error).
        let parsed = parse(&f).unwrap();
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].data, b"abc");
    }
}
