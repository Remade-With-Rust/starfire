# Protocol 09 — Input

> Provenance: observation against Sunshine vX.Y. Clean-room. Packet structs,
> scaling math, and AES-GCM framing are **[CAPTURE-LOCKED]**. Input rides the
> control channel (§06).

## Goal

Capture local keyboard/mouse/gamepad, encode into Sunshine's input packet formats,
AES-GCM encrypt, and send over the control channel with correct coordinate scaling
— and **pacing that does not look synthetic** (anti-cheat-safe), which is a
first-class requirement, not an afterthought.

## Devices & payloads — [CAPTURE-LOCKED]

| Device | Payload essentials |
|--------|--------------------|
| **Keyboard** | scancodes + modifier bitmask; key down/up events |
| **Mouse — motion** | absolute *and* relative motion variants |
| **Mouse — buttons** | button id + down/up |
| **Mouse — scroll** | vertical + horizontal, **high-resolution** scroll |
| **Gamepad** | controller index, button bitmask, analog sticks, analog triggers |
| **Gamepad — rumble** | host→client (received on control, §06) |
| **Gamepad — DS4/DS5 basics** | battery / touchpad / gyro where feasible |

Each has a distinct packet **type id + struct**. Freeze each from capture and
golden-test the encoder against it.

## Coordinate scaling

- Absolute mouse coordinates are scaled to the **stream resolution**, not the
  client window. Get the math exactly right or the cursor drifts. **[CAPTURE-LOCKED]**:
  the coordinate space (e.g. normalized fixed-point vs absolute pixels) and
  rounding.

## Capture backends (trait `InputBackend`)

- **Windows:** XInput (gamepad) + raw input (kbd/mouse).
- **macOS:** IOKit-HID.
- Capture is platform (`starfire-input`); **encoding is protocol** (`starfire-core`).
  Multiple controllers supported via the gamepad mask (§04).

## Encryption

- Input packets are **AES-GCM** encrypted with the session key, framed exactly as
  the control channel expects (§06). **[CAPTURE-LOCKED]** nonce/AAD.

## Anti-cheat-safe pacing — first-class requirement

- Input timing must **not look synthetic**: no perfectly-uniform inter-event
  intervals, no batching that produces robotic deltas. Preserve the real device's
  natural timing as closely as the transport allows; add no artificial regularity.
- Treat this as an acceptance criterion ([`../07-performance-budgets.md`](../07-performance-budgets.md)):
  sub-frame encode+send latency **and** a timing profile indistinguishable from a
  local device. Audit it explicitly in Phase 3.

## Hot-path rules

- Sub-frame encode+send; no `unwrap`/`panic`; never block the capture thread on
  the network.

## Tests

- **Fixture:** captured input packets for each device/event type (plaintext +
  key material to reproduce ciphertext).
- **Golden:** encode each event → assert == captured plaintext; encrypt → assert
  == captured ciphertext; scaling round-trips exactly.
- **Pacing:** statistical test that emitted timing matches a captured human-device
  distribution, not a uniform one.
- **Live:** drive the real desktop (mouse+keyboard, then gamepad); dated note.

## Open / to-confirm

- [ ] Per-device packet type ids + struct layouts. **[CAPTURE-LOCKED]**
- [ ] Absolute coordinate space + scaling/rounding. **[CAPTURE-LOCKED]**
- [ ] High-res scroll units. **[CAPTURE-LOCKED]**
- [ ] AES-GCM nonce/AAD for input. **[CAPTURE-LOCKED]**
- [ ] DS4/DS5 extended-feature feasibility. **[CAPTURE-LOCKED]**
