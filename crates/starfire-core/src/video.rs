// SPDX-License-Identifier: Apache-2.0
//! Video ingest, FEC & reassembly — docs/protocol/07-video-rtp-fec.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! **The long pole.** RTP framing + Reed-Solomon geometry must match Sunshine
//! bit-for-bit or recovery silently corrupts frames. Everything here is
//! [CAPTURE-LOCKED] and gets the most capture budget.

/// Video codecs we ingest. AV1 primary; HEVC/H.264 fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    Hevc,
    H264,
}

/// One coded frame handed to the decoder (AV1 OBUs / HEVC|H.264 NALs).
/// The exact AU framing per codec is [CAPTURE-LOCKED].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub codec: Codec,
    pub frame_index: u32,
    pub is_keyframe: bool,
    pub data: Vec<u8>,
}

/// RTP depacketization — docs/protocol/07 §1. Parses RTP + the Sunshine-specific
/// payload header. Layout derived from captured wire packets + the Sunshine
/// *server* sender semantics (never the moonlight-common-c client struct).
pub mod rtp {
    /// `video_packet_raw_t` = RTP_PACKET(12) + reserved[4] + NV_VIDEO_PACKET.
    /// Offsets are wire-derived (see the `dump_fixture_layout` test).
    pub const RTP_HEADER_LEN: usize = 12;
    pub const RESERVED_LEN: usize = 4;
    pub const NV_OFFSET: usize = RTP_HEADER_LEN + RESERVED_LEN; // 16

    pub const FLAG_PIC_DATA: u8 = 0x01;
    pub const FLAG_EOF: u8 = 0x02;
    pub const FLAG_SOF: u8 = 0x04;

    /// Parsed video packet header (the fields needed to reassemble a frame).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VideoHeader {
        /// RTP sequence number (big-endian on the wire).
        pub rtp_seq: u16,
        /// Monotonic frame counter.
        pub frame_index: u32,
        /// Per-packet stream index (host stores it `<< 8`).
        pub stream_packet_index: u32,
        /// NV flags: PIC_DATA | EOF | SOF.
        pub flags: u8,
        /// Shard index within the frame's FEC block (data shards first).
        pub shard_index: u16,
        /// Number of data shards in this frame's FEC block.
        pub data_shards: u16,
        /// FEC overhead percentage the host applied.
        pub fec_percentage: u8,
    }

    impl VideoHeader {
        pub fn is_sof(&self) -> bool {
            self.flags & FLAG_SOF != 0
        }
        pub fn is_eof(&self) -> bool {
            self.flags & FLAG_EOF != 0
        }
    }

    /// NV_VIDEO_PACKET is 16 bytes; the coded payload follows at [`PAYLOAD_OFFSET`].
    pub const NV_HEADER_LEN: usize = 16;
    pub const PAYLOAD_OFFSET: usize = NV_OFFSET + NV_HEADER_LEN; // 32
    /// SOF packets prefix an 8-byte `video_short_frame_header_t` before the NALs.
    pub const SHORT_FRAME_HEADER_LEN: usize = 8;

    // fecInfo bit layout (host: `fecInfo = x<<12 | data_shards<<22 | pct<<4`).
    const FEC_PCT_SHIFT: u32 = 4;
    const FEC_SHARD_SHIFT: u32 = 12;
    const FEC_DATASHARDS_SHIFT: u32 = 22;
    const FEC_10BIT: u32 = 0x3FF;
    const FEC_8BIT: u32 = 0xFF;

    fn le_u32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    /// Parse the header of one received video datagram. `None` if too short.
    pub fn parse_header(pkt: &[u8]) -> Option<VideoHeader> {
        if pkt.len() < PAYLOAD_OFFSET {
            return None;
        }
        let fec_info = le_u32(pkt, NV_OFFSET + 12);
        Some(VideoHeader {
            rtp_seq: u16::from_be_bytes([pkt[2], pkt[3]]),
            stream_packet_index: le_u32(pkt, NV_OFFSET),
            frame_index: le_u32(pkt, NV_OFFSET + 4),
            flags: pkt[NV_OFFSET + 8],
            shard_index: ((fec_info >> FEC_SHARD_SHIFT) & FEC_10BIT) as u16,
            data_shards: ((fec_info >> FEC_DATASHARDS_SHIFT) & FEC_10BIT) as u16,
            fec_percentage: ((fec_info >> FEC_PCT_SHIFT) & FEC_8BIT) as u8,
        })
    }

    /// The frame type byte from a SOF packet's `video_short_frame_header_t`
    /// (offset 3 in that header): 2 = IDR/keyframe, 1 = P, 4/5 = P variants.
    pub fn sof_frame_type(pkt: &[u8]) -> Option<u8> {
        let at = PAYLOAD_OFFSET + 3;
        pkt.get(at).copied()
    }

    /// Offset of the coded payload (NALs) within a packet: after the NV header,
    /// plus the short-frame header on SOF packets.
    pub fn payload_offset(h: &VideoHeader) -> usize {
        if h.is_sof() {
            PAYLOAD_OFFSET + SHORT_FRAME_HEADER_LEN
        } else {
            PAYLOAD_OFFSET
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod fixture_tests {
    use super::*;

    /// Load the captured datagram fixture (u16-LE length prefix + bytes each).
    fn load_fixture() -> Vec<Vec<u8>> {
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

    /// Reassemble the captured stream into frames and validate the output:
    /// the first frame is an IDR whose bytes start with an HEVC VPS NAL, and
    /// subsequent frames reassemble cleanly. This is the no-loss golden path.
    #[test]
    fn reassembles_fixture_into_hevc_frames() {
        use super::reassembly::Depacketizer;

        let pkts = load_fixture();
        assert!(pkts.len() > 100, "fixture should have many packets");

        let mut dep = Depacketizer::new(Codec::Hevc);
        let mut frames: Vec<AccessUnit> = Vec::new();
        for p in &pkts {
            if let Some(au) = dep.push(p) {
                frames.push(au);
            }
        }

        println!("reassembled {} complete frame(s)", frames.len());
        let first = frames.first().expect("at least one complete frame");
        println!(
            "frame {} keyframe={} bytes={} head={:02x?}",
            first.frame_index,
            first.is_keyframe,
            first.data.len(),
            &first.data[..first.data.len().min(8)]
        );

        // First frame is the IDR and must carry parameter sets + slice.
        assert!(first.is_keyframe, "first frame should be an IDR/keyframe");
        // HEVC Annex-B: a NAL start code (00 00 00 01 or 00 00 01) then the VPS
        // NAL header (0x40 0x01 = nal_type 32).
        let d = &first.data;
        let starts_with_startcode = d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1]);
        assert!(starts_with_startcode, "frame must start with an Annex-B start code: {:02x?}", &d[..8.min(d.len())]);
        assert!(
            d.windows(2).any(|w| w == [0x40, 0x01]),
            "IDR must contain a VPS NAL (40 01)"
        );
    }
}

/// Reed-Solomon FEC — docs/protocol/07 §2, the bit-exact core.
/// `reed-solomon-erasure` with Sunshine's exact `k`/`m`/shard geometry + matrix
/// convention, proven by deterministic loss-injection golden tests
/// (`starfire_testkit::drop_indices`). [CAPTURE-LOCKED]. Stub until captured.
pub mod fec {}

/// Frame reassembly — docs/protocol/07 §3. Reorders fragments by shard index,
/// assembles frames, and emits a complete [`AccessUnit`]. FEC recovery for
/// missing data shards is layered on top in [`super::fec`].
pub mod reassembly {
    use super::rtp::{self, FLAG_PIC_DATA};
    use super::{AccessUnit, Codec};

    /// HEVC frame type 2 = IDR (keyframe), from the SOF short-frame header.
    const FRAME_TYPE_IDR: u8 = 2;

    /// Streaming reassembler. Feed every received video datagram via [`push`];
    /// it yields an [`AccessUnit`] when a frame's data shards are all present.
    ///
    /// Perf: shard payloads are sliced (`bytes[32..]`) without re-parsing the
    /// stream, indexed directly by shard index — no sort on the hot path.
    ///
    /// [`push`]: Depacketizer::push
    pub struct Depacketizer {
        codec: Codec,
        frame_index: Option<u32>,
        data_shards: usize,
        /// Per-shard payload (`bytes[32..]`); `None` until received.
        shards: Vec<Option<Vec<u8>>>,
        received: usize,
        is_keyframe: bool,
        /// Real byte length of the last data shard (rest are full-size).
        last_payload_len: Option<usize>,
    }

    impl Depacketizer {
        pub fn new(codec: Codec) -> Self {
            Self {
                codec,
                frame_index: None,
                data_shards: 0,
                shards: Vec::new(),
                received: 0,
                is_keyframe: false,
                last_payload_len: None,
            }
        }

        fn reset_for(&mut self, frame_index: u32, data_shards: usize) {
            self.frame_index = Some(frame_index);
            self.data_shards = data_shards;
            self.shards.clear();
            self.shards.resize(data_shards, None);
            self.received = 0;
            self.is_keyframe = false;
            self.last_payload_len = None;
        }

        /// Feed one received datagram. Returns a completed [`AccessUnit`] when the
        /// current frame's data shards are all present. Late/duplicate or
        /// non-picture packets return `None`.
        pub fn push(&mut self, pkt: &[u8]) -> Option<AccessUnit> {
            let h = rtp::parse_header(pkt)?;
            if h.flags & FLAG_PIC_DATA == 0 || h.data_shards == 0 {
                return None;
            }
            // A new frame index starts a fresh accumulation (we don't currently
            // reorder across frames; out-of-order frames are dropped, IDR-repair
            // territory handled later).
            if self.frame_index != Some(h.frame_index) {
                self.reset_for(h.frame_index, h.data_shards as usize);
            }
            if h.is_sof() {
                self.is_keyframe = rtp::sof_frame_type(pkt) == Some(FRAME_TYPE_IDR);
                // lastPayloadLen lives in the SOF short-frame header (LE u16 @ +4).
                let at = rtp::PAYLOAD_OFFSET + 4;
                if at + 2 <= pkt.len() {
                    self.last_payload_len =
                        Some(u16::from_le_bytes([pkt[at], pkt[at + 1]]) as usize);
                }
            }
            // Only data shards carry frame bytes; parity shards feed FEC (later).
            let slot = h.shard_index as usize;
            if slot < self.data_shards && self.shards[slot].is_none() {
                let off = rtp::PAYLOAD_OFFSET;
                if off <= pkt.len() {
                    self.shards[slot] = Some(pkt[off..].to_vec());
                    self.received += 1;
                }
            }
            if self.received == self.data_shards {
                return self.assemble();
            }
            None
        }

        fn assemble(&mut self) -> Option<AccessUnit> {
            let mut data = Vec::with_capacity(self.data_shards * 1376);
            for (i, shard) in self.shards.iter().enumerate() {
                let bytes = shard.as_ref()?;
                // Trim the last data shard to its real length (rest are padding).
                if i + 1 == self.data_shards {
                    if let Some(n) = self.last_payload_len {
                        data.extend_from_slice(&bytes[..n.min(bytes.len())]);
                        continue;
                    }
                }
                data.extend_from_slice(bytes);
            }
            // Strip the 8-byte short-frame header that prefixes shard 0's data.
            if data.len() >= rtp::SHORT_FRAME_HEADER_LEN {
                data.drain(..rtp::SHORT_FRAME_HEADER_LEN);
            }
            let au = AccessUnit {
                codec: self.codec,
                frame_index: self.frame_index.unwrap_or(0),
                is_keyframe: self.is_keyframe,
                data,
            };
            self.frame_index = None; // force a fresh frame next push
            Some(au)
        }
    }
}
