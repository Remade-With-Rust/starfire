// SPDX-License-Identifier: Apache-2.0
//! Minimal TCP stream reassembly. Captures arrive out of order and with
//! retransmissions; we order each direction's segments by sequence number and
//! concatenate into the plaintext byte stream (RTSP on 48010, HTTP on 47989).
//!
//! Scope: no sequence-number wrap handling (fixture-sized captures don't wrap a
//! u32), best-effort over gaps. Good enough to recover a transcript; the golden
//! tests downstream are what enforce exactness.

use std::collections::HashMap;

use crate::layers::Layer;

/// Identifies one TCP connection independent of direction: the client endpoint
/// plus the server port (the well-known Sunshine port).
type ConnKey = (Layer, String, u16, u16);

struct Segment {
    seq: u32,
    data: Vec<u8>,
}

#[derive(Default)]
struct Conn {
    c2s: Vec<Segment>,
    s2c: Vec<Segment>,
}

/// One reassembled connection's two directional byte streams.
pub struct ReassembledConn {
    pub layer: Layer,
    pub client_ip: String,
    pub client_port: u16,
    pub server_port: u16,
    pub c2s: Vec<u8>,
    pub s2c: Vec<u8>,
}

#[derive(Default)]
pub struct Reassembler {
    conns: HashMap<ConnKey, Conn>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one TCP segment. `to_server` orients it into the c2s vs s2c stream.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        layer: Layer,
        client_ip: String,
        client_port: u16,
        server_port: u16,
        to_server: bool,
        seq: u32,
        payload: &[u8],
    ) {
        if payload.is_empty() {
            return; // pure ACK / handshake — no stream bytes.
        }
        let conn = self
            .conns
            .entry((layer, client_ip, client_port, server_port))
            .or_default();
        let dir = if to_server {
            &mut conn.c2s
        } else {
            &mut conn.s2c
        };
        dir.push(Segment {
            seq,
            data: payload.to_vec(),
        });
    }

    /// Assemble all connections. Output is sorted for deterministic fixtures.
    pub fn finish(self) -> Vec<ReassembledConn> {
        let mut out: Vec<ReassembledConn> = self
            .conns
            .into_iter()
            .map(
                |((layer, client_ip, client_port, server_port), conn)| ReassembledConn {
                    layer,
                    client_ip,
                    client_port,
                    server_port,
                    c2s: assemble(conn.c2s),
                    s2c: assemble(conn.s2c),
                },
            )
            .collect();
        out.sort_by(|a, b| {
            (a.layer.name(), &a.client_ip, a.client_port).cmp(&(
                b.layer.name(),
                &b.client_ip,
                b.client_port,
            ))
        });
        out
    }
}

/// Order segments by sequence and concatenate, skipping retransmitted overlap.
fn assemble(mut segs: Vec<Segment>) -> Vec<u8> {
    if segs.is_empty() {
        return Vec::new();
    }
    segs.sort_by_key(|s| s.seq);
    let mut out = Vec::new();
    let mut next_seq = segs[0].seq;
    for seg in segs {
        let end = seg.seq.wrapping_add(seg.data.len() as u32);
        if end <= next_seq {
            continue; // fully-overlapping retransmit.
        }
        if seg.seq <= next_seq {
            // Partial overlap: append only the new tail.
            let offset = (next_seq - seg.seq) as usize;
            out.extend_from_slice(&seg.data[offset..]);
        } else {
            // Gap (lost capture): append what we have, best-effort.
            out.extend_from_slice(&seg.data);
        }
        next_seq = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_out_of_order_with_retransmit() {
        let mut r = Reassembler::new();
        let layer = Layer::Rtsp;
        let ip = "10.0.0.1".to_string();
        // seq 100:"HELLO" (5), then a duplicate of the same, then seq 105:"WORLD".
        r.push(layer, ip.clone(), 5000, 48010, true, 105, b"WORLD");
        r.push(layer, ip.clone(), 5000, 48010, true, 100, b"HELLO");
        r.push(layer, ip.clone(), 5000, 48010, true, 100, b"HELLO"); // retransmit
        let conns = r.finish();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].c2s, b"HELLOWORLD");
        assert!(conns[0].s2c.is_empty());
    }

    #[test]
    fn handles_partial_overlap() {
        let mut r = Reassembler::new();
        r.push(Layer::Rtsp, "1.1.1.1".into(), 1, 48010, true, 10, b"ABCDE");
        // Overlaps last 2 bytes of the previous segment, adds "FGH".
        r.push(Layer::Rtsp, "1.1.1.1".into(), 1, 48010, true, 13, b"DEFGH");
        let conns = r.finish();
        assert_eq!(conns[0].c2s, b"ABCDEFGH");
    }
}
