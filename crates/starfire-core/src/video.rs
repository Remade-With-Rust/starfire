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

impl Codec {
    /// Map a host-advertised codec string (the `<VideoCodec>` element in
    /// `/serverinfo`, or the `STARFIRE_CODEC` override) to a `Codec`.
    /// Case-insensitive; accepts the common aliases. Returns `None` for an
    /// unknown value so the caller can fall back to its default.
    pub fn from_wire(s: &str) -> Option<Codec> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hevc" | "h265" | "h.265" => Some(Codec::Hevc),
            "h264" | "h.264" | "avc" => Some(Codec::H264),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
}

/// One coded frame handed to the decoder (AV1 OBUs / HEVC|H.264 NALs).
/// The exact AU framing per codec is [CAPTURE-LOCKED].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub codec: Codec,
    pub frame_index: u32,
    pub is_keyframe: bool,
    /// Host-side frame processing latency reported in the SOF header
    /// (`video_short_frame_header_t.frame_processing_latency`), in 0.1 ms units
    /// (so `value / 10.0` ms) — the encoder/capture latency on the host.
    pub host_latency_tenths_ms: u16,
    pub data: Vec<u8>,
    /// How the frame arrived: timestamps and FEC accounting (instrumentation;
    /// never needed to decode the frame).
    pub meta: FrameMeta,
}

/// Arrival facts for one reassembled frame — the client's first two timeline
/// points (`first_packet_at`, `complete_at`) plus what FEC had to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameMeta {
    /// The host's 90 kHz capture timestamp for this frame (RTP timestamp).
    pub rtp_timestamp: u32,
    /// When the first packet of this frame reached us.
    pub first_packet_at: Option<std::time::Instant>,
    /// When the frame became decodable (last needed packet arrived).
    pub complete_at: Option<std::time::Instant>,
    /// Packets received for this frame before it completed.
    pub packets: u16,
    /// Data shards the frame is made of (across all FEC blocks).
    pub data_shards: u16,
    /// Data shards that were lost in transit and rebuilt from parity.
    pub recovered_shards: u16,
    /// FEC blocks the frame was split into (1 for all but very large frames).
    pub fec_blocks: u8,
}

/// RTP depacketization — docs/protocol/07 §1. The packet header layout lives in
/// the shared wire crate so the host writes exactly what the client parses.
pub use starfire_protocol::video::rtp;

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
/// (Sunshine) encodes parity with a systematic Cauchy code over GF(2^8);
/// recovery must use the same matrix or it silently corrupts frames.
///
/// The implementation lives in the shared wire crate
/// ([`starfire_protocol::fec`]) so the client's recovery and the host's parity
/// generation are one piece of code; this module is the client-facing name.
pub mod fec {
    pub use starfire_protocol::fec::{parity_count, scalar, Fec, MAX_BLOCK_SHARDS};

    /// One-shot recovery of the missing data shards in one FEC block (see
    /// [`Fec::recover`]). Builds a fresh coder each call — fine for tests and
    /// one-offs; the [`Depacketizer`](super::reassembly::Depacketizer) keeps its
    /// own [`Fec`] so steady-state recovery reuses the expanded tables.
    pub fn recover(
        data_shards: usize,
        parity_shards: usize,
        shards: &mut [Option<Vec<u8>>],
    ) -> bool {
        Fec::new().recover(data_shards, parity_shards, shards)
    }
}

/// Frame reassembly — docs/protocol/07 §3. Slots shards by index into a short
/// window of in-flight frames, recovers lost data shards with FEC, and emits
/// complete [`AccessUnit`]s strictly in frame order.
pub mod reassembly {
    use std::collections::VecDeque;
    use std::time::Instant;

    use super::fec::{Fec, MAX_BLOCK_SHARDS};
    use super::rtp::{self, VideoHeader, FLAG_PIC_DATA};
    use super::{AccessUnit, Codec, FrameMeta};

    /// HEVC/H.264 frame type 2 = IDR (keyframe), from the short-frame header.
    const FRAME_TYPE_IDR: u8 = 2;

    /// Frames that may be in flight at once. Packets of frame N+1 routinely
    /// overtake the tail of frame N on Wi-Fi, so the frame being assembled must
    /// not be the only one we remember; four is far more than reordering needs
    /// and bounds memory when a burst leaves several frames incomplete.
    const WINDOW_FRAMES: usize = 4;

    /// A frame index this far *behind* the newest delivered frame is not a late
    /// packet — the host restarted its counter. Reset and resynchronise.
    const RESTART_DISTANCE: u32 = 600;

    /// Recycled shard buffers kept for reuse (bounds the pool's memory).
    const POOL_LIMIT: usize = 1024;

    /// A FEC block may hold up to four independent blocks per frame (2-bit index).
    const MAX_BLOCKS: usize = 4;

    /// `a` is a later frame than `b` (wrap-safe).
    fn newer(a: u32, b: u32) -> bool {
        a != b && a.wrapping_sub(b) < 0x8000_0000
    }

    /// Running totals for one stream. All counts are exact, so they answer
    /// "what did the link do?" without a clock.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ReassemblyStats {
        /// Video packets accepted into a frame.
        pub packets: u64,
        /// Frames handed to the decoder.
        pub frames_delivered: u64,
        /// Delivered frames that needed FEC to complete.
        pub frames_recovered: u64,
        /// Data shards rebuilt from parity.
        pub shards_recovered: u64,
        /// Frames that could not be completed (declared lost).
        pub frames_lost: u64,
        /// Complete frames withheld because they depend on a lost frame.
        pub frames_skipped: u64,
        /// Packets for frames already delivered or given up on. On a clean
        /// link this is mostly parity arriving after its frame was complete
        /// (the frame is delivered the moment enough shards are in), so it runs
        /// at about the FEC percentage and is not a sign of trouble.
        pub late_packets: u64,
        /// Packets repeating a shard we already hold.
        pub duplicate_packets: u64,
        /// Packets whose header was inconsistent with their frame.
        pub malformed_packets: u64,
    }

    /// A run of consecutive frames that were lost in transit.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LossEvent {
        pub first_frame: u32,
        pub last_frame: u32,
    }

    impl LossEvent {
        /// Number of frames in the lost run.
        pub fn frames(&self) -> u32 {
            self.last_frame
                .wrapping_sub(self.first_frame)
                .wrapping_add(1)
        }
    }

    /// One FEC block of a frame in flight.
    #[derive(Default)]
    struct Block {
        /// 0 until the first packet of this block arrives.
        data_shards: usize,
        parity_shards: usize,
        fec_percentage: u8,
        /// Shard payloads (`bytes[32..]`), data `0..k` then parity `k..k+m`.
        shards: Vec<Option<Vec<u8>>>,
        received: usize,
        received_data: usize,
    }

    impl Block {
        fn known(&self) -> bool {
            self.data_shards != 0
        }
        /// Any `data_shards` of the block's shards reconstruct it.
        fn complete(&self) -> bool {
            self.known() && self.received >= self.data_shards
        }
    }

    /// A frame being assembled.
    struct Partial {
        frame_index: u32,
        blocks: Vec<Block>,
        first_packet_at: Instant,
        rtp_timestamp: u32,
        packets: u16,
        /// Single-block protected frames: the access unit being built in place.
        direct: Option<Direct>,
    }

    /// The access unit of a single-block, FEC-protected frame, built as its
    /// data shards arrive instead of being copied together at the end.
    ///
    /// Every shard of a protected block has the same length, so data shard `i`
    /// has a known place in the frame. Shards that arrive in order (nearly all
    /// of them on a working link) are appended straight into `data`; the rest
    /// wait in the block's slots as before and are appended once the gap before
    /// them closes. That removes the second copy of every byte of video -- the
    /// one that concatenated the shards into the access unit.
    struct Direct {
        /// Payload of data shards `0..contig`, without shard 0's 8-byte
        /// short-frame header.
        data: Vec<u8>,
        /// Length of every shard in this block.
        shard_len: usize,
        /// Data shards appended so far (always a prefix).
        contig: usize,
        /// Shard 0's short-frame header.
        sof: [u8; rtp::SHORT_FRAME_HEADER_LEN],
    }

    impl Direct {
        /// Append data shard `contig` (which must be `payload`'s index).
        fn append(&mut self, payload: &[u8]) {
            if self.contig == 0 {
                let (h, rest) = payload.split_at(rtp::SHORT_FRAME_HEADER_LEN);
                self.sof.copy_from_slice(h);
                self.data.extend_from_slice(rest);
            } else {
                self.data.extend_from_slice(payload);
            }
            self.contig += 1;
        }

        /// Data shard `i < contig`, as one contiguous slice -- except shard 0,
        /// whose header lives in `sof` (see [`Direct::shard0`]).
        fn shard(&self, i: usize) -> &[u8] {
            let at = (self.shard_len - rtp::SHORT_FRAME_HEADER_LEN) + (i - 1) * self.shard_len;
            &self.data[at..at + self.shard_len]
        }

        /// Shard 0 re-joined with its header (loss path / fallback only).
        fn shard0(&self) -> Vec<u8> {
            let body = self.shard_len - rtp::SHORT_FRAME_HEADER_LEN;
            [&self.sof[..], &self.data[..body]].concat()
        }

        /// Give the appended shards back to the block as ordinary slots: used
        /// when a shard of the wrong length shows the block is not uniform.
        fn spill(self, blk: &mut Block) {
            for i in 0..self.contig {
                blk.shards[i] = Some(if i == 0 {
                    self.shard0()
                } else {
                    self.shard(i).to_vec()
                });
            }
        }
    }

    impl Partial {
        fn complete(&self) -> bool {
            self.blocks.iter().all(Block::complete)
        }
    }

    /// Streaming reassembler. Feed every received video datagram via
    /// [`push`](Depacketizer::push); it yields an [`AccessUnit`] the moment a
    /// frame is recoverable (all data shards present, or enough data + parity
    /// to rebuild the missing ones).
    ///
    /// Guarantees:
    /// * **Order.** Frames come out in increasing frame index. A frame that is
    ///   still incomplete when a later one completes is declared lost — its
    ///   packets were sent a whole frame interval earlier, so they are not
    ///   coming — and reported through [`take_loss`](Depacketizer::take_loss).
    /// * **Reorder tolerance.** Packets of the next frame may overtake the tail
    ///   of the current one, and stale packets of old frames are ignored; neither
    ///   disturbs a frame in progress.
    /// * **No steady-state allocation per packet.** Shard buffers are recycled.
    ///
    /// With the keyframe gate on ([`gate_on_keyframes`](Depacketizer::gate_on_keyframes))
    /// it also withholds frames that depend on a lost one, so the decoder is
    /// never handed a frame whose reference is missing; delivery resumes at the
    /// next keyframe.
    pub struct Depacketizer {
        codec: Codec,
        /// In-flight frames, oldest first.
        window: VecDeque<Partial>,
        /// Newest frame index delivered or given up on; older packets are late.
        floor: Option<u32>,
        gate: bool,
        awaiting_keyframe: bool,
        pending_loss: Option<LossEvent>,
        fec: Fec,
        pool: Vec<Vec<u8>>,
        stats: ReassemblyStats,
    }

    impl Depacketizer {
        /// A pure reassembler: every complete frame is delivered.
        pub fn new(codec: Codec) -> Self {
            Self {
                codec,
                window: VecDeque::with_capacity(WINDOW_FRAMES + 1),
                floor: None,
                gate: false,
                awaiting_keyframe: false,
                pending_loss: None,
                fec: Fec::new(),
                pool: Vec::new(),
                stats: ReassemblyStats::default(),
            }
        }

        /// Turn the keyframe gate on or off (off by default). With it on, the
        /// stream starts at the first keyframe, and after any lost frame nothing
        /// is delivered until the next keyframe — what a player wants, since a
        /// predicted frame without its reference decodes to garbage.
        pub fn gate_on_keyframes(mut self, on: bool) -> Self {
            self.gate = on;
            self.awaiting_keyframe = on;
            self
        }

        /// Totals since the stream began.
        pub fn stats(&self) -> ReassemblyStats {
            self.stats
        }

        /// The FEC engine's reach counters (fast path vs scalar fallback).
        pub fn fec_blocks(&self) -> (u64, u64) {
            (self.fec.fast_blocks, self.fec.fallback_blocks)
        }

        /// The most recent run of lost frames, once. Call after each
        /// [`push`](Depacketizer::push); a `Some` means the host should be asked
        /// to recover (IDR / reference invalidation) right away.
        pub fn take_loss(&mut self) -> Option<LossEvent> {
            self.pending_loss.take()
        }

        /// True while the keyframe gate is closed (frames are being withheld
        /// until a keyframe arrives). The caller should keep asking for one.
        pub fn awaiting_keyframe(&self) -> bool {
            self.awaiting_keyframe
        }

        /// Close the keyframe gate from outside: the decoder has lost its
        /// references (it failed on frames that arrived intact), so stop feeding
        /// it predicted frames until a keyframe arrives. No-op with the gate off.
        pub fn require_keyframe(&mut self) {
            if self.gate {
                self.awaiting_keyframe = true;
            }
        }

        /// Feed one received datagram, stamped with the current time.
        pub fn push(&mut self, pkt: &[u8]) -> Option<AccessUnit> {
            self.push_at(pkt, Instant::now())
        }

        /// Feed one received datagram that arrived at `now`. Returns a completed
        /// [`AccessUnit`] as soon as its frame is recoverable; late, duplicate,
        /// malformed or non-picture packets return `None`.
        pub fn push_at(&mut self, pkt: &[u8], now: Instant) -> Option<AccessUnit> {
            let h = rtp::parse_header(pkt)?;
            if h.data_shards == 0 {
                return None; // not a picture packet
            }
            // A data shard states PIC_DATA explicitly. A parity shard's `flags`
            // byte is not meaningful (Sunshine leaves it as coded bytes; only
            // frameIndex and fecInfo are set), so accept parity on its index.
            if !h.is_parity() && h.flags & FLAG_PIC_DATA == 0 {
                return None;
            }
            let (k, m) = (h.data_shards as usize, h.parity_shards());
            let (block, last_block) = (h.fec_block as usize, h.fec_last_block as usize);
            if h.shard_index as usize >= k + m
                || (m > 0 && k + m > MAX_BLOCK_SHARDS)
                || block > last_block
                || last_block >= MAX_BLOCKS
            {
                self.stats.malformed_packets += 1;
                return None;
            }

            if let Some(floor) = self.floor {
                if !newer(h.frame_index, floor) {
                    if floor.wrapping_sub(h.frame_index) > RESTART_DISTANCE {
                        self.restart(); // the host reset its frame counter
                    } else {
                        self.stats.late_packets += 1;
                        return None;
                    }
                }
            }

            let payload = &pkt[rtp::PAYLOAD_OFFSET..];
            let Some(pos) = self.slot_for(&h, payload.len(), now) else {
                self.stats.late_packets += 1;
                return None;
            };
            let frame = &mut self.window[pos];
            if frame.blocks.len() != last_block + 1 {
                self.stats.malformed_packets += 1;
                return None;
            }
            let Partial {
                blocks,
                direct,
                packets,
                ..
            } = frame;
            let blk = &mut blocks[block];
            if !blk.known() {
                blk.data_shards = k;
                blk.parity_shards = m;
                blk.fec_percentage = h.fec_percentage;
                blk.shards.resize_with(k + m, || None);
            } else if blk.data_shards != k || blk.fec_percentage != h.fec_percentage {
                self.stats.malformed_packets += 1;
                return None;
            }
            let slot = h.shard_index as usize;
            if direct.as_ref().is_some_and(|d| slot < d.contig) || blk.shards[slot].is_some() {
                self.stats.duplicate_packets += 1;
                return None;
            }
            // A data shard of a different length shows the block is not uniform:
            // hand the in-place shards back and assemble the classic way.
            if slot < k
                && direct
                    .as_ref()
                    .is_some_and(|d| payload.len() != d.shard_len)
            {
                if let Some(d) = direct.take() {
                    d.spill(blk);
                }
            }
            match direct.as_mut() {
                Some(d) if slot == d.contig && slot < k => {
                    d.append(payload);
                    // Close the gap: shards that arrived early follow in order.
                    while d.contig < k {
                        let Some(buf) = blk.shards[d.contig].take() else {
                            break;
                        };
                        d.append(&buf);
                        if self.pool.len() < POOL_LIMIT {
                            self.pool.push(buf);
                        }
                    }
                }
                _ => {
                    let mut buf = self.pool.pop().unwrap_or_default();
                    buf.clear();
                    buf.extend_from_slice(payload);
                    blk.shards[slot] = Some(buf);
                }
            }
            blk.received += 1;
            if slot < k {
                blk.received_data += 1;
            }
            *packets = packets.saturating_add(1);
            self.stats.packets += 1;

            if self.window[pos].complete() {
                self.finish(pos, now)
            } else {
                None
            }
        }

        /// Index in the window of the frame this packet belongs to, creating it
        /// if needed. When the window is full the oldest in-flight frame is
        /// given up to make room; a packet for a frame older than everything in
        /// a full window is stale and gets `None`.
        fn slot_for(&mut self, h: &VideoHeader, shard_len: usize, now: Instant) -> Option<usize> {
            if let Some(pos) = self
                .window
                .iter()
                .position(|f| f.frame_index == h.frame_index)
            {
                return Some(pos);
            }
            if self.window.len() >= WINDOW_FRAMES {
                let oldest = self.window.front()?.frame_index;
                if !newer(h.frame_index, oldest) {
                    return None;
                }
                // Overtaken by a full window of newer frames and still not
                // complete: its packets are not coming.
                if let Some(old) = self.window.pop_front() {
                    self.recycle(old);
                }
                self.give_up_through(oldest);
            }
            let mut blocks = Vec::with_capacity(h.fec_last_block as usize + 1);
            blocks.resize_with(h.fec_last_block as usize + 1, Block::default);
            // A single protected block: every shard is `shard_len` long, so the
            // access unit can be built in place (see `Direct`).
            let k = h.data_shards as usize;
            let direct = (h.fec_last_block == 0
                && h.parity_shards() > 0
                && shard_len >= rtp::SHORT_FRAME_HEADER_LEN)
                .then(|| Direct {
                    data: Vec::with_capacity(k * shard_len - rtp::SHORT_FRAME_HEADER_LEN),
                    shard_len,
                    contig: 0,
                    sof: [0; rtp::SHORT_FRAME_HEADER_LEN],
                });
            let partial = Partial {
                frame_index: h.frame_index,
                blocks,
                first_packet_at: now,
                rtp_timestamp: h.rtp_timestamp,
                packets: 0,
                direct,
            };
            let pos = self
                .window
                .iter()
                .position(|f| newer(f.frame_index, h.frame_index))
                .unwrap_or(self.window.len());
            self.window.insert(pos, partial);
            Some(pos)
        }

        /// Give up on every frame after the floor up to and including `index`
        /// (they are lost), and move the floor there.
        fn give_up_through(&mut self, index: u32) {
            let first = match self.floor {
                Some(floor) => floor.wrapping_add(1),
                None => index,
            };
            self.declare_lost(first, index);
            self.floor = Some(index);
        }

        /// The frame at `pos` is recoverable: retire everything older, rebuild
        /// lost shards, and assemble its access unit.
        fn finish(&mut self, pos: usize, now: Instant) -> Option<AccessUnit> {
            let mut frame = self.window.remove(pos)?;
            let index = frame.frame_index;

            // Everything older than a completed frame is lost: frames still
            // incomplete in the window, and frames that never produced a packet.
            let mut first_in_window = None;
            while self
                .window
                .front()
                .is_some_and(|f| newer(index, f.frame_index))
            {
                if let Some(old) = self.window.pop_front() {
                    first_in_window.get_or_insert(old.frame_index);
                    self.recycle(old);
                }
            }
            let first_lost = match self.floor {
                Some(floor) => Some(floor.wrapping_add(1)),
                None => first_in_window, // joined mid-stream: only count what we saw
            };
            if let Some(first) = first_lost {
                if first != index {
                    self.declare_lost(first, index.wrapping_sub(1));
                }
            }
            self.floor = Some(index);

            let (au, recovered) = match frame.direct.take() {
                Some(d) => match self.complete_direct(&mut frame, d, now) {
                    Some(done) => done,
                    None => {
                        self.recycle(frame);
                        self.declare_lost(index, index);
                        return None;
                    }
                },
                None => {
                    // Rebuild missing data shards from parity (loss path only).
                    let mut recovered = 0usize;
                    for blk in &mut frame.blocks {
                        if blk.received_data < blk.data_shards {
                            let missing = blk.data_shards - blk.received_data;
                            if !self.fec.recover(
                                blk.data_shards,
                                blk.parity_shards,
                                &mut blk.shards,
                            ) {
                                self.recycle(frame);
                                self.declare_lost(index, index);
                                return None;
                            }
                            recovered += missing;
                        }
                    }
                    (self.assemble(&frame, recovered, now), recovered)
                }
            };
            self.recycle(frame);
            let au = match au {
                Some(au) => au,
                None => {
                    self.stats.malformed_packets += 1;
                    self.declare_lost(index, index);
                    return None;
                }
            };

            if self.awaiting_keyframe {
                if au.is_keyframe {
                    self.awaiting_keyframe = false;
                } else {
                    self.stats.frames_skipped += 1;
                    return None;
                }
            }
            self.stats.frames_delivered += 1;
            if recovered > 0 {
                self.stats.frames_recovered += 1;
                self.stats.shards_recovered += recovered as u64;
            }
            Some(au)
        }

        /// Finish a frame built in place: recover any missing data shards from
        /// views into the buffer, append the shards still waiting in their
        /// slots, trim the last shard's padding. `None` if recovery fails.
        fn complete_direct(
            &mut self,
            frame: &mut Partial,
            mut d: Direct,
            now: Instant,
        ) -> Option<(Option<AccessUnit>, usize)> {
            let blk = frame.blocks.first_mut()?;
            let (k, m, len) = (blk.data_shards, blk.parity_shards, d.shard_len);
            let mut rebuilt: Vec<(usize, Vec<u8>)> = Vec::new();
            if blk.received_data < k {
                let head = (d.contig > 0).then(|| d.shard0());
                let view: Vec<Option<&[u8]>> = (0..k + m)
                    .map(|i| {
                        if i == 0 && d.contig > 0 {
                            head.as_deref()
                        } else if i < d.contig {
                            Some(d.shard(i))
                        } else {
                            blk.shards[i].as_deref()
                        }
                    })
                    .collect();
                rebuilt = self.fec.recover_views(k, m, &view)?;
            }
            let recovered = rebuilt.len();
            let mut rebuilt = rebuilt.into_iter();
            while d.contig < k {
                match blk.shards[d.contig].take() {
                    Some(buf) => {
                        d.append(&buf);
                        if self.pool.len() < POOL_LIMIT {
                            self.pool.push(buf);
                        }
                    }
                    None => {
                        let (_, buf) = rebuilt.next()?;
                        if buf.len() != len {
                            return None;
                        }
                        d.append(&buf);
                    }
                }
            }
            // lastPayloadLen: the real bytes in the final (padded) data shard.
            let sof = d.sof;
            let last_payload_len = u16::from_le_bytes([sof[4], sof[5]]) as usize;
            let end_last = if last_payload_len > 0 {
                last_payload_len.min(len)
            } else {
                len
            };
            let header = rtp::SHORT_FRAME_HEADER_LEN;
            let total = if k == 1 {
                end_last.max(header) - header
            } else {
                (len - header) + (k - 2) * len + end_last
            };
            d.data.truncate(total);
            Some((
                Some(AccessUnit {
                    codec: self.codec,
                    frame_index: frame.frame_index,
                    is_keyframe: sof[3] == FRAME_TYPE_IDR,
                    host_latency_tenths_ms: u16::from_le_bytes([sof[1], sof[2]]),
                    data: d.data,
                    meta: FrameMeta {
                        rtp_timestamp: frame.rtp_timestamp,
                        first_packet_at: Some(frame.first_packet_at),
                        complete_at: Some(now),
                        packets: frame.packets,
                        data_shards: k.min(u16::MAX as usize) as u16,
                        recovered_shards: recovered.min(u16::MAX as usize) as u16,
                        fec_blocks: 1,
                    },
                }),
                recovered,
            ))
        }

        /// Concatenate the frame's data shards (block 0 first) into one access
        /// unit, dropping the 8-byte short-frame header and the final shard's
        /// padding.
        fn assemble(&self, frame: &Partial, recovered: usize, now: Instant) -> Option<AccessUnit> {
            // The frame header lives at the front of block 0 / shard 0 (now
            // guaranteed present, possibly via FEC) — robust to losing the SOF.
            let s0 = frame.blocks.first()?.shards.first()?.as_ref()?;
            if s0.len() < rtp::SHORT_FRAME_HEADER_LEN {
                return None;
            }
            let is_keyframe = s0[3] == FRAME_TYPE_IDR;
            // video_short_frame_header_t.frame_processing_latency (LE u16 @ +1).
            let host_latency = u16::from_le_bytes([s0[1], s0[2]]);
            // lastPayloadLen (LE u16 @ +4): real bytes in the final data shard.
            let last_payload_len = u16::from_le_bytes([s0[4], s0[5]]) as usize;

            let total_data: usize = frame.blocks.iter().map(|b| b.data_shards).sum();
            let mut data = Vec::with_capacity(total_data * s0.len());
            let last_block = frame.blocks.len() - 1;
            for (b, blk) in frame.blocks.iter().enumerate() {
                for i in 0..blk.data_shards {
                    let bytes = blk.shards[i].as_ref()?;
                    let first = b == 0 && i == 0;
                    let last = b == last_block && i + 1 == blk.data_shards;
                    let start = if first {
                        rtp::SHORT_FRAME_HEADER_LEN
                    } else {
                        0
                    };
                    let end = if last && last_payload_len > 0 {
                        last_payload_len.min(bytes.len())
                    } else {
                        bytes.len()
                    };
                    data.extend_from_slice(bytes.get(start..end.max(start))?);
                }
            }
            Some(AccessUnit {
                codec: self.codec,
                frame_index: frame.frame_index,
                is_keyframe,
                host_latency_tenths_ms: host_latency,
                data,
                meta: FrameMeta {
                    rtp_timestamp: frame.rtp_timestamp,
                    first_packet_at: Some(frame.first_packet_at),
                    complete_at: Some(now),
                    packets: frame.packets,
                    data_shards: total_data.min(u16::MAX as usize) as u16,
                    recovered_shards: recovered.min(u16::MAX as usize) as u16,
                    fec_blocks: frame.blocks.len() as u8,
                },
            })
        }

        /// Record `first..=last` as lost and close the keyframe gate.
        fn declare_lost(&mut self, first: u32, last: u32) {
            let run = LossEvent {
                first_frame: first,
                last_frame: last,
            };
            self.stats.frames_lost += run.frames() as u64;
            // Merge with an unread event so the caller sees one range.
            self.pending_loss = Some(match self.pending_loss {
                Some(prev) => LossEvent {
                    first_frame: prev.first_frame,
                    last_frame: last,
                },
                None => run,
            });
            if self.gate {
                self.awaiting_keyframe = true;
            }
        }

        /// Return a retired frame's shard buffers to the pool.
        fn recycle(&mut self, frame: Partial) {
            for blk in frame.blocks {
                for buf in blk.shards.into_iter().flatten() {
                    if self.pool.len() < POOL_LIMIT {
                        self.pool.push(buf);
                    }
                }
            }
        }

        /// Forget all in-flight state (the host restarted its frame counter).
        fn restart(&mut self) {
            while let Some(f) = self.window.pop_front() {
                self.recycle(f);
            }
            self.floor = None;
            self.awaiting_keyframe = self.gate;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod testwire;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod reassembly_tests;
