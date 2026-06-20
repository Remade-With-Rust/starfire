// SPDX-License-Identifier: Apache-2.0
//! stream-bench — objective wire benchmark of a captured GameStream video stream.
//!
//! The video is plaintext HEVC on the wire for any client, so the SAME analysis
//! applies to a Starfire capture or a Moonlight capture — making back-to-back
//! runs directly comparable, with no reliance on either client's self-reporting.
//!
//! Computes FPS, bitrate, host encode latency (from the frame header), and
//! dropped frames using the stream's own 90 kHz RTP clock for duration.
//!
//! Usage: `stream-bench <capture.pcap> [label]`  (capture: `tcpdump -w` on the
//! client, filtered to `udp src port 47998`).

use std::collections::BTreeSet;
use std::env;

use fixture_slicer::{l2l4, pcap};

const VIDEO_PORT: u16 = 47998;
// Video packet field offsets (RTP(12) + reserved(4) + NV_VIDEO_PACKET(16)),
// matching starfire-core::video.
const RTP_TS: usize = 4; // BE u32, 90 kHz
const FRAME_INDEX: usize = 20; // LE u32
const FLAGS: usize = 24; // u8
const PAYLOAD: usize = 32; // coded payload start
const FLAG_SOF: u8 = 0x04;

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: stream-bench <capture.pcap> [label]");
        std::process::exit(2);
    });
    let label = env::args().nth(2).unwrap_or_else(|| "stream".into());
    let bytes = std::fs::read(&path).expect("read pcap");
    let file = pcap::parse(&bytes).expect("parse pcap");

    let (mut packets, mut video_bytes) = (0u64, 0u64);
    let mut first_ts: Option<u32> = None;
    let mut last_ts = 0u32;
    let (mut min_frame, mut max_frame) = (u32::MAX, 0u32);
    let mut frames = BTreeSet::new();
    let mut host_lat_tenths: Vec<u16> = Vec::new();

    for rec in &file.records {
        let Some(l4) = l2l4::parse(file.datalink, &rec.data) else {
            continue;
        };
        if l4.src_port != VIDEO_PORT {
            continue;
        }
        let p = &l4.payload;
        if p.len() < PAYLOAD {
            continue;
        }
        packets += 1;
        video_bytes += p.len() as u64;
        if let Some(ts) = be32(p, RTP_TS) {
            first_ts.get_or_insert(ts);
            last_ts = ts;
        }
        if let Some(fi) = le32(p, FRAME_INDEX) {
            min_frame = min_frame.min(fi);
            max_frame = max_frame.max(fi);
            frames.insert(fi);
        }
        if p[FLAGS] & FLAG_SOF != 0 {
            if let Some(b) = p.get(PAYLOAD + 1..PAYLOAD + 3) {
                host_lat_tenths.push(u16::from_le_bytes([b[0], b[1]]));
            }
        }
    }

    let dur = first_ts
        .map(|f| last_ts.wrapping_sub(f) as f64 / 90_000.0)
        .unwrap_or(0.0)
        .max(1e-9);
    let frame_count = frames.len();
    let fps = frame_count as f64 / dur;
    let mbps = video_bytes as f64 * 8.0 / dur / 1e6;
    let (host_avg, host_med) = if host_lat_tenths.is_empty() {
        (0.0, 0.0)
    } else {
        let avg =
            host_lat_tenths.iter().map(|&x| x as f64).sum::<f64>() / host_lat_tenths.len() as f64;
        let mut sorted = host_lat_tenths.clone();
        sorted.sort_unstable();
        let med = sorted[sorted.len() / 2] as f64;
        (avg / 10.0, med / 10.0)
    };
    let dropped = if max_frame >= min_frame {
        ((max_frame - min_frame + 1) as usize).saturating_sub(frame_count)
    } else {
        0
    };
    let drop_pct = 100.0 * dropped as f64 / (frame_count + dropped).max(1) as f64;

    println!("===== {label} (wire capture) =====");
    if packets == 0 {
        println!("NO VIDEO PACKETS found (datalink {} — check capture)", file.datalink);
        return;
    }
    println!("duration (RTP)  : {dur:.1} s");
    println!("frames          : {frame_count}   |   FPS: {fps:.1}");
    println!(
        "host latency    : median {host_med:.1} ms / avg {host_avg:.1} ms   (encoder, from frame header)"
    );
    println!("video bitrate   : {mbps:.1} Mbps");
    println!("video packets   : {packets}   ({:.0}/s)", packets as f64 / dur);
    println!("dropped frames  : {dropped}   ({drop_pct:.2}%)");
}
