// SPDX-License-Identifier: Apache-2.0
//! `fixture-slicer` — cut a Sunshine session pcap into per-layer fixtures.
//!
//! Usage:
//!   fixture-slicer <session.pcap> --list
//!   fixture-slicer <session.pcap> --out tests/fixtures --sunshine-version 0.23.1 \
//!       [--captured 2026-06-18] [--notes "host=gaming-vm, 4k120 hdr"]
//!
//! `--list` summarizes what a capture contains without writing anything — run it
//! first. Writing emits `<layer>/<conn>-c2s.bin` / `-s2c.bin` for TCP transcripts
//! and `<layer>/<conn>.frames` for UDP datagrams, each with a `.meta.toml`
//! sidecar (docs/03-bitexact-methodology.md). Std-only.

use std::path::PathBuf;
use std::process::ExitCode;

use fixture_slicer::{slice, write_fixtures, MetaParams, Slices};

struct Args {
    input: PathBuf,
    out: Option<PathBuf>,
    list: bool,
    sunshine_version: String,
    captured: String,
    notes: String,
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("error: {msg}\n");
            eprintln!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let bytes = match std::fs::read(&args.input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: reading {}: {e}", args.input.display());
            return ExitCode::FAILURE;
        }
    };

    let slices = match slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: parsing pcap: {e}");
            return ExitCode::FAILURE;
        }
    };

    print_summary(&slices);

    if args.list {
        return ExitCode::SUCCESS;
    }

    let Some(out) = args.out.as_ref() else {
        eprintln!("\nerror: pass --out <dir> to write fixtures (or --list to only summarize)");
        return ExitCode::FAILURE;
    };

    let meta = MetaParams {
        sunshine_version: args.sunshine_version.clone(),
        captured: args.captured.clone(),
        notes: args.notes.clone(),
    };
    match write_fixtures(&slices, out, &meta) {
        Ok(n) => {
            println!("\nwrote {n} fixture file(s) under {}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: writing fixtures: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_summary(s: &Slices) {
    println!("capture summary (packets per layer):");
    if s.counts.is_empty() {
        println!("  (no Sunshine-port traffic recognized)");
    }
    for (layer, count) in &s.counts {
        println!("  {layer:<14} {count}");
    }
    println!("\nreassembled:");
    for c in &s.tcp {
        println!(
            "  {:<14} {}:{} <-> :{}   c2s {} B, s2c {} B",
            c.layer.name(),
            c.client_ip,
            c.client_port,
            c.server_port,
            c.c2s.len(),
            c.s2c.len()
        );
    }
    for f in &s.udp {
        let total: usize = f.packets.iter().map(|p| p.len()).sum();
        println!(
            "  {:<14} {}:{} -> :{}   {} datagram(s), {} B",
            f.layer.name(),
            f.client_ip,
            f.client_port,
            f.server_port,
            f.packets.len(),
            total
        );
    }
    if s.https_packets > 0 {
        println!(
            "\nnote: {} encrypted HTTPS (47984) packet(s) seen but not sliced — \
             capture the plaintext HTTP control port (47989) for those layers.",
            s.https_packets
        );
    }
}

const USAGE: &str = "\
usage: fixture-slicer <session.pcap> [--list] [--out <dir>]
                      [--sunshine-version <v>] [--captured <YYYY-MM-DD>] [--notes <text>]

  --list                 summarize the capture, write nothing
  --out <dir>            write fixtures under this dir (e.g. tests/fixtures)
  --sunshine-version <v> stamped into every .meta.toml (default: UNKNOWN)
  --captured <date>      capture date for .meta.toml (default: UNKNOWN)
  --notes <text>         free-form note prepended to each fixture's meta";

fn parse_args() -> Result<Args, String> {
    let mut input: Option<PathBuf> = None;
    let mut out = None;
    let mut list = false;
    let mut sunshine_version = "UNKNOWN".to_string();
    let mut captured = "UNKNOWN".to_string();
    let mut notes = String::new();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--out" => out = Some(PathBuf::from(next(&mut it, "--out")?)),
            "--sunshine-version" => sunshine_version = next(&mut it, "--sunshine-version")?,
            "--captured" => captured = next(&mut it, "--captured")?,
            "--notes" => notes = next(&mut it, "--notes")?,
            "-h" | "--help" => return Err("help".to_string()),
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => {
                if input.replace(PathBuf::from(other)).is_some() {
                    return Err("more than one input file given".to_string());
                }
            }
        }
    }

    let input = input.ok_or("no input pcap given")?;
    Ok(Args {
        input,
        out,
        list,
        sunshine_version,
        captured,
        notes,
    })
}

fn next(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} needs a value"))
}
