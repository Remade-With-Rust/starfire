// SPDX-License-Identifier: Apache-2.0
//! Test-only packet builder: produces the same wire datagrams a host emits, so
//! reassembly can be exercised with any loss / reorder pattern without a host.

use super::fec::{parity_count, Fec};
use super::rtp::{self, VideoHeader, FLAG_EOF, FLAG_PIC_DATA, FLAG_SOF};

/// Deterministic pseudo-random NAL bytes for frame `seed`.
pub fn nals(seed: u32, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

/// One frame as host datagrams, split into `blocks` FEC blocks (1 = the common
/// case) each carrying `pct` percent parity. Packets come back in send order:
/// every block's data shards followed by that block's parity.
pub fn frame(
    frame_index: u32,
    keyframe: bool,
    payload_nals: &[u8],
    blocksize: usize,
    pct: u8,
    blocks: usize,
) -> Vec<Vec<u8>> {
    let mut payload = vec![0u8; rtp::SHORT_FRAME_HEADER_LEN];
    payload[1..3].copy_from_slice(&42u16.to_le_bytes()); // host latency, 4.2 ms
    payload[3] = if keyframe { 2 } else { 1 };
    payload.extend_from_slice(payload_nals);
    let k_total = payload.len().div_ceil(blocksize).max(1);
    let last_len = payload.len() - (k_total - 1) * blocksize;
    payload[4..6].copy_from_slice(&(last_len as u16).to_le_bytes());
    payload.resize(k_total * blocksize, 0);

    let blocks = blocks.clamp(1, 4).min(k_total);
    let per_block = k_total.div_ceil(blocks);
    let mut fec = Fec::new();
    let mut out = Vec::new();
    let mut seq = (frame_index as u16).wrapping_mul(1000);
    let mut first = 0usize;
    for b in 0..blocks {
        let k = per_block.min(k_total - first);
        let m = parity_count(k, pct);
        let data: Vec<&[u8]> = (0..k)
            .map(|i| &payload[(first + i) * blocksize..(first + i + 1) * blocksize])
            .collect();
        let mut parity = vec![vec![0u8; blocksize]; m];
        if m > 0 {
            let mut pr: Vec<&mut [u8]> = parity.iter_mut().map(|p| p.as_mut_slice()).collect();
            assert!(fec.encode(&data, &mut pr));
        }
        for shard in 0..k + m {
            let mut flags = FLAG_PIC_DATA;
            if shard == 0 && b == 0 {
                flags |= FLAG_SOF;
            }
            if shard + 1 == k + m && b + 1 == blocks {
                flags |= FLAG_EOF;
            }
            let h = VideoHeader {
                rtp_seq: seq,
                frame_index,
                stream_packet_index: (seq as u32) << 8,
                flags,
                shard_index: shard as u16,
                data_shards: k as u16,
                fec_percentage: if m > 0 { pct } else { 0 },
                rtp_timestamp: frame_index.wrapping_mul(1500),
                fec_block: b as u8,
                fec_last_block: (blocks - 1) as u8,
            };
            let body: &[u8] = if shard < k {
                data[shard]
            } else {
                &parity[shard - k]
            };
            let mut pkt = vec![0u8; rtp::PAYLOAD_OFFSET + body.len()];
            assert!(h.write(&mut pkt));
            pkt[rtp::PAYLOAD_OFFSET..].copy_from_slice(body);
            out.push(pkt);
            seq = seq.wrapping_add(1);
        }
        first += k;
    }
    out
}

/// The captured Sunshine stream (`u16`-LE length prefix + datagram, repeated).
pub fn load_fixture() -> Vec<Vec<u8>> {
    let path = format!(
        "{}/../../tests/fixtures/video/stream-hevc.fix",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read(&path).expect("read video fixture");
    let mut pkts = Vec::new();
    let mut i = 0;
    while i + 2 <= raw.len() {
        let n = u16::from_le_bytes([raw[i], raw[i + 1]]) as usize;
        i += 2;
        if i + n > raw.len() {
            break;
        }
        pkts.push(raw[i..i + n].to_vec());
        i += n;
    }
    pkts
}

#[test]
fn header_write_is_the_inverse_of_parse() {
    let h = VideoHeader {
        rtp_seq: 0xBEEF,
        frame_index: 0x0102_0304,
        stream_packet_index: 0x00AB_CD00,
        flags: FLAG_PIC_DATA | FLAG_SOF,
        shard_index: 513,
        data_shards: 777,
        fec_percentage: 50,
        rtp_timestamp: 0xDEAD_BEEF,
        fec_block: 2,
        fec_last_block: 3,
    };
    let mut pkt = vec![0xFFu8; rtp::PAYLOAD_OFFSET + 4];
    assert!(h.write(&mut pkt));
    assert_eq!(rtp::parse_header(&pkt), Some(h));
    assert_eq!(&pkt[rtp::PAYLOAD_OFFSET..], &[0xFF; 4], "payload untouched");
    assert!(!h.write(&mut [0u8; 8]), "short buffer refused");
}

/// Our writer reproduces the header bytes Sunshine put on the wire for the
/// first packet of the captured stream (frame 1, shard 0 of 35, 20 % FEC).
#[test]
fn written_header_matches_the_captured_sunshine_header() {
    let pkts = load_fixture();
    let captured = &pkts[0];
    let h = rtp::parse_header(captured).expect("parse");
    assert_eq!(
        (
            h.frame_index,
            h.shard_index,
            h.data_shards,
            h.fec_percentage
        ),
        (1, 0, 35, 20)
    );
    let mut rebuilt = vec![0u8; rtp::PAYLOAD_OFFSET];
    assert!(h.write(&mut rebuilt));
    assert_eq!(&rebuilt[..], &captured[..rtp::PAYLOAD_OFFSET]);
}
