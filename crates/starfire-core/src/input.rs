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
///
/// Every body is at most 10 bytes, so the message is assembled in a stack
/// buffer and copied to the heap once (one allocation, one copy) instead of
/// four capacity-checked appends.
fn frame(magic: u32, body: &[u8]) -> Vec<u8> {
    const HEAD: usize = 10;
    const MAX_BODY: usize = 16;
    let mut head = [0u8; HEAD];
    head[0..2].copy_from_slice(&CTRL_TYPE_INPUT.to_le_bytes());
    head[2..6].copy_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    head[6..10].copy_from_slice(&magic.to_le_bytes());
    if body.len() > MAX_BODY {
        // No message has such a body today; never truncate one if it appears.
        return [&head[..], body].concat();
    }
    let mut m = [0u8; HEAD + MAX_BODY];
    m[..HEAD].copy_from_slice(&head);
    m[HEAD..HEAD + body.len()].copy_from_slice(body);
    m[..HEAD + body.len()].to_vec()
}

/// Relative mouse motion (raw deltas) — the FPS path: no acceleration, no
/// screen-edge clamping; send one per OS motion event for lowest latency.
pub fn mouse_move_rel(dx: i16, dy: i16) -> Vec<u8> {
    // The most frequent message, so it is built whole from its constant
    // header rather than through the generic framer.
    let mut m = [0u8; MOUSE_REL_LEN];
    m[..10].copy_from_slice(&MOUSE_REL_HEADER);
    m[10..12].copy_from_slice(&dx.to_be_bytes());
    m[12..14].copy_from_slice(&dy.to_be_bytes());
    m.to_vec()
}

/// Length of a relative-mouse-move message: type(2) + size(4) + magic(4) + dx(2) + dy(2).
const MOUSE_REL_LEN: usize = 14;

/// The `(dx, dy)` of an encoded relative mouse move, or `None` if `msg` is any
/// other message.
pub fn decode_mouse_rel(msg: &[u8]) -> Option<(i16, i16)> {
    // One comparison against the constant header instead of three field checks.
    let m: &[u8; MOUSE_REL_LEN] = msg.try_into().ok()?;
    if m[..10] != MOUSE_REL_HEADER {
        return None;
    }
    Some((i16::from_be_bytes([m[10], m[11]]), i16::from_be_bytes([m[12], m[13]])))
}

/// The fixed first 10 bytes of every relative-mouse-move message: type (LE),
/// size = 8 (BE), magic (LE).
const MOUSE_REL_HEADER: [u8; 10] = {
    let t = CTRL_TYPE_INPUT.to_le_bytes();
    let n = 8u32.to_be_bytes();
    let m = MAGIC_MOUSE_MOVE_REL.to_le_bytes();
    [t[0], t[1], n[0], n[1], n[2], n[3], m[0], m[1], m[2], m[3]]
};

/// Fold relative mouse move `next` into `prev` (summing the deltas) when both
/// are relative moves and the sum still fits. Returns `true` if `prev` now
/// carries both; `false` leaves `prev` untouched and the caller sends `next`
/// separately. Used only when moves have **queued up** behind a stalled link:
/// one message with the total motion replaces a backlog the host would have to
/// replay one stale step at a time.
pub fn merge_mouse_rel(prev: &mut [u8], next: &[u8]) -> bool {
    let (Some((ax, ay)), Some((bx, by))) = (decode_mouse_rel(prev), decode_mouse_rel(next)) else {
        return false;
    };
    let (Some(dx), Some(dy)) = (ax.checked_add(bx), ay.checked_add(by)) else {
        return false;
    };
    prev[10..12].copy_from_slice(&dx.to_be_bytes());
    prev[12..14].copy_from_slice(&dy.to_be_bytes());
    true
}

/// Absolute mouse position within a `width`×`height` reference viewport.
pub fn mouse_move_abs(x: i16, y: i16, width: i16, height: i16) -> Vec<u8> {
    let mut body = [0u8; 10];
    body[0..2].copy_from_slice(&x.to_be_bytes());
    body[2..4].copy_from_slice(&y.to_be_bytes());
    // body[4..6] unused (zero)
    body[6..8].copy_from_slice(&width.to_be_bytes());
    body[8..10].copy_from_slice(&height.to_be_bytes());
    frame(MAGIC_MOUSE_MOVE_ABS, &body)
}

/// Mouse button press/release.
pub fn mouse_button(button: MouseButton, down: bool) -> Vec<u8> {
    let magic = if down { MAGIC_MOUSE_BTN_DOWN } else { MAGIC_MOUSE_BTN_UP };
    frame(magic, &[button as u8])
}

/// Vertical scroll (positive = up). `amount` is in 120ths of a wheel notch.
pub fn scroll_vertical(amount: i16) -> Vec<u8> {
    let mut body = [0u8; 6];
    body[0..2].copy_from_slice(&amount.to_be_bytes()); // scrollAmt1
    body[2..4].copy_from_slice(&amount.to_be_bytes()); // scrollAmt2
                                                       // body[4..6] zero3
    frame(MAGIC_SCROLL, &body)
}

/// Horizontal scroll (positive = right).
pub fn scroll_horizontal(amount: i16) -> Vec<u8> {
    frame(MAGIC_HSCROLL, &amount.to_be_bytes())
}

/// Keyboard key down/up. `vk` is a Windows virtual-key code; `modifiers` is the
/// VK modifier bitmask (shift/ctrl/alt/meta).
///
/// `NV_KEYBOARD_PACKET` is `{ char flags; short keyCode; char modifiers; short
/// zero2; }`, and `Input.h` wraps everything in `#pragma pack(push, 1)` — so the
/// fields are **packed** (no alignment padding). `keyCode` is little-endian (the
/// host reads it un-swapped, VK in the low byte). [SOURCE: Input.h pack(1)]
pub fn key(vk: u16, modifiers: u8, down: bool) -> Vec<u8> {
    let magic = if down { MAGIC_KEY_DOWN } else { MAGIC_KEY_UP };
    let mut body = [0u8; 6];
    // body[0] flags (zero)
    body[1..3].copy_from_slice(&vk.to_le_bytes()); // keyCode (LE)
    body[3] = modifiers; // modifiers
                         // body[4..6] zero2
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
    fn backlogged_relative_moves_fold_into_one() {
        let mut a = mouse_move_rel(100, -50);
        assert!(merge_mouse_rel(&mut a, &mouse_move_rel(-30, 20)));
        assert_eq!(
            a,
            mouse_move_rel(70, -30),
            "the merged message is a plain rel-move"
        );
        assert_eq!(decode_mouse_rel(&a), Some((70, -30)));
    }

    #[test]
    fn merge_refuses_anything_that_is_not_two_relative_moves() {
        // A click between two moves must stay between them: order is behaviour.
        let mut mv = mouse_move_rel(5, 5);
        let before = mv.clone();
        assert!(!merge_mouse_rel(
            &mut mv,
            &mouse_button(MouseButton::Left, true)
        ));
        assert!(!merge_mouse_rel(&mut mv, &key(0x41, 0, true)));
        assert!(!merge_mouse_rel(&mut mv, &mouse_move_abs(1, 2, 1920, 1080)));
        assert!(!merge_mouse_rel(&mut mv, &scroll_vertical(120)));
        assert_eq!(mv, before, "prev is untouched when the merge is refused");
        let mut click = mouse_button(MouseButton::Left, true);
        assert!(!merge_mouse_rel(&mut click, &mouse_move_rel(1, 1)));
    }

    #[test]
    fn merge_refuses_a_sum_that_would_overflow() {
        let mut a = mouse_move_rel(i16::MAX - 1, 0);
        let before = a.clone();
        assert!(
            !merge_mouse_rel(&mut a, &mouse_move_rel(2, 0)),
            "would wrap"
        );
        assert_eq!(a, before);
        let mut b = mouse_move_rel(0, i16::MIN + 1);
        assert!(!merge_mouse_rel(&mut b, &mouse_move_rel(0, -2)));
        // Exactly at the limit is fine.
        let mut c = mouse_move_rel(i16::MAX - 1, 0);
        assert!(merge_mouse_rel(&mut c, &mouse_move_rel(1, 0)));
        assert_eq!(decode_mouse_rel(&c), Some((i16::MAX, 0)));
    }

    #[test]
    fn mouse_button_layout() {
        let msg = mouse_button(MouseButton::Left, true);
        // size = magic(4)+button(1) = 5; magic 0x08 down
        assert_eq!(msg, vec![0x06, 0x02, 0, 0, 0, 5, 0x08, 0, 0, 0, 0x01]);
    }

    #[test]
    fn key_down_packed_layout() {
        // 'A' = VK 0x41. Packed (pragma pack 1): flags|keyCode(LE)|mods|zero2.
        let msg = key(0x41, 0, true);
        assert_eq!(
            msg,
            vec![
                0x06, 0x02, // input control type (LE)
                0x00, 0x00, 0x00, 0x0A, // size = magic(4)+body(6) (BE)
                0x03, 0x00, 0x00, 0x00, // magic KEY_DOWN (LE)
                0x00, // flags
                0x41, 0x00, // keyCode 0x41 (LE)
                0x00, // modifiers
                0x00, 0x00, // zero2
            ]
        );
    }
}
