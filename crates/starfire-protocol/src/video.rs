// SPDX-License-Identifier: Apache-2.0
//! Video packet wire layout, shared by the client (parse) and the host (write)
//! — docs/protocol/07-video-rtp-fec.md §1. One definition of the header means
//! the two ends cannot drift apart.
//!
//! Derived from protocol observation against Sunshine (the captured stream in
//! `tests/fixtures/video/stream-hevc.fix`) plus the Sunshine *server* sender
//! semantics. Clean-room w.r.t. the client.

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
        /// RTP timestamp (big-endian on the wire): the frame's capture time on
        /// the host's 90 kHz media clock, identical on every packet of a frame.
        /// [SOURCE: observed in the Sunshine capture fixture — constant within a
        /// frame, advancing ~1500 ticks per 60 fps frame.]
        pub rtp_timestamp: u32,
        /// Which FEC block of the frame this shard belongs to (`0..=3`). A frame
        /// too large for one 255-shard block is split into up to four
        /// independent blocks; shard indices and `data_shards` are per block.
        pub fec_block: u8,
        /// Index of the frame's last FEC block (`0` = single-block frame).
        pub fec_last_block: u8,
    }

    impl VideoHeader {
        pub fn is_sof(&self) -> bool {
            self.flags & FLAG_SOF != 0
        }
        pub fn is_eof(&self) -> bool {
            self.flags & FLAG_EOF != 0
        }

        /// Parity shards in this packet's FEC block: `ceil(data * pct / 100)`.
        pub fn parity_shards(&self) -> usize {
            crate::fec::parity_count(self.data_shards as usize, self.fec_percentage)
        }

        /// True when this packet carries a parity shard (index past the data).
        pub fn is_parity(&self) -> bool {
            self.shard_index >= self.data_shards
        }

        /// Write this header into the first [`PAYLOAD_OFFSET`] bytes of `pkt`
        /// (the exact inverse of [`parse_header`]) — the single definition of
        /// the layout for anything that builds video packets. Returns `false`
        /// (nothing written) if `pkt` is shorter than the header.
        ///
        /// `#[inline]`: the host writes one per datagram; inlined into the
        /// packetizer's loop the header is assembled in registers and stored
        /// once, instead of a zero-fill plus field stores behind a call.
        #[inline]
        pub fn write(&self, pkt: &mut [u8]) -> bool {
            let Some(out) = pkt.first_chunk_mut::<PAYLOAD_OFFSET>() else {
                return false;
            };
            let mut h = [0u8; PAYLOAD_OFFSET];
            h[0] = RTP_V2_EXT;
            h[2..4].copy_from_slice(&self.rtp_seq.to_be_bytes());
            h[4..8].copy_from_slice(&self.rtp_timestamp.to_be_bytes());
            h[NV_OFFSET..NV_OFFSET + 4].copy_from_slice(&self.stream_packet_index.to_le_bytes());
            h[NV_OFFSET + 4..NV_OFFSET + 8].copy_from_slice(&self.frame_index.to_le_bytes());
            h[NV_OFFSET + 8] = self.flags;
            h[NV_OFFSET + 10] = MULTI_FEC_FLAGS;
            h[NV_OFFSET + 11] = ((self.fec_block & 0x3) << MULTI_FEC_BLOCK_SHIFT)
                | ((self.fec_last_block & 0x3) << MULTI_FEC_LAST_SHIFT);
            let fec_info: u32 = ((self.data_shards as u32 & FEC_10BIT) << FEC_DATASHARDS_SHIFT)
                | ((self.shard_index as u32 & FEC_10BIT) << FEC_SHARD_SHIFT)
                | ((self.fec_percentage as u32) << FEC_PCT_SHIFT);
            h[NV_OFFSET + 12..NV_OFFSET + 16].copy_from_slice(&fec_info.to_le_bytes());
            *out = h;
            true
        }
    }

    /// RTP byte 0 as Sunshine sends it: version 2 with the extension bit.
    /// [SOURCE: capture fixture — every video packet starts `0x90`.]
    pub const RTP_V2_EXT: u8 = 0x90;
    /// `NV_VIDEO_PACKET.multiFecFlags` (offset +10): constant `0x10` on every
    /// data shard. [SOURCE: capture fixture.]
    pub const MULTI_FEC_FLAGS: u8 = 0x10;
    // `NV_VIDEO_PACKET.multiFecBlocks` (offset +11): this shard's block index in
    // bits 4-5 and the frame's last block index in bits 6-7, on every shard of
    // the block (data and parity). `0` for a single-block frame, which is what
    // the capture fixture shows. [SOURCE: Sunshine server sender semantics, per
    // docs/clean-room-policy.md §"On Sunshine being GPLv3"; the multi-block
    // values are CAPTURE-LOCKED pending a capture of a frame >255 shards.]
    const MULTI_FEC_BLOCK_SHIFT: u32 = 4;
    const MULTI_FEC_LAST_SHIFT: u32 = 6;

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
    ///
    /// `#[inline]`: it runs once per received packet from the depacketizer in
    /// another crate, which reads only some of the fields; inlined, the unused
    /// ones are never loaded and there is no call frame.
    #[inline]
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
            rtp_timestamp: u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
            fec_block: (pkt[NV_OFFSET + 11] >> MULTI_FEC_BLOCK_SHIFT) & 0x3,
            fec_last_block: (pkt[NV_OFFSET + 11] >> MULTI_FEC_LAST_SHIFT) & 0x3,
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
