// SPDX-License-Identifier: Apache-2.0
//! Slice a Sunshine session pcap into per-layer fixtures.
//!
//! The capture harness from docs/03-bitexact-methodology.md + docs/06-testing.md:
//! demultiplex a `tcpdump` capture by the Sunshine ports, reassemble the TCP
//! transcripts (RTSP/HTTP), and collect the UDP media/control datagrams — so
//! each protocol layer gets a committed, verbatim fixture to golden-test against.

// Panicking is the failure signal in tests; the unwrap/expect lints are for
// production paths, which here return Results/Options throughout.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod l2l4;
pub mod layers;
pub mod pcap;
pub mod tcp;
pub mod testbuild;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use l2l4::Proto;
use layers::Layer;
use tcp::{ReassembledConn, Reassembler};

/// One UDP flow's datagram payloads, in capture order (RTP video/audio, ENet
/// control, mDNS). Boundaries are preserved — they matter for RTP/FEC framing.
pub struct UdpFlow {
    pub layer: Layer,
    pub client_ip: String,
    pub client_port: u16,
    pub server_port: u16,
    pub packets: Vec<Vec<u8>>,
}

/// Everything the slicer pulled from a capture.
pub struct Slices {
    pub tcp: Vec<ReassembledConn>,
    pub udp: Vec<UdpFlow>,
    /// Encrypted HTTPS (47984) packets are counted but not sliced — no keys.
    pub https_packets: usize,
    /// Per-layer packet counts (by `Layer::name`) for the summary view.
    pub counts: BTreeMap<&'static str, usize>,
}

/// Run the full demux/reassembly pipeline over raw pcap bytes.
pub fn slice(pcap_bytes: &[u8]) -> Result<Slices, pcap::PcapError> {
    let file = pcap::parse(pcap_bytes)?;
    let mut reasm = Reassembler::new();
    let mut udp: HashMap<(Layer, String, u16, u16), Vec<Vec<u8>>> = HashMap::new();
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut https_packets = 0usize;

    for rec in &file.records {
        let Some(p) = l2l4::parse(file.datalink, &rec.data) else {
            continue;
        };
        let Some(cls) = layers::classify(p.proto, p.src_port, p.dst_port) else {
            continue;
        };
        *counts.entry(cls.layer.name()).or_insert(0) += 1;

        if cls.layer == Layer::HttpsControl {
            https_packets += 1;
            continue; // encrypted; nothing to slice without session keys.
        }

        let to_server = p.dst_port == cls.server_port;
        let (client_ip, client_port) = if to_server {
            (p.src_ip, p.src_port)
        } else {
            (p.dst_ip, p.dst_port)
        };

        match p.proto {
            Proto::Tcp => reasm.push(
                cls.layer,
                client_ip,
                client_port,
                cls.server_port,
                to_server,
                p.seq,
                &p.payload,
            ),
            Proto::Udp => {
                if p.payload.is_empty() {
                    continue;
                }
                udp.entry((cls.layer, client_ip, client_port, cls.server_port))
                    .or_default()
                    .push(p.payload);
            }
        }
    }

    let mut udp_flows: Vec<UdpFlow> = udp
        .into_iter()
        .map(
            |((layer, client_ip, client_port, server_port), packets)| UdpFlow {
                layer,
                client_ip,
                client_port,
                server_port,
                packets,
            },
        )
        .collect();
    udp_flows.sort_by(|a, b| {
        (a.layer.name(), &a.client_ip, a.client_port).cmp(&(
            b.layer.name(),
            &b.client_ip,
            b.client_port,
        ))
    });

    Ok(Slices {
        tcp: reasm.finish(),
        udp: udp_flows,
        https_packets,
        counts,
    })
}

/// Encode UDP datagrams into the `.frames` fixture container: a u32-LE length
/// prefix per packet, preserving datagram boundaries. The decoder for this lives
/// alongside the depacketizer tests in starfire-core when video ingest lands.
pub fn encode_frames(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for pkt in packets {
        out.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
        out.extend_from_slice(pkt);
    }
    out
}

/// Stamp applied to every fixture's `.meta.toml`. Mirrors `starfire_testkit::Meta`
/// so the slicer's output loads back through the test harness unchanged.
pub struct MetaParams {
    pub sunshine_version: String,
    pub captured: String,
    pub notes: String,
}

/// Write all slices under `out` as `<layer>/<conn>.{bin,frames}` + `.meta.toml`.
/// Returns the number of files written. TCP transcripts become `-c2s.bin` /
/// `-s2c.bin`; UDP flows become `.frames` (see [`encode_frames`]).
pub fn write_fixtures(s: &Slices, out: &Path, meta: &MetaParams) -> std::io::Result<usize> {
    let mut written = 0;
    for c in &s.tcp {
        let dir = out.join(c.layer.name());
        let base = format!("{}-{}", sanitize(&c.client_ip), c.client_port);
        if !c.c2s.is_empty() {
            written += write_one(
                &dir,
                &format!("{base}-c2s"),
                "bin",
                &c.c2s,
                c.layer.name(),
                meta,
                "client->server transcript",
            )?;
        }
        if !c.s2c.is_empty() {
            written += write_one(
                &dir,
                &format!("{base}-s2c"),
                "bin",
                &c.s2c,
                c.layer.name(),
                meta,
                "server->client transcript",
            )?;
        }
    }
    for f in &s.udp {
        let dir = out.join(f.layer.name());
        let base = format!("{}-{}", sanitize(&f.client_ip), f.client_port);
        let body = encode_frames(&f.packets);
        let note = format!(
            "udp .frames container: u32-LE length-prefixed records; {} datagram(s)",
            f.packets.len()
        );
        written += write_one(&dir, &base, "frames", &body, f.layer.name(), meta, &note)?;
    }
    Ok(written)
}

fn write_one(
    dir: &Path,
    name: &str,
    ext: &str,
    body: &[u8],
    layer: &str,
    meta: &MetaParams,
    extra_note: &str,
) -> std::io::Result<usize> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(format!("{name}.{ext}")), body)?;
    let notes = if meta.notes.is_empty() {
        extra_note.to_string()
    } else {
        format!("{}; {}", meta.notes, extra_note)
    };
    std::fs::write(
        dir.join(format!("{name}.meta.toml")),
        meta_toml(layer, &meta.sunshine_version, &meta.captured, &notes),
    )?;
    Ok(2)
}

fn meta_toml(layer: &str, version: &str, captured: &str, notes: &str) -> String {
    let esc = |s: &str| s.replace('\\', "").replace('"', "'");
    format!(
        "sunshine_version = \"{}\"\ncaptured = \"{}\"\nlayer = \"{}\"\nnotes = \"{}\"\n",
        esc(version),
        esc(captured),
        esc(layer),
        esc(notes),
    )
}

/// Make an IP string safe as a filename component (IPv6 `:` -> `_`).
pub fn sanitize(ip: &str) -> String {
    ip.replace(':', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use testbuild::{eth, ipv4, pcap as build_pcap, tcp, udp};

    fn frame_udp(sp: u16, dp: u16, payload: &[u8]) -> Vec<u8> {
        eth(
            0x0800,
            &ipv4(
                17,
                [192, 168, 1, 50],
                [192, 168, 1, 10],
                &udp(sp, dp, payload),
            ),
        )
    }
    fn frame_tcp(sp: u16, dp: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
        eth(
            0x0800,
            &ipv4(
                6,
                [192, 168, 1, 50],
                [192, 168, 1, 10],
                &tcp(sp, dp, seq, payload),
            ),
        )
    }

    #[test]
    fn end_to_end_slice() {
        // An RTSP transcript split out of order, a video datagram, a control
        // datagram, and an (ignored) encrypted HTTPS packet.
        let frames = vec![
            frame_tcp(50001, 48010, 1007, b"WORLD"),
            frame_udp(50000, 47998, b"RTP-A"),
            frame_tcp(50001, 48010, 1000, b"HELLO__"), // 7 bytes -> seq ends 1007
            frame_udp(50000, 47998, b"RTP-B"),
            frame_udp(50002, 47999, b"CTRL"),
            frame_tcp(50003, 47984, 1, b"<encrypted>"),
        ];
        let bytes = build_pcap(1, &frames);
        let s = slice(&bytes).unwrap();

        // RTSP reassembled in order despite out-of-order capture.
        assert_eq!(s.tcp.len(), 1);
        assert_eq!(s.tcp[0].layer, Layer::Rtsp);
        assert_eq!(s.tcp[0].c2s, b"HELLO__WORLD");

        // Video datagrams kept with boundaries, in order.
        let video = s.udp.iter().find(|f| f.layer == Layer::Video).unwrap();
        assert_eq!(video.packets, vec![b"RTP-A".to_vec(), b"RTP-B".to_vec()]);

        // Control flow present.
        assert!(s.udp.iter().any(|f| f.layer == Layer::Control));

        // HTTPS counted but not sliced.
        assert_eq!(s.https_packets, 1);
        assert_eq!(s.counts.get("https-control"), Some(&1));
        assert_eq!(s.counts.get("video"), Some(&2));
    }

    #[test]
    fn frames_container_roundtrips_boundaries() {
        let packets = vec![b"aa".to_vec(), b"bbbb".to_vec()];
        let encoded = encode_frames(&packets);
        // u32(2) + "aa" + u32(4) + "bbbb"
        assert_eq!(
            encoded,
            vec![2, 0, 0, 0, b'a', b'a', 4, 0, 0, 0, b'b', b'b', b'b', b'b']
        );
    }
}
