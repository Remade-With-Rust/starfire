// SPDX-License-Identifier: Apache-2.0
//! Input capture behind the `InputBackend` trait — docs/04-platform-backends.md.
//!
//! Captures keyboard/mouse/gamepad from the OS (XInput + raw input on Windows,
//! IOKit-HID on macOS). **Encoding to the wire format stays in `starfire-core`**
//! (docs/protocol/09) so the protocol is platform-independent. Capture must
//! preserve natural device timing for anti-cheat-safe pacing. Lands Phase 1/2.

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("no input backend available for this platform yet")]
    NoBackend,
}

/// A captured input event, pre-encoding. Variants expand as devices land; the
/// wire encoding of each is core's job (docs/protocol/09), not this crate's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    KeyDown { scancode: u16, modifiers: u16 },
    KeyUp { scancode: u16, modifiers: u16 },
    MouseMoveRelative { dx: i16, dy: i16 },
    MouseButton { button: u8, pressed: bool },
}

/// The platform seam: pull captured events to hand to core's encoder.
pub trait InputBackend: Send {
    fn poll_event(&mut self) -> Result<Option<InputEvent>, InputError>;
}
