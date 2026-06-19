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

    /// Diagnostic: for frame 1 (the IDR), report the data/parity shard geometry
    /// and whether parity shards were captured (needed to test FEC recovery).
    #[test]
    fn fixture_fec_geometry() {
        let pkts = load_fixture();
        let mut indices = std::collections::BTreeSet::new();
        let mut data_shards = 0u16;
        let mut sizes = std::collections::BTreeSet::new();
        for p in &pkts {
            let h = rtp::parse_header(p).expect("parse");
            if h.frame_index == 1 {
                indices.insert(h.shard_index);
                data_shards = h.data_shards;
                sizes.insert(p.len());
            }
        }
        let max_idx = indices.iter().last().copied().unwrap_or(0);
        let parity_present: Vec<u16> = indices.iter().copied().filter(|&i| i >= data_shards).collect();
        println!(
            "frame 1: data_shards={data_shards} shard_indices={}..={} count={} parity_present={:?} pkt_sizes={:?}",
            indices.iter().next().copied().unwrap_or(0),
            max_idx,
            indices.len(),
            parity_present,
            sizes
        );
    }

    /// THE decisive FEC test: take frame 1's real captured shards (35 data + 7
    /// parity), drop a data shard, recover it from parity, and assert it matches
    /// the real one byte-for-byte. This proves `reed-solomon-erasure`'s matrix is
    /// compatible with Sunshine's `nanors` encoder (if it fails, we need a
    /// matrix-matched decoder).
    ///
    /// Acceptance test for the `nanors`-compatible Cauchy RS decoder.
    #[test]
    fn fec_recovers_dropped_data_shard_matching_real_bytes() {
        const BLOCKSIZE: usize = 1376; // 1408-byte packet − 32-byte header
        let data_shards = 35usize;
        let parity_shards = 7usize;
        let pkts = load_fixture();

        let mut shards: Vec<Option<Vec<u8>>> = vec![None; data_shards + parity_shards];
        for p in &pkts {
            let h = rtp::parse_header(p).expect("parse");
            if h.frame_index == 1 {
                let idx = h.shard_index as usize;
                let end = rtp::PAYLOAD_OFFSET + BLOCKSIZE;
                if idx < shards.len() && end <= p.len() {
                    shards[idx] = Some(p[rtp::PAYLOAD_OFFSET..end].to_vec());
                }
            }
        }
        assert!(shards.iter().all(|s| s.is_some()), "need all 42 real shards");

        let dropped = 10usize;
        let original = shards[dropped].clone().unwrap();
        shards[dropped] = None;

        let ok = super::fec::recover(data_shards, parity_shards, &mut shards);
        assert!(ok, "FEC recover() returned false");
        let recovered = shards[dropped].as_ref().expect("slot filled");
        let matches = recovered == &original;
        println!(
            "FEC recover matches real bytes: {matches} (recovered[..8]={:02x?} original[..8]={:02x?})",
            &recovered[..8],
            &original[..8]
        );
        assert!(matches, "recovered shard != Sunshine's bytes");
    }

    /// Drop the MAXIMUM recoverable number of data shards (= parity count) and
    /// confirm every one is restored byte-for-byte.
    #[test]
    fn fec_recovers_max_drops() {
        const BLOCKSIZE: usize = 1376;
        let (data_shards, parity_shards) = (35usize, 7usize);
        let pkts = load_fixture();
        let mut full: Vec<Option<Vec<u8>>> = vec![None; data_shards + parity_shards];
        for p in &pkts {
            let h = rtp::parse_header(p).expect("parse");
            if h.frame_index == 1 {
                let end = rtp::PAYLOAD_OFFSET + BLOCKSIZE;
                if (h.shard_index as usize) < full.len() && end <= p.len() {
                    full[h.shard_index as usize] = Some(p[rtp::PAYLOAD_OFFSET..end].to_vec());
                }
            }
        }
        assert!(full.iter().all(|s| s.is_some()));
        // Drop `parity_shards` data shards (spread out) — the recovery limit.
        let drops = [0usize, 5, 12, 18, 24, 30, 34];
        let originals: Vec<Vec<u8>> = drops.iter().map(|&d| full[d].clone().unwrap()).collect();
        let mut shards = full.clone();
        for &d in &drops {
            shards[d] = None;
        }
        assert!(super::fec::recover(data_shards, parity_shards, &mut shards));
        for (k, &d) in drops.iter().enumerate() {
            assert_eq!(shards[d].as_ref().unwrap(), &originals[k], "drop {d} mismatch");
        }
    }

    /// End-to-end: feed frame 1's packets through the Depacketizer but DROP two
    /// data-shard packets; FEC must heal it and emit the identical IDR bytes as
    /// the lossless run.
    #[test]
    fn depacketizer_heals_lossy_frame_via_fec() {
        use super::reassembly::Depacketizer;
        let pkts = load_fixture();
        let frame1: Vec<&Vec<u8>> = pkts
            .iter()
            .filter(|p| rtp::parse_header(p).map(|h| h.frame_index == 1).unwrap_or(false))
            .collect();

        // Lossless reference.
        let mut d0 = Depacketizer::new(Codec::Hevc);
        let mut reference = None;
        for p in &frame1 {
            if let Some(au) = d0.push(p) {
                reference = Some(au);
            }
        }
        let reference = reference.expect("lossless frame");

        // Lossy: drop two data-shard packets (shard 3 and 20).
        let mut d1 = Depacketizer::new(Codec::Hevc);
        let mut healed = None;
        for p in &frame1 {
            let h = rtp::parse_header(p).unwrap();
            if h.shard_index == 3 || h.shard_index == 20 {
                continue; // simulate packet loss
            }
            if let Some(au) = d1.push(p) {
                healed = Some(au);
            }
        }
        let healed = healed.expect("FEC-healed frame should still emit");
        assert_eq!(healed.data, reference.data, "FEC-healed frame must match lossless bytes");
        assert!(healed.is_keyframe);
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

/// Reed-Solomon FEC — docs/protocol/07 §2, the bit-exact core. The host
/// (Sunshine) encodes parity with the `nanors` GF(2^8) Reed-Solomon library;
/// recovery must use a matrix-compatible decoder or it silently corrupts frames.
/// Geometry (from the Sunshine server FEC source): data shards `0..k-1` then
/// parity `k..k+m-1`, all padded to a fixed blocksize; `m = ceil(k*pct/100)`
/// (floored to a minimum), up to 4 independent FEC blocks per frame.
pub mod fec {
    use std::sync::OnceLock;

    /// GF(2^8) with primitive polynomial `0x11d` and generator `2` — matches the
    /// `nanors` library Sunshine encodes with, so recovery is byte-compatible.
    struct Gf256 {
        /// `exp[i] = g^i` (doubled to 512 so `log[a]+log[b]` never wraps).
        exp: [u8; 512],
        log: [u8; 256],
        /// Multiplicative inverse table (`inv[0]` unused).
        inv: [u8; 256],
    }

    impl Gf256 {
        fn build() -> Self {
            let mut exp = [0u8; 512];
            let mut log = [0u8; 256];
            let mut x: u16 = 1;
            for (i, slot) in exp.iter_mut().take(255).enumerate() {
                *slot = x as u8;
                log[x as usize] = i as u8;
                x <<= 1;
                if x & 0x100 != 0 {
                    x ^= 0x11d;
                }
            }
            for i in 255..512 {
                exp[i] = exp[i - 255];
            }
            let mut inv = [0u8; 256];
            for a in 1..256usize {
                inv[a] = exp[255 - log[a] as usize];
            }
            Self { exp, log, inv }
        }

        #[inline]
        fn mul(&self, a: u8, b: u8) -> u8 {
            if a == 0 || b == 0 {
                0
            } else {
                self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
            }
        }

        #[inline]
        fn inverse(&self, a: u8) -> u8 {
            self.inv[a as usize]
        }
    }

    fn gf() -> &'static Gf256 {
        static GF: OnceLock<Gf256> = OnceLock::new();
        GF.get_or_init(Gf256::build)
    }

    /// Cauchy parity coefficient for parity row `j`, data column `i`:
    /// `1 / ((parity_shards + i) XOR j)` in GF(2^8). [matches `nanors` rs_new:
    /// `GF2_8_INV[(ps + i) ^ j]`].
    fn parity_coeff(parity_shards: usize, i: usize, j: usize) -> u8 {
        gf().inverse(((parity_shards + i) ^ j) as u8)
    }

    /// Invert a `k×k` GF(2^8) matrix (row-major) in place via Gauss-Jordan.
    /// Returns `false` if singular.
    fn invert(m: &mut [u8], k: usize) -> bool {
        let g = gf();
        let mut inv = vec![0u8; k * k];
        for i in 0..k {
            inv[i * k + i] = 1;
        }
        for col in 0..k {
            let mut piv = col;
            while piv < k && m[piv * k + col] == 0 {
                piv += 1;
            }
            if piv == k {
                return false; // singular
            }
            if piv != col {
                for c in 0..k {
                    m.swap(piv * k + c, col * k + c);
                    inv.swap(piv * k + c, col * k + c);
                }
            }
            let pvi = g.inverse(m[col * k + col]);
            for c in 0..k {
                m[col * k + c] = g.mul(m[col * k + c], pvi);
                inv[col * k + c] = g.mul(inv[col * k + c], pvi);
            }
            for r in 0..k {
                if r == col {
                    continue;
                }
                let f = m[r * k + col];
                if f == 0 {
                    continue;
                }
                for c in 0..k {
                    m[r * k + c] ^= g.mul(f, m[col * k + c]);
                    inv[r * k + c] ^= g.mul(f, inv[col * k + c]);
                }
            }
        }
        m.copy_from_slice(&inv);
        true
    }

    /// Recover missing data shards in one FEC block from the parity shards,
    /// byte-compatible with Sunshine's `nanors` systematic Cauchy RS code.
    /// `shards` has `data_shards + parity_shards` slots (`None` = lost); all
    /// present shards must be the same byte length. On success every data-shard
    /// slot is filled with the host's real bytes. Returns `false` if fewer than
    /// `data_shards` shards are present (unrecoverable) or the matrix is singular.
    ///
    /// Only runs on actual loss; the no-loss reassembly path never calls it.
    pub fn recover(
        data_shards: usize,
        parity_shards: usize,
        shards: &mut [Option<Vec<u8>>],
    ) -> bool {
        let k = data_shards;
        if shards.len() < k + parity_shards || shards.iter().filter(|s| s.is_some()).count() < k {
            return false;
        }
        if (0..k).all(|i| shards[i].is_some()) {
            return true; // all data shards already present
        }
        let len = match shards.iter().flatten().next() {
            Some(s) => s.len(),
            None => return false,
        };

        // The k surviving rows of the systematic generator F = [I_k ; Cauchy].
        let rows: Vec<usize> = shards
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .map(|(i, _)| i)
            .take(k)
            .collect();
        let present: Vec<Vec<u8>> = rows.iter().filter_map(|&r| shards[r].clone()).collect();

        // Build M = F[rows] (k×k): data rows are identity, parity rows are Cauchy.
        let mut mat = vec![0u8; k * k];
        for (r, &sh) in rows.iter().enumerate() {
            if sh < k {
                mat[r * k + sh] = 1;
            } else {
                let j = sh - k;
                for (c, slot) in mat[r * k..r * k + k].iter_mut().enumerate() {
                    *slot = parity_coeff(parity_shards, c, j);
                }
            }
        }
        if !invert(&mut mat, k) {
            return false;
        }

        // data = M⁻¹ · present (per byte), filling only the missing data shards.
        let g = gf();
        for d in 0..k {
            if shards[d].is_some() {
                continue;
            }
            let mut out = vec![0u8; len];
            for (r, src) in present.iter().enumerate() {
                let coeff = mat[d * k + r];
                if coeff == 0 {
                    continue;
                }
                for (o, &s) in out.iter_mut().zip(src.iter()) {
                    *o ^= g.mul(coeff, s);
                }
            }
            shards[d] = Some(out);
        }
        true
    }
}

/// Frame reassembly — docs/protocol/07 §3. Reorders fragments by shard index,
/// assembles frames, and emits a complete [`AccessUnit`]. FEC recovery for
/// missing data shards is layered on top in [`super::fec`].
pub mod reassembly {
    use super::rtp::{self, FLAG_PIC_DATA};
    use super::{AccessUnit, Codec};

    /// HEVC frame type 2 = IDR (keyframe), from the SOF short-frame header.
    const FRAME_TYPE_IDR: u8 = 2;

    /// Streaming reassembler. Feed every received video datagram via [`push`];
    /// it yields an [`AccessUnit`] when a frame is recoverable (all data shards
    /// present, or enough data+parity shards to FEC-recover the missing ones).
    ///
    /// Perf: shard payloads are sliced (`bytes[32..]`) and slotted directly by
    /// shard index — no hot-path sort. FEC (the per-byte GF(2^8) matrix solve)
    /// runs only on actual loss; the clean path never touches it.
    ///
    /// Note: assumes a single FEC block per frame (`multiFecBlocks == 0`), which
    /// holds for Sunshine up to ~255 shards/frame. Multi-block frames are a TODO.
    ///
    /// [`push`]: Depacketizer::push
    pub struct Depacketizer {
        codec: Codec,
        frame_index: Option<u32>,
        data_shards: usize,
        parity_shards: usize,
        /// Per-shard payload (`bytes[32..]`), data `0..k` then parity `k..k+m`.
        shards: Vec<Option<Vec<u8>>>,
        /// Total shards received (data + parity).
        received: usize,
        /// Data shards received (drives the no-loss fast path).
        received_data: usize,
        /// Set once the frame has been emitted, to ignore trailing packets.
        emitted: bool,
    }

    impl Depacketizer {
        pub fn new(codec: Codec) -> Self {
            Self {
                codec,
                frame_index: None,
                data_shards: 0,
                parity_shards: 0,
                shards: Vec::new(),
                received: 0,
                received_data: 0,
                emitted: false,
            }
        }

        fn reset_for(&mut self, frame_index: u32, data_shards: usize, parity_shards: usize) {
            self.frame_index = Some(frame_index);
            self.data_shards = data_shards;
            self.parity_shards = parity_shards;
            self.shards.clear();
            self.shards.resize(data_shards + parity_shards, None);
            self.received = 0;
            self.received_data = 0;
            self.emitted = false;
        }

        /// Feed one received datagram. Returns a completed [`AccessUnit`] as soon
        /// as the frame is recoverable. Late/duplicate, non-picture, or
        /// already-emitted-frame packets return `None`.
        pub fn push(&mut self, pkt: &[u8]) -> Option<AccessUnit> {
            let h = rtp::parse_header(pkt)?;
            if h.flags & FLAG_PIC_DATA == 0 || h.data_shards == 0 {
                return None;
            }
            if self.frame_index != Some(h.frame_index) {
                // parity = ceil(data*pct/100); the host encodes the final pct so
                // this also recovers the min-floored count. (See Sunshine FEC.)
                let parity = (h.data_shards as usize * h.fec_percentage as usize).div_ceil(100);
                self.reset_for(h.frame_index, h.data_shards as usize, parity);
            }
            if self.emitted {
                return None; // trailing shard of a frame we already delivered
            }
            let slot = h.shard_index as usize;
            if slot < self.shards.len() && self.shards[slot].is_none() {
                self.shards[slot] = Some(pkt[rtp::PAYLOAD_OFFSET..].to_vec());
                self.received += 1;
                if slot < self.data_shards {
                    self.received_data += 1;
                }
            }
            // `data_shards` shards (any mix) are enough to reconstruct the frame.
            if self.received >= self.data_shards {
                return self.finalize();
            }
            None
        }

        fn finalize(&mut self) -> Option<AccessUnit> {
            // Recover missing data shards from parity if needed (loss path only).
            if self.received_data < self.data_shards
                && !super::fec::recover(self.data_shards, self.parity_shards, &mut self.shards)
            {
                return None; // not yet recoverable; retry as more shards arrive
            }

            // The frame header lives at the front of shard 0 (now guaranteed
            // present, possibly via FEC) — robust to losing the SOF packet.
            let s0 = self.shards[0].as_ref()?;
            let is_keyframe = s0.get(3).copied() == Some(FRAME_TYPE_IDR);
            let last_payload_len = s0
                .get(4..6)
                .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);

            let mut data = Vec::with_capacity(self.data_shards * 1376);
            for i in 0..self.data_shards {
                let bytes = self.shards[i].as_ref()?;
                if i + 1 == self.data_shards {
                    if let Some(n) = last_payload_len {
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
            self.emitted = true;
            Some(AccessUnit {
                codec: self.codec,
                frame_index: self.frame_index.unwrap_or(0),
                is_keyframe,
                data,
            })
        }
    }
}
