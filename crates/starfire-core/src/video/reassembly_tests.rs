// SPDX-License-Identifier: Apache-2.0
//! Reassembly under loss and reordering — the conditions a Wi-Fi link produces.

use std::time::{Duration, Instant};

use super::reassembly::{Depacketizer, LossEvent, ReassemblyStats};
use super::testwire::{frame, load_fixture, nals};
use super::*;

const BS: usize = 1360;

fn feed<'a>(
    dep: &mut Depacketizer,
    pkts: impl IntoIterator<Item = &'a Vec<u8>>,
) -> Vec<AccessUnit> {
    pkts.into_iter().filter_map(|p| dep.push(p)).collect()
}

fn indices(aus: &[AccessUnit]) -> Vec<u32> {
    aus.iter().map(|au| au.frame_index).collect()
}

/// Drop the packets at `drops` (positions in send order) from a frame.
fn without(pkts: &[Vec<u8>], drops: &[usize]) -> Vec<Vec<u8>> {
    pkts.iter()
        .enumerate()
        .filter(|(i, _)| !drops.contains(i))
        .map(|(_, p)| p.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Reordering
// ---------------------------------------------------------------------------

/// Sunshine sends 7 parity shards with the captured IDR (35 data, 20 %), so the
/// frame must survive the loss of any 7 packets. The parity packets carry
/// arbitrary bytes in the NV `flags` field (only `frameIndex` and `fecInfo` are
/// meaningful on a parity shard), so they must be accepted on their shard
/// index, not on the PIC_DATA flag.
#[test]
fn every_captured_sunshine_parity_shard_is_usable() {
    let pkts = load_fixture();
    let frame1: Vec<&Vec<u8>> = pkts
        .iter()
        .filter(|p| {
            rtp::parse_header(p)
                .map(|h| h.frame_index == 1)
                .unwrap_or(false)
        })
        .collect();
    let mut clean = Depacketizer::new(Codec::Hevc);
    let reference = frame1
        .iter()
        .filter_map(|p| clean.push(p))
        .next()
        .expect("lossless frame");

    // Drop 7 data shards: recovery needs all 7 captured parity shards.
    let drops = [0u16, 5, 12, 18, 24, 30, 34];
    let mut lossy = Depacketizer::new(Codec::Hevc);
    let healed = frame1
        .iter()
        .filter(|p| !drops.contains(&rtp::parse_header(p).unwrap().shard_index))
        .filter_map(|p| lossy.push(p))
        .next()
        .expect("7 lost data shards must be healed by the 7 parity shards");
    assert_eq!(healed.data, reference.data);
    assert_eq!(healed.meta.recovered_shards, 7);
}

/// A packet of the previous frame that arrives late (reordered behind the next
/// frame's first packets) must not disturb the frame in progress.
#[test]
fn a_late_packet_of_the_previous_frame_does_not_reset_the_current_one() {
    let a = frame(10, true, &nals(10, 6 * BS), BS, 0, 1);
    let b = frame(11, false, &nals(11, 6 * BS), BS, 0, 1);
    let mut dep = Depacketizer::new(Codec::Hevc);
    let mut out = feed(&mut dep, &a);
    assert_eq!(indices(&out), vec![10], "frame 10 completes");
    // First half of frame 11, then a stray duplicate of frame 10's last packet,
    // then the rest of frame 11.
    out.extend(feed(&mut dep, &b[..3]));
    out.extend(feed(&mut dep, std::iter::once(a.last().unwrap())));
    out.extend(feed(&mut dep, &b[3..]));
    assert_eq!(
        indices(&out),
        vec![10, 11],
        "frame 11 must still be delivered"
    );
    assert_eq!(out[1].data, nals(11, 6 * BS));
}

/// The common Wi-Fi reorder: the first packet of frame N+1 overtakes the last
/// packet of frame N. Both frames are whole and must both be delivered, in order.
#[test]
fn packets_swapped_across_a_frame_boundary_deliver_both_frames() {
    let a = frame(20, true, &nals(20, 5 * BS), BS, 0, 1);
    let b = frame(21, false, &nals(21, 5 * BS), BS, 0, 1);
    let mut dep = Depacketizer::new(Codec::Hevc);
    let mut out = feed(&mut dep, &a[..a.len() - 1]);
    out.extend(feed(&mut dep, std::iter::once(&b[0]))); // overtakes ...
    out.extend(feed(&mut dep, std::iter::once(a.last().unwrap()))); // ... this
    out.extend(feed(&mut dep, &b[1..]));
    assert_eq!(indices(&out), vec![20, 21]);
    assert_eq!(out[0].data, nals(20, 5 * BS));
    assert_eq!(out[1].data, nals(21, 5 * BS));
}

// ---------------------------------------------------------------------------
// Loss accounting and recovery signalling
// ---------------------------------------------------------------------------

/// A frame that can no longer complete is reported exactly once, as soon as a
/// later frame completes — that report is what triggers the recovery request.
#[test]
fn an_unrecoverable_frame_is_reported_when_the_next_one_completes() {
    let f30 = frame(30, true, &nals(30, 4 * BS), BS, 0, 1);
    let f31 = frame(31, false, &nals(31, 4 * BS), BS, 0, 1); // no parity
    let f32 = frame(32, false, &nals(32, 4 * BS), BS, 0, 1);
    let mut dep = Depacketizer::new(Codec::Hevc);
    assert_eq!(indices(&feed(&mut dep, &f30)), vec![30]);
    assert_eq!(dep.take_loss(), None);

    // Frame 31 loses one packet and has no parity: it can never complete.
    assert!(feed(&mut dep, &without(&f31, &[2])).is_empty());
    assert_eq!(
        dep.take_loss(),
        None,
        "not lost until a later frame proves it"
    );

    assert_eq!(indices(&feed(&mut dep, &f32)), vec![32]);
    assert_eq!(
        dep.take_loss(),
        Some(LossEvent {
            first_frame: 31,
            last_frame: 31
        })
    );
    assert_eq!(dep.take_loss(), None, "reported once");
    assert_eq!(dep.stats().frames_lost, 1);
    assert_eq!(dep.stats().frames_delivered, 2);
}

/// Frames whose every packet was lost never enter the window; the gap in frame
/// indices must still be counted and reported.
#[test]
fn a_frame_that_vanished_entirely_is_still_counted_as_lost() {
    let f40 = frame(40, true, &nals(40, 2 * BS), BS, 20, 1);
    let f43 = frame(43, false, &nals(43, 2 * BS), BS, 20, 1);
    let mut dep = Depacketizer::new(Codec::Hevc);
    feed(&mut dep, &f40);
    assert_eq!(indices(&feed(&mut dep, &f43)), vec![43]);
    let loss = dep.take_loss().expect("41 and 42 never arrived");
    assert_eq!(
        (loss.first_frame, loss.last_frame, loss.frames()),
        (41, 42, 2)
    );
    assert_eq!(dep.stats().frames_lost, 2);
}

/// With the keyframe gate on, nothing that depends on a lost frame reaches the
/// decoder: delivery stops at the loss and resumes at the next keyframe.
#[test]
fn keyframe_gate_withholds_frames_until_the_stream_is_decodable_again() {
    let key = frame(50, true, &nals(50, 3 * BS), BS, 0, 1);
    let p51 = frame(51, false, &nals(51, 3 * BS), BS, 0, 1);
    let p52 = frame(52, false, &nals(52, 3 * BS), BS, 0, 1);
    let p53 = frame(53, false, &nals(53, 3 * BS), BS, 0, 1);
    let key54 = frame(54, true, &nals(54, 3 * BS), BS, 0, 1);
    let p55 = frame(55, false, &nals(55, 3 * BS), BS, 0, 1);

    let mut dep = Depacketizer::new(Codec::Hevc).gate_on_keyframes(true);
    let mut out = feed(&mut dep, &key);
    out.extend(feed(&mut dep, &without(&p51, &[1]))); // lost
    out.extend(feed(&mut dep, &p52)); // complete, but its reference is gone
    assert!(dep.awaiting_keyframe());
    out.extend(feed(&mut dep, &p53)); // same
    out.extend(feed(&mut dep, &key54)); // decodable again
    assert!(!dep.awaiting_keyframe());
    out.extend(feed(&mut dep, &p55));

    assert_eq!(indices(&out), vec![50, 54, 55]);
    assert_eq!(dep.stats().frames_skipped, 2, "52 and 53 were withheld");
    assert_eq!(dep.stats().frames_lost, 1);
}

/// The gate also covers the start of a stream: a predicted frame received
/// before any keyframe cannot be decoded, so it is withheld.
#[test]
fn keyframe_gate_starts_closed() {
    let p1 = frame(1, false, &nals(1, BS), BS, 0, 1);
    let k2 = frame(2, true, &nals(2, BS), BS, 0, 1);
    let mut gated = Depacketizer::new(Codec::Hevc).gate_on_keyframes(true);
    assert!(gated.awaiting_keyframe());
    assert!(feed(&mut gated, &p1).is_empty());
    assert_eq!(indices(&feed(&mut gated, &k2)), vec![2]);

    // Without the gate the reassembler is pure: every complete frame comes out.
    let mut plain = Depacketizer::new(Codec::Hevc);
    assert_eq!(indices(&feed(&mut plain, &p1)), vec![1]);
}

/// Frames are never delivered out of order: once frame N+1 has been delivered,
/// frame N's stragglers are late and must not produce a frame.
#[test]
fn a_frame_completing_after_its_successor_is_not_delivered() {
    let a = frame(60, true, &nals(60, 3 * BS), BS, 0, 1);
    let b = frame(61, false, &nals(61, 3 * BS), BS, 0, 1);
    let mut dep = Depacketizer::new(Codec::Hevc);
    let mut out = feed(&mut dep, &a[..a.len() - 1]);
    out.extend(feed(&mut dep, &b)); // 61 completes first => 60 is lost
    out.extend(feed(&mut dep, std::iter::once(a.last().unwrap())));
    assert_eq!(indices(&out), vec![61]);
    assert_eq!(dep.stats().late_packets, 1);
    assert_eq!(
        dep.take_loss(),
        Some(LossEvent {
            first_frame: 60,
            last_frame: 60
        })
    );
}

// ---------------------------------------------------------------------------
// FEC through the reassembler
// ---------------------------------------------------------------------------

/// Any `m` packets of a protected frame may be lost — data, parity, or the
/// first packet carrying the frame header — and the frame is byte-identical.
#[test]
fn any_loss_pattern_within_the_parity_budget_is_healed() {
    let payload = nals(70, 20 * BS - 100);
    let pkts = frame(70, true, &payload, BS, 20, 1); // k = 20, m = 4
    assert_eq!(pkts.len(), 24);
    for drops in [
        vec![0usize],         // the SOF / frame header
        vec![19],             // the last (padded) data shard
        vec![0, 1, 2, 3],     // a burst at the front
        vec![16, 17, 18, 19], // a burst at the tail of the data
        vec![3, 9, 21, 23],   // data + parity mixed
        vec![20, 21, 22, 23], // all parity (nothing to recover)
    ] {
        let mut dep = Depacketizer::new(Codec::Hevc);
        let out = feed(&mut dep, &without(&pkts, &drops));
        assert_eq!(indices(&out), vec![70], "drops {drops:?}");
        assert_eq!(out[0].data, payload, "drops {drops:?}");
        assert!(out[0].is_keyframe);
        assert_eq!(out[0].host_latency_tenths_ms, 42);
        let lost_data = drops.iter().filter(|&&d| d < 20).count();
        assert_eq!(
            out[0].meta.recovered_shards as usize, lost_data,
            "drops {drops:?}"
        );
        assert_eq!(dep.fec_blocks().1, 0, "recovery stays on the fast path");
    }
}

/// Data shards may arrive in any order, with or without loss, and the frame is
/// still byte-identical: shards that arrive early wait in their slots and are
/// placed once the gap before them closes (in-place assembly).
#[test]
fn single_block_shards_may_arrive_in_any_order() {
    let payload = nals(73, 20 * BS - 100);
    let pkts = frame(73, true, &payload, BS, 20, 1); // k = 20, m = 4
    let orders: Vec<Vec<usize>> = vec![
        (0..24).rev().collect(),      // everything reversed
        (1..24).chain([0]).collect(), // the header shard last
        vec![
            20, 2, 0, 1, 21, 5, 4, 3, 9, 8, 7, 6, 10, 19, 11, 12, 13, 14, 15, 16, 17, 18,
        ],
        (0..20)
            .filter(|i| i % 2 == 1)
            .chain((0..20).filter(|i| i % 2 == 0))
            .collect(),
        vec![
            22, 23, 19, 18, 17, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
        ], // 0 lost
    ];
    for order in orders {
        let mut dep = Depacketizer::new(Codec::Hevc);
        let out: Vec<AccessUnit> = order.iter().filter_map(|&i| dep.push(&pkts[i])).collect();
        assert_eq!(indices(&out), vec![73], "order {order:?}");
        assert_eq!(out[0].data, payload, "order {order:?}");
        assert!(out[0].is_keyframe);
        assert_eq!(out[0].host_latency_tenths_ms, 42);
    }
}

/// A data shard shorter than its block's others (malformed) is taken as-is,
/// exactly as when every frame was concatenated from separate shards: the
/// frame comes out with that shard's bytes, whichever shard arrives first.
#[test]
fn a_data_shard_of_the_wrong_length_is_taken_as_is() {
    let payload = nals(74, 20 * BS - 100);
    let mut pkts = frame(74, false, &payload, BS, 20, 1); // k = 20
    pkts[5].pop(); // shard 5 loses its last byte
    let mut expected = payload.clone();
    expected.remove(6 * BS - 1 - rtp::SHORT_FRAME_HEADER_LEN);
    for order in [
        (0..24).collect::<Vec<usize>>(), // short shard mid-stream
        [5].into_iter().chain(0..5).chain(6..24).collect(), // short shard first
        (0..5).chain(6..20).chain([5]).collect(), // short shard last
    ] {
        let mut dep = Depacketizer::new(Codec::Hevc);
        let out: Vec<AccessUnit> = order.iter().filter_map(|&i| dep.push(&pkts[i])).collect();
        assert_eq!(indices(&out), vec![74], "order {order:?}");
        assert_eq!(out[0].data, expected, "order {order:?}");
    }
}

/// One loss beyond the parity budget is unrecoverable: no frame, and certainly
/// no wrong frame.
#[test]
fn loss_beyond_the_parity_budget_yields_no_frame() {
    let pkts = frame(71, true, &nals(71, 20 * BS), BS, 20, 1); // k = 21, m = 5
    let k = 21;
    let m = pkts.len() - k;
    assert_eq!(m, 5);
    let drops: Vec<usize> = (0..=m).collect(); // m + 1 data shards
    let mut dep = Depacketizer::new(Codec::Hevc);
    assert!(feed(&mut dep, &without(&pkts, &drops)).is_empty());
}

/// The frame is delivered the instant enough shards arrive — it does not wait
/// for the remaining parity.
#[test]
fn a_frame_is_delivered_as_soon_as_k_shards_have_arrived() {
    let pkts = frame(72, false, &nals(72, 10 * BS - 8), BS, 50, 1); // k = 10, m = 5
    assert_eq!(pkts.len(), 15);
    let mut dep = Depacketizer::new(Codec::Hevc);
    for (i, p) in pkts.iter().enumerate() {
        let got = dep.push(p).is_some();
        assert_eq!(
            got,
            i == 9,
            "delivery must happen at packet 10 exactly (packet {})",
            i + 1
        );
    }
    assert_eq!(
        dep.stats().late_packets,
        5,
        "trailing parity is simply late"
    );
}

// ---------------------------------------------------------------------------
// Multi-block frames (frames larger than one 255-shard FEC block)
// ---------------------------------------------------------------------------

/// A large frame split across FEC blocks reassembles to the original bytes for
/// every block count the wire format allows.
#[test]
fn multi_block_frames_reassemble_exactly() {
    for blocks in 2..=4usize {
        // 300 data shards: more than one block can hold with 20 % parity.
        let payload = nals(80 + blocks as u32, 300 * BS - 123);
        let pkts = frame(80, true, &payload, BS, 20, blocks);
        let mut dep = Depacketizer::new(Codec::Hevc);
        let out = feed(&mut dep, &pkts);
        assert_eq!(indices(&out), vec![80], "{blocks} blocks");
        assert_eq!(out[0].data.len(), payload.len());
        assert!(out[0].data == payload, "{blocks} blocks: bytes differ");
        assert_eq!(out[0].meta.fec_blocks as usize, blocks);
        assert_eq!(out[0].meta.data_shards, 300);
    }
}

/// Each block recovers independently: losses up to every block's own parity
/// budget, in every block at once, still heal.
#[test]
fn multi_block_frames_heal_losses_in_every_block() {
    // 8-byte frame header + NALs fill exactly 400 shards (the last one short).
    let payload = nals(90, 400 * BS - 8 - 7);
    let pkts = frame(90, true, &payload, BS, 10, 4); // 4 x (100 data + 10 parity)
    assert_eq!(pkts.len(), 440);
    // Drop 10 data shards from each block (its whole budget), incl. the header.
    let drops: Vec<usize> = (0..4)
        .flat_map(|b| (0..10).map(move |i| b * 110 + i * 9))
        .collect();
    let mut dep = Depacketizer::new(Codec::Hevc);
    let out = feed(&mut dep, &without(&pkts, &drops));
    assert_eq!(indices(&out), vec![90]);
    assert!(out[0].data == payload);
    assert_eq!(out[0].meta.recovered_shards, 40);

    // One loss too many in a single block sinks the frame (other blocks fine).
    let mut too_many = drops.clone();
    too_many.push(110 + 99); // an 11th data shard of block 1
    let mut dep = Depacketizer::new(Codec::Hevc);
    assert!(feed(&mut dep, &without(&pkts, &too_many)).is_empty());
}

/// Blocks may arrive interleaved or in reverse; the frame is the same.
#[test]
fn multi_block_shards_may_arrive_in_any_order() {
    let payload = nals(95, 280 * BS);
    let mut pkts = frame(95, false, &payload, BS, 20, 2);
    pkts.reverse();
    let mut dep = Depacketizer::new(Codec::Hevc);
    let out = feed(&mut dep, &pkts);
    assert_eq!(indices(&out), vec![95]);
    assert!(out[0].data == payload);
}

// ---------------------------------------------------------------------------
// Robustness: nothing a peer sends may panic or wedge the reassembler
// ---------------------------------------------------------------------------

/// Stale, duplicate and inconsistent packets are counted and ignored.
#[test]
fn stale_duplicate_and_malformed_packets_are_counted_and_ignored() {
    let a = frame(100, true, &nals(100, 4 * BS), BS, 0, 1);
    let mut dep = Depacketizer::new(Codec::Hevc);
    assert!(dep.push(&a[0]).is_none());
    assert!(dep.push(&a[0]).is_none(), "duplicate shard");
    assert_eq!(dep.stats().duplicate_packets, 1);

    // Same frame, a free shard slot, but a different geometry: inconsistent.
    let other = frame(100, true, &nals(100, 9 * BS), BS, 0, 1);
    assert!(dep.push(&other[1]).is_none());
    assert_eq!(dep.stats().malformed_packets, 1);

    // Shard index beyond data + parity.
    let mut bad = a[1].clone();
    let mut h = rtp::parse_header(&bad).unwrap();
    h.shard_index = 900;
    assert!(h.write(&mut bad));
    assert!(dep.push(&bad).is_none());
    assert_eq!(dep.stats().malformed_packets, 2);

    // The frame still completes from its own packets.
    let out = feed(&mut dep, &a[1..]);
    assert_eq!(indices(&out), vec![100]);
    assert_eq!(out[0].data, nals(100, 4 * BS));

    // Truncated and empty datagrams are not picture packets.
    assert!(dep.push(&[]).is_none());
    assert!(dep.push(&a[0][..20]).is_none());
}

/// A burst that leaves several frames incomplete cannot grow the window without
/// bound: the oldest is given up, and a later complete frame is still delivered.
#[test]
fn the_window_is_bounded_and_recovers_after_a_burst() {
    let mut dep = Depacketizer::new(Codec::Hevc);
    for n in 200..220u32 {
        let f = frame(n, false, &nals(n, 3 * BS), BS, 0, 1);
        assert!(feed(&mut dep, &without(&f, &[1])).is_empty()); // every frame damaged
    }
    let good = frame(220, true, &nals(220, 3 * BS), BS, 0, 1);
    assert_eq!(indices(&feed(&mut dep, &good)), vec![220]);
    let s: ReassemblyStats = dep.stats();
    assert_eq!(s.frames_lost, 20, "all 20 damaged frames are accounted for");
    assert_eq!(s.frames_delivered, 1);
}

/// If the host restarts its frame counter the stream must resynchronise instead
/// of treating everything as "late" forever.
#[test]
fn a_restarted_frame_counter_resynchronises() {
    let mut dep = Depacketizer::new(Codec::Hevc);
    assert_eq!(
        indices(&feed(
            &mut dep,
            &frame(50_000, true, &nals(1, BS), BS, 0, 1)
        )),
        vec![50_000]
    );
    // A slightly older frame is just late ...
    assert!(feed(&mut dep, &frame(49_990, true, &nals(2, BS), BS, 0, 1)).is_empty());
    // ... a counter back near zero is a restart.
    assert_eq!(
        indices(&feed(&mut dep, &frame(1, true, &nals(3, BS), BS, 0, 1))),
        vec![1]
    );
    assert_eq!(
        indices(&feed(&mut dep, &frame(2, false, &nals(4, BS), BS, 0, 1))),
        vec![2]
    );
}

/// Frame indices wrap at 2^32; ordering must survive the wrap.
#[test]
fn frame_order_survives_the_index_wrap() {
    let mut dep = Depacketizer::new(Codec::Hevc);
    let mut out = feed(&mut dep, &frame(u32::MAX, true, &nals(1, BS), BS, 0, 1));
    out.extend(feed(&mut dep, &frame(0, false, &nals(2, BS), BS, 0, 1)));
    out.extend(feed(&mut dep, &frame(1, false, &nals(3, BS), BS, 0, 1)));
    assert_eq!(indices(&out), vec![u32::MAX, 0, 1]);
    assert_eq!(dep.stats().frames_lost, 0);
}

// ---------------------------------------------------------------------------
// Timeline metadata
// ---------------------------------------------------------------------------

/// The access unit carries when its first packet arrived and when it became
/// decodable — the first two points of the client latency timeline.
#[test]
fn access_unit_reports_first_packet_and_completion_times() {
    let pkts = frame(7, true, &nals(7, 5 * BS), BS, 0, 1);
    let t0 = Instant::now();
    let mut dep = Depacketizer::new(Codec::Hevc);
    let mut out = None;
    for (i, p) in pkts.iter().enumerate() {
        out = out.or(dep.push_at(p, t0 + Duration::from_micros(300 * i as u64)));
    }
    let au = out.expect("frame");
    assert_eq!(au.meta.first_packet_at, Some(t0));
    assert_eq!(
        au.meta.complete_at,
        Some(t0 + Duration::from_micros(300 * 5))
    );
    assert_eq!(au.meta.packets, 6);
    assert_eq!(au.meta.rtp_timestamp, 7 * 1500);
    assert_eq!(au.meta.fec_blocks, 1);
}

/// The whole captured Sunshine stream reassembles with nothing lost, skipped or
/// malformed, in strictly increasing frame order.
#[test]
fn the_captured_stream_reassembles_cleanly_with_the_keyframe_gate_on() {
    let pkts = load_fixture();
    let mut dep = Depacketizer::new(Codec::Hevc).gate_on_keyframes(true);
    let out = feed(&mut dep, &pkts);
    let s = dep.stats();
    assert!(
        out.len() > 100,
        "the fixture holds well over 100 frames, got {}",
        out.len()
    );
    assert!(out[0].is_keyframe);
    assert_eq!((s.frames_skipped, s.malformed_packets), (0, 0));
    assert!(
        out.windows(2).all(|w| w[1].frame_index > w[0].frame_index),
        "strictly increasing"
    );
    assert!(out.iter().all(|au| au.meta.recovered_shards == 0));
}
