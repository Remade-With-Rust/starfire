// SPDX-License-Identifier: Apache-2.0
//! Input encoding — docs/protocol/09-input.md.
//! Derived from protocol observation against Sunshine. Clean-room.
//!
//! Encodes keyboard/mouse/gamepad into Sunshine's packet formats, AES-GCM over
//! the control channel, with stream-resolution coordinate scaling and
//! anti-cheat-safe pacing. Packet layouts + scaling are [CAPTURE-LOCKED];
//! capture lives in starfire-input, encoding lives here.
//!
//! Phase 1 (F10) lands real mouse/keyboard packets against captured fixtures,
//! built on [`crate::wire::Wire`] + [`crate::wire::be_u16`].

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::wire::{assert_roundtrip, be_u16, Wire, WireError};

    /// Harness demonstration ONLY — proves the `Wire` + golden-test machinery
    /// works end-to-end. This is **not** a real Sunshine packet format; the real
    /// input layouts are [CAPTURE-LOCKED] and land with fixtures (docs/protocol/09).
    #[derive(Debug, PartialEq, Eq)]
    struct DemoPacket {
        button: u16,
        seq: u16,
    }

    impl Wire for DemoPacket {
        fn encode(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&self.button.to_be_bytes());
            out.extend_from_slice(&self.seq.to_be_bytes());
        }
        fn decode(input: &[u8]) -> Result<Self, WireError> {
            Ok(Self {
                button: be_u16(input, 0)?,
                seq: be_u16(input, 2)?,
            })
        }
    }

    #[test]
    fn wire_roundtrip_machinery_works() {
        let pkt = DemoPacket {
            button: 0x0102,
            seq: 0xBEEF,
        };
        // Mirrors a golden test: a known value <-> known bytes, both directions.
        assert_roundtrip(&pkt, &[0x01, 0x02, 0xBE, 0xEF]);
    }

    #[test]
    fn short_input_errors_not_panics() {
        assert!(DemoPacket::decode(&[0x01]).is_err());
    }
}
