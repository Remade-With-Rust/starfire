// SPDX-License-Identifier: Apache-2.0
//! Decoder latency probe: feed the captured Sunshine HEVC stream through this
//! platform's hardware decoder at a live cadence and report **how long each
//! frame takes to come back to the caller** — the number the pipeline actually
//! experiences, which is not the same as how long `push()` takes to return.
//!
//! ```text
//! cargo run --release -p starfire-decode --example decode_fixture -- [--fps 60] [--async] [fixture]
//! ```
//!
//! Method: frames go in at a fixed cadence (`--fps`, default 60), exactly as
//! they arrive from the network. Decoders emit in order, so output `j` is input
//! `j`; a frame's latency is the time from the `push` that submitted it to the
//! `push` (or `flush`) that returned it. The count of frames still inside the
//! decoder after each `push` is reported too — a deterministic check that needs
//! no clock: a zero-latency decoder holds 0, a one-frame-late decoder holds 1.

use std::time::{Duration, Instant};

use starfire_core::metrics::LatencySeries;
use starfire_core::video::reassembly::Depacketizer;
use starfire_core::video::{AccessUnit, Codec};
use starfire_decode::select::{create_decoder, Accel};

fn load_access_units(path: &str) -> Result<Vec<AccessUnit>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let mut dep = Depacketizer::new(Codec::Hevc);
    let mut aus = Vec::new();
    let mut i = 0;
    while i + 2 <= raw.len() {
        let n = u16::from_le_bytes([raw[i], raw[i + 1]]) as usize;
        i += 2;
        if i + n > raw.len() {
            break;
        }
        if let Some(au) = dep.push(&raw[i..i + n]) {
            aus.push(au);
        }
        i += n;
    }
    Ok(aus)
}

fn main() {
    let mut fps = 60.0f64;
    let mut path = format!(
        "{}/../../tests/fixtures/video/stream-hevc.fix",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fps" => fps = args.next().and_then(|v| v.parse().ok()).unwrap_or(fps),
            "--async" => std::env::set_var("STARFIRE_DECODE_ASYNC", "1"),
            other => path = other.to_string(),
        }
    }

    let aus = match load_access_units(&path) {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => {
            eprintln!("no frames in {path}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let mut decoder = match create_decoder(Codec::Hevc, Accel::PreferHardware) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("no decoder: {e}");
            std::process::exit(1);
        }
    };

    let interval = Duration::from_secs_f64(1.0 / fps.max(1.0));
    let mut submitted: Vec<Instant> = Vec::with_capacity(aus.len());
    let mut returned = 0usize;
    let mut push_call = LatencySeries::new();
    let mut in_hand = LatencySeries::new();
    let mut held = [0u64; 8]; // frames inside the decoder after each push
    let mut pts_lag = [0u64; 8]; // submitted frame index - returned frame's pts
    let mut pts_unknown = 0u64;
    let (mut errors, mut dims) = (0u64, (0u32, 0u32));

    let start = Instant::now();
    for (n, au) in aus.iter().enumerate() {
        // Live cadence: frame n is submitted at start + n * interval.
        let due = start + interval * n as u32;
        while Instant::now() < due {
            std::thread::sleep(Duration::from_micros(200));
        }
        let t0 = Instant::now();
        submitted.push(t0);
        let out = decoder.push(au);
        let t1 = Instant::now();
        push_call.record_span(t0, t1);
        match out {
            Ok(Some(frame)) => {
                dims = (frame.width, frame.height);
                // Backends stamp each output with the frame index it was decoded
                // from, so the lag can be read directly rather than inferred.
                match (au.frame_index as i64).checked_sub(frame.pts) {
                    Some(lag) if (0..pts_lag.len() as i64).contains(&lag) => {
                        pts_lag[lag as usize] += 1
                    }
                    _ => pts_unknown += 1,
                }
                if let Some(&sub) = submitted.get(returned) {
                    in_hand.record_span(sub, t1);
                }
                returned += 1;
            }
            Ok(None) => {}
            Err(e) => {
                errors += 1;
                if errors <= 3 {
                    eprintln!("decode error at frame {n}: {e}");
                }
            }
        }
        let inside = (n + 1).saturating_sub(returned);
        held[inside.min(held.len() - 1)] += 1;
    }
    let at_flush = match decoder.flush() {
        Ok(v) => v.len(),
        Err(e) => {
            eprintln!("flush: {e}");
            0
        }
    };

    println!("== decoder latency probe ==");
    println!(
        "platform {} | {}x{} HEVC | {} frames in at {fps} fps | {} out during stream, {} at flush, {} errors",
        std::env::consts::OS,
        dims.0,
        dims.1,
        aus.len(),
        returned,
        at_flush,
        errors
    );
    println!("push() call time        : {}", push_call.summary());
    println!("submit -> frame in hand : {}", in_hand.summary());
    print!("frames held after push  :");
    for (k, &c) in held.iter().enumerate() {
        if c > 0 {
            print!("  {k} held x{c}");
        }
    }
    println!();
    print!("lag by timestamp (frames):");
    for (k, &c) in pts_lag.iter().enumerate() {
        if c > 0 {
            print!("  {k} x{c}");
        }
    }
    if pts_unknown > 0 {
        print!("  unreadable x{pts_unknown}");
    }
    println!();
    let lost = aus.len() as i64 - returned as i64 - at_flush as i64 - errors as i64;
    if lost != 0 {
        println!("NOTE: {lost} frame(s) unaccounted for (dropped inside the decoder) — the in-order pairing above is then approximate");
    }
}
