// SPDX-License-Identifier: Apache-2.0
//! Synthetic packet/pcap builders. Public so both in-crate unit tests and the
//! integration tests can construct hermetic captures — no real `.pcap` needed to
//! test the slicer. (This is an internal, `publish = false` tool crate.)

/// Wrap an L3 payload in an Ethernet II frame with the given ethertype.
pub fn eth(ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 12]; // dst + src MAC (zeros)
    v.extend_from_slice(&ethertype.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Build an IPv4 packet (checksum left zero — the parser ignores it).
pub fn ipv4(proto: u8, src: [u8; 4], dst: [u8; 4], l4: &[u8]) -> Vec<u8> {
    let total = 20 + l4.len();
    let mut v = Vec::new();
    v.push(0x45); // version 4, IHL 5
    v.push(0x00); // DSCP/ECN
    v.extend_from_slice(&(total as u16).to_be_bytes());
    v.extend_from_slice(&[0, 0]); // identification
    v.extend_from_slice(&[0, 0]); // flags + fragment offset
    v.push(64); // TTL
    v.push(proto);
    v.extend_from_slice(&[0, 0]); // header checksum
    v.extend_from_slice(&src);
    v.extend_from_slice(&dst);
    v.extend_from_slice(l4);
    v
}

/// Build a UDP datagram.
pub fn udp(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut v = Vec::new();
    v.extend_from_slice(&src_port.to_be_bytes());
    v.extend_from_slice(&dst_port.to_be_bytes());
    v.extend_from_slice(&(len as u16).to_be_bytes());
    v.extend_from_slice(&[0, 0]); // checksum
    v.extend_from_slice(payload);
    v
}

/// Build a TCP segment with a 20-byte header (no options).
pub fn tcp(src_port: u16, dst_port: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&src_port.to_be_bytes());
    v.extend_from_slice(&dst_port.to_be_bytes());
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]); // ack
    v.push(0x50); // data offset = 5 words (20 bytes)
    v.push(0x18); // flags PSH+ACK
    v.extend_from_slice(&[0, 0]); // window
    v.extend_from_slice(&[0, 0]); // checksum
    v.extend_from_slice(&[0, 0]); // urgent ptr
    v.extend_from_slice(payload);
    v
}

/// Assemble a little-endian classic pcap from a list of link-layer frames.
pub fn pcap(datalink: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&0i32.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&65535u32.to_le_bytes());
    v.extend_from_slice(&datalink.to_le_bytes());
    for f in frames {
        v.extend_from_slice(&0u32.to_le_bytes()); // ts_sec
        v.extend_from_slice(&0u32.to_le_bytes()); // ts_usec
        v.extend_from_slice(&(f.len() as u32).to_le_bytes()); // incl_len
        v.extend_from_slice(&(f.len() as u32).to_le_bytes()); // orig_len
        v.extend_from_slice(f);
    }
    v
}
