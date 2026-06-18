// SPDX-License-Identifier: Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used)] // panics are the test signal
//! End-to-end: build a synthetic Sunshine capture, slice it, write fixtures, and
//! load them back through `starfire-testkit` — proving the slicer's output is
//! exactly what the golden-test harness consumes (no schema drift between the
//! two tools). Uses a unique temp dir; no external test deps.

use std::path::PathBuf;

use fixture_slicer::testbuild::{eth, ipv4, pcap, tcp, udp};
use fixture_slicer::{encode_frames, slice, write_fixtures, MetaParams};
use starfire_testkit::Fixture;

fn unique_tmp(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("starfire-slicer-test-{}-{tag}", std::process::id()));
    dir
}

#[test]
fn slices_then_loads_through_testkit() {
    // RTSP transcript (two ordered segments) + two video datagrams.
    let src = [192, 168, 1, 50];
    let dst = [192, 168, 1, 10];
    let frames = vec![
        eth(
            0x0800,
            &ipv4(6, src, dst, &tcp(40001, 48010, 1, b"OPTIONS rtsp")),
        ),
        eth(
            0x0800,
            &ipv4(6, src, dst, &tcp(40001, 48010, 13, b" * RTSP/1.0")),
        ),
        eth(0x0800, &ipv4(17, src, dst, &udp(40000, 47998, b"RTP-1"))),
        eth(0x0800, &ipv4(17, src, dst, &udp(40000, 47998, b"RTP-2"))),
    ];
    let bytes = pcap(1, &frames);
    let slices = slice(&bytes).expect("slice ok");

    let out = unique_tmp("load");
    let _ = std::fs::remove_dir_all(&out);
    let meta = MetaParams {
        sunshine_version: "0.23.1".into(),
        captured: "2026-06-18".into(),
        notes: "synthetic test capture".into(),
    };
    let n = write_fixtures(&slices, &out, &meta).expect("write ok");
    assert_eq!(n, 4, "rtsp c2s .bin + meta, video .frames + meta");

    // The RTSP transcript fixture loads with the right bytes + meta.
    let rtsp = Fixture::load(out.join("rtsp").join("192.168.1.50-40001-c2s.bin"))
        .expect("rtsp fixture loads via testkit");
    assert_eq!(rtsp.bytes, b"OPTIONS rtsp * RTSP/1.0");
    assert_eq!(rtsp.meta.sunshine_version, "0.23.1");
    assert_eq!(rtsp.meta.captured, "2026-06-18");
    assert_eq!(rtsp.meta.layer, "rtsp");

    // The video .frames fixture preserves datagram boundaries.
    let video = Fixture::load(out.join("video").join("192.168.1.50-40000.frames"))
        .expect("video fixture loads via testkit");
    assert_eq!(
        video.bytes,
        encode_frames(&[b"RTP-1".to_vec(), b"RTP-2".to_vec()])
    );
    assert_eq!(video.meta.layer, "video");

    let _ = std::fs::remove_dir_all(&out);
}
