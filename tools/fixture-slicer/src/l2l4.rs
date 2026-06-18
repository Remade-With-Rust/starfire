// SPDX-License-Identifier: Apache-2.0
//! Link → network → transport extraction. Given a captured frame + the pcap
//! link type, pull out (proto, addrs, ports, seq, payload). Std-only.
//!
//! Coverage: Ethernet (+ one VLAN tag), Linux cooked (SLL), raw IP, BSD null;
//! IPv4 + IPv6 (base header only) → TCP / UDP. IP fragments and IPv6 extension
//! header chains are skipped (returns `None`) — fine for LAN GameStream capture,
//! and never panics on malformed input (uses checked slicing throughout).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

/// One extracted transport segment.
pub struct L4 {
    pub proto: Proto,
    pub src_ip: String,
    pub dst_ip: String,
    pub src_port: u16,
    pub dst_port: u16,
    /// TCP sequence number (0 for UDP).
    pub seq: u32,
    pub payload: Vec<u8>,
}

const DLT_NULL: u32 = 0;
const DLT_EN10MB: u32 = 1;
const DLT_RAW: u32 = 101;
const DLT_LINUX_SLL: u32 = 113;

pub fn parse(datalink: u32, frame: &[u8]) -> Option<L4> {
    let ip = match datalink {
        DLT_EN10MB => eth_payload(frame)?,
        DLT_LINUX_SLL => frame.get(16..)?,
        DLT_RAW => frame,
        DLT_NULL => frame.get(4..)?,
        _ => return None,
    };
    parse_ip(ip)
}

fn eth_payload(f: &[u8]) -> Option<&[u8]> {
    let mut ethertype = be16(f, 12)?;
    let mut off = 14;
    if ethertype == 0x8100 {
        // 802.1Q VLAN tag: 2 bytes TCI then the real ethertype.
        ethertype = be16(f, 16)?;
        off = 18;
    }
    match ethertype {
        0x0800 | 0x86DD => f.get(off..),
        _ => None,
    }
}

fn parse_ip(ip: &[u8]) -> Option<L4> {
    match ip.first()? >> 4 {
        4 => parse_ipv4(ip),
        6 => parse_ipv6(ip),
        _ => None,
    }
}

fn parse_ipv4(ip: &[u8]) -> Option<L4> {
    let ihl = (ip.first()? & 0x0f) as usize * 4;
    if ihl < 20 {
        return None;
    }
    let flags_frag = be16(ip, 6)?;
    let frag_offset = flags_frag & 0x1fff;
    let more_fragments = flags_frag & 0x2000 != 0;
    if frag_offset != 0 || more_fragments {
        return None; // reassembling IP fragments is out of scope.
    }
    let proto = *ip.get(9)?;
    let src = ipv4_str(ip.get(12..16)?);
    let dst = ipv4_str(ip.get(16..20)?);
    finish_l4(proto, src, dst, ip.get(ihl..)?)
}

fn parse_ipv6(ip: &[u8]) -> Option<L4> {
    let next_header = *ip.get(6)?;
    let src = ipv6_str(ip.get(8..24)?);
    let dst = ipv6_str(ip.get(24..40)?);
    // Only handle a payload that is directly TCP/UDP (no extension-header walk).
    finish_l4(next_header, src, dst, ip.get(40..)?)
}

fn finish_l4(proto: u8, src: String, dst: String, l4: &[u8]) -> Option<L4> {
    match proto {
        6 => {
            let data_offset = ((*l4.get(12)? >> 4) as usize) * 4;
            if data_offset < 20 {
                return None;
            }
            Some(L4 {
                proto: Proto::Tcp,
                src_ip: src,
                dst_ip: dst,
                src_port: be16(l4, 0)?,
                dst_port: be16(l4, 2)?,
                seq: be32(l4, 4)?,
                payload: l4.get(data_offset..)?.to_vec(),
            })
        }
        17 => Some(L4 {
            proto: Proto::Udp,
            src_ip: src,
            dst_ip: dst,
            src_port: be16(l4, 0)?,
            dst_port: be16(l4, 2)?,
            seq: 0,
            payload: l4.get(8..)?.to_vec(),
        }),
        _ => None,
    }
}

fn be16(b: &[u8], o: usize) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn ipv4_str(b: &[u8]) -> String {
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

fn ipv6_str(b: &[u8]) -> String {
    let groups: Vec<String> = (0..8)
        .map(|i| format!("{:x}", u16::from_be_bytes([b[i * 2], b[i * 2 + 1]])))
        .collect();
    groups.join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testbuild::{eth, ipv4, tcp, udp};

    #[test]
    fn extracts_udp_over_eth_ipv4() {
        let frame = eth(
            0x0800,
            &ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], &udp(1111, 47998, b"hi")),
        );
        let l4 = parse(DLT_EN10MB, &frame).unwrap();
        assert_eq!(l4.proto, Proto::Udp);
        assert_eq!(l4.src_port, 1111);
        assert_eq!(l4.dst_port, 47998);
        assert_eq!(l4.dst_ip, "10.0.0.2");
        assert_eq!(l4.payload, b"hi");
    }

    #[test]
    fn extracts_tcp_seq_and_payload() {
        let frame = eth(
            0x0800,
            &ipv4(
                6,
                [10, 0, 0, 1],
                [10, 0, 0, 2],
                &tcp(2222, 48010, 500, b"DESCRIBE"),
            ),
        );
        let l4 = parse(DLT_EN10MB, &frame).unwrap();
        assert_eq!(l4.proto, Proto::Tcp);
        assert_eq!(l4.seq, 500);
        assert_eq!(l4.payload, b"DESCRIBE");
    }

    #[test]
    fn skips_fragmented_ipv4() {
        let mut frame = eth(
            0x0800,
            &ipv4(17, [10, 0, 0, 1], [10, 0, 0, 2], &udp(1, 2, b"x")),
        );
        // Set the "more fragments" bit in the IP header (offset 6 of the IP part,
        // which is 14 into the Ethernet frame).
        frame[14 + 6] |= 0x20;
        assert!(parse(DLT_EN10MB, &frame).is_none());
    }
}
