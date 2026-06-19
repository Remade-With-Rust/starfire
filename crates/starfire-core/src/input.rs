// SPDX-License-Identifier: Apache-2.0
//! Input encoding — docs/protocol/09-input.md.
//!
//! Encodes keyboard/mouse events into Sunshine's GameStream control-stream
//! packets. Sent over the ENet control channel (channel 0, reliable) as
//! plaintext when control encryption is off (our `encryptionEnabled:0` session).
//!
//! # Clean-room provenance
//! Wire layout: control type `0x0206` (IDX_INPUT_DATA) and the per-message
//! framing are from the **Sunshine server** (`stream.cpp`/`input.cpp`). The input
//! magic constants + struct field offsets are protocol facts read (constants
//! only, owner-approved) from `moonlight-common-c Input.h` — we can't derive them
//! from the wire here because we're the *sender* and Moonlight encrypts its
//! input. The encoder below is first-party. [SOURCE: Sunshine stream.cpp/input.cpp
//! + moonlight-common-c Input.h — constants only, owner-approved]

/// Control-stream message type for input data (`packetTypes[IDX_INPUT_DATA]`).
pub const CTRL_TYPE_INPUT: u16 = 0x0206;

// Input packet magics (NV_INPUT_HEADER.magic, little-endian on the wire). The
// GEN5 variants are what the host switches on.
const MAGIC_MOUSE_MOVE_REL: u32 = 0x07;
const MAGIC_MOUSE_MOVE_ABS: u32 = 0x05;
const MAGIC_MOUSE_BTN_DOWN: u32 = 0x08;
const MAGIC_MOUSE_BTN_UP: u32 = 0x09;
const MAGIC_SCROLL: u32 = 0x0A;
const MAGIC_HSCROLL: u32 = 0x5500_0001;
const MAGIC_KEY_DOWN: u32 = 0x03;
const MAGIC_KEY_UP: u32 = 0x04;

/// Mouse button identifiers (GameStream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Middle = 2,
    Right = 3,
    Side1 = 4, // "back"
    Side2 = 5, // "forward"
}

/// Frame an input packet: `[type:u16 LE][size:u32 BE][magic:u32 LE][body]`.
/// `size` is the NV_INPUT_HEADER size = `magic (4) + body` (excludes the size
/// field and the control type). The whole buffer is one ENet control payload.
fn frame(magic: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + 8 + body.len());
    v.extend_from_slice(&CTRL_TYPE_INPUT.to_le_bytes());
    v.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    v.extend_from_slice(&magic.to_le_bytes());
    v.extend_from_slice(body);
    v
}

/// Relative mouse motion (raw deltas) — the FPS path: no acceleration, no
/// screen-edge clamping; send one per OS motion event for lowest latency.
pub fn mouse_move_rel(dx: i16, dy: i16) -> Vec<u8> {
    let mut body = [0u8; 4];
    body[0..2].copy_from_slice(&dx.to_be_bytes());
    body[2..4].copy_from_slice(&dy.to_be_bytes());
    frame(MAGIC_MOUSE_MOVE_REL, &body)
}

/// Absolute mouse position within a `width`×`height` reference viewport.
pub fn mouse_move_abs(x: i16, y: i16, width: i16, height: i16) -> Vec<u8> {
    let mut body = Vec::with_capacity(10);
    body.extend_from_slice(&x.to_be_bytes());
    body.extend_from_slice(&y.to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes()); // unused
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    frame(MAGIC_MOUSE_MOVE_ABS, &body)
}

/// Mouse button press/release.
pub fn mouse_button(button: MouseButton, down: bool) -> Vec<u8> {
    let magic = if down { MAGIC_MOUSE_BTN_DOWN } else { MAGIC_MOUSE_BTN_UP };
    frame(magic, &[button as u8])
}

/// Vertical scroll (positive = up). `amount` is in 120ths of a wheel notch.
pub fn scroll_vertical(amount: i16) -> Vec<u8> {
    let mut body = Vec::with_capacity(6);
    body.extend_from_slice(&amount.to_be_bytes()); // scrollAmt1
    body.extend_from_slice(&amount.to_be_bytes()); // scrollAmt2
    body.extend_from_slice(&0i16.to_be_bytes()); // zero3
    frame(MAGIC_SCROLL, &body)
}

/// Horizontal scroll (positive = right).
pub fn scroll_horizontal(amount: i16) -> Vec<u8> {
    frame(MAGIC_HSCROLL, &amount.to_be_bytes())
}

/// Keyboard key down/up. `vk` is a Windows virtual-key code; `modifiers` is the
/// VK modifier bitmask (shift/ctrl/alt/meta). NV_KEYBOARD_PACKET order:
/// `flags(u8), keyCode(u16), modifiers(u8), zero2(u16)`.
pub fn key(vk: u16, modifiers: u8, down: bool) -> Vec<u8> {
    let magic = if down { MAGIC_KEY_DOWN } else { MAGIC_KEY_UP };
    let mut body = Vec::with_capacity(6);
    body.push(0u8); // flags
    body.extend_from_slice(&vk.to_le_bytes()); // keyCode (VK in low byte)
    body.push(modifiers);
    body.extend_from_slice(&0u16.to_le_bytes()); // zero2
    frame(magic, &body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rel_mouse_move_layout() {
        // type 0x0206 LE | size=8 BE | magic=0x07 LE | dx=+100 BE | dy=-50 BE
        let msg = mouse_move_rel(100, -50);
        assert_eq!(
            msg,
            vec![
                0x06, 0x02, // CTRL_TYPE_INPUT (LE)
                0x00, 0x00, 0x00, 0x08, // size = magic(4)+body(4) (BE)
                0x07, 0x00, 0x00, 0x00, // magic 0x07 (LE)
                0x00, 0x64, // dx = 100 (BE)
                0xff, 0xce, // dy = -50 (BE)
            ]
        );
    }

    #[test]
    fn mouse_button_layout() {
        let msg = mouse_button(MouseButton::Left, true);
        // size = magic(4)+button(1) = 5; magic 0x08 down
        assert_eq!(msg, vec![0x06, 0x02, 0, 0, 0, 5, 0x08, 0, 0, 0, 0x01]);
    }

    #[test]
    fn key_down_is_vk_in_low_byte() {
        // 'A' = VK 0x41, no modifiers.
        let msg = key(0x41, 0, true);
        assert_eq!(msg[0..2], [0x06, 0x02]); // input control type
        assert_eq!(msg[6..10], [0x03, 0, 0, 0]); // magic KEY_DOWN (LE)
        assert_eq!(msg[10], 0); // flags
        assert_eq!(msg[11..13], [0x41, 0x00]); // keyCode 0x41 (LE)
    }
}
