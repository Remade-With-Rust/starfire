# Protocol 06 — Control Stream (ENet over UDP)

> Provenance: observation against Sunshine vX.Y. Clean-room. ENet channel setup,
> AES-GCM framing, and message type IDs are **[CAPTURE-LOCKED]**. Crate:
> `rusty_enet` (MIT).

## Goal

A reliable-UDP, **AES-GCM-encrypted** bidirectional control channel that carries
input, IDR requests, keepalive, stats, and host→client events. Brought up right
after RTSP `PLAY` (§05), keyed by the RI key/IV (§02/§04/§05).

## Transport

- **ENet** over UDP (conventionally port 47999; **[CAPTURE-LOCKED]** from SETUP).
- `rusty_enet` gives us reliable + sequenced channels over UDP. Match Sunshine's
  ENet config exactly: **[CAPTURE-LOCKED]** channel count, peer count, and any
  connect-data / handshake bytes.

## Encryption

- Control messages are **AES-GCM** encrypted with the session RI key + an IV/nonce
  construction. **[CAPTURE-LOCKED]**: the exact nonce derivation (e.g. sequence
  counter ‖ salt), AAD, and tag placement. Reproduce from a fixture where the key
  is known and assert ciphertext equality.

## Message catalog (logical)

Each message is a typed payload framed with a message type id. **[CAPTURE-LOCKED]**
ids + byte layouts:

| Direction | Message | Purpose |
|-----------|---------|---------|
| C→H | input event | keyboard/mouse/gamepad (§09 owns the payloads) |
| C→H | **IDR / keyframe request** | on unrecoverable video loss (§07) |
| C→H | ping / keepalive | liveness + RTT measurement |
| C→H | loss + RTT stats | feeds host ABR |
| C→H | adaptive-bitrate feedback | request bitrate change |
| C→H | termination | graceful quit |
| H→C | rumble / haptics | forwarded to gamepad backend |
| H→C | HDR mode change | renderer reconfig |
| H→C | host events | pause/resume/idr-ack etc. |

## Hot-path rules

- The control loop runs on its own task; **no `unwrap`/`panic`** on any inbound
  byte. A malformed/forged frame is dropped + logged, never fatal.
- IDR-request latency matters (it gates recovery); keep encode+send sub-frame.
- Keepalive cadence must match what the host expects or it tears down the session.
  **[CAPTURE-LOCKED]** interval.

## Tests

- **Fixture:** captured ENet handshake + a sample of each message type (plaintext
  structure + the key material to reproduce ciphertext).
- **Golden:** encode each message → assert == captured plaintext; encrypt with the
  fixture key → assert == captured ciphertext.
- **Loss/abuse:** fuzz inbound frames; assert no panic, correct drop behavior.
- **Live:** establish control, send keepalive + IDR request, observe host ack;
  dated note.

## Open / to-confirm

- [ ] ENet channel/peer config + connect data. **[CAPTURE-LOCKED]**
- [ ] AES-GCM nonce/AAD construction. **[CAPTURE-LOCKED]**
- [ ] Message type id table + payload layouts. **[CAPTURE-LOCKED]**
- [ ] Keepalive interval + timeout. **[CAPTURE-LOCKED]**
