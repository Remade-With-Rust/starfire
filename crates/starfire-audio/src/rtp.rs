// SPDX-License-Identifier: Apache-2.0
//! Audio RTP parsing — docs/protocol/08. Derived from the Sunshine *server*
//! audio sender (`audio_packet_t = { RTP_PACKET }`) + the captured wire, never
//! the Moonlight client. A 12-byte RTP header then the Opus payload;
//! `packetType` distinguishes data (97) from Reed-Solomon FEC (127).

/// RTP header length; the Opus payload is `pkt[RTP_HEADER_LEN..]`.
pub const RTP_HEADER_LEN: usize = 12;
/// `rtp.packetType` for an Opus data packet.
pub const PACKET_TYPE_DATA: u8 = 97;
/// `rtp.packetType` for an audio FEC (parity) packet.
pub const PACKET_TYPE_FEC: u8 = 127;

/// Parsed audio RTP header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    pub packet_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
}

impl AudioHeader {
    pub fn is_data(&self) -> bool {
        self.packet_type == PACKET_TYPE_DATA
    }
    pub fn is_fec(&self) -> bool {
        self.packet_type == PACKET_TYPE_FEC
    }
}

/// Parse the RTP header of an audio datagram. `None` if it's too short.
pub fn parse(pkt: &[u8]) -> Option<AudioHeader> {
    if pkt.len() < RTP_HEADER_LEN {
        return None;
    }
    Some(AudioHeader {
        packet_type: pkt[1],
        sequence: u16::from_be_bytes([pkt[2], pkt[3]]),
        timestamp: u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]),
    })
}

/// The Opus payload of an audio data packet (the bytes after the RTP header).
pub fn opus_payload(pkt: &[u8]) -> Option<&[u8]> {
    let h = parse(pkt)?;
    if !h.is_data() || pkt.len() <= RTP_HEADER_LEN {
        return None;
    }
    Some(&pkt[RTP_HEADER_LEN..])
}
