# Protocol 06 — Control Stream (ENet over UDP)

> Provenance: observation against **Sunshine 2026.516.143833**. Clean-room. Crate:
> `rusty_enet` (MIT). AES-GCM framing + message type IDs are **[CAPTURE-LOCKED]**.

## 🟡 Status — transport implemented; data-plane validation blocked
`starfire_core::control::ControlChannel` is the ENet transport (connect with the
RTSP `X-SS-Connect-Data` token, poll/send, AES-GCM to follow). It compiles and is
structurally complete, but **is not yet live-validated** — see the blocker below.

### ⛔ Blocker — same-machine data plane (environment, not protocol)
Sunshine will **not bring up the streaming data plane** (encoder + media/control
UDP ports) for a **same-machine client**. Observed:
- Control plane (discover→pair→serverinfo→applist→launch→RTSP, F1–F5) works fully
  over loopback.
- After `PLAY`, Sunshine logs `Executing [Desktop]` but, for a loopback client,
  warns `Unable to find MAC address for 127.0.0.1` and never creates the encoder
  or binds UDP 47998–48000. ENet connect to 47999 → ICMP port-unreachable (10054).
- Switching the client to the host's **LAN IP** + pinging from the **advertised
  client port (X-GS-ClientPort=50000)** advanced Sunshine into stream init
  (color range, bitrate, encoder enumeration) — but the encoder still never
  fully created and the control port still didn't bind.
- The data-plane streams appear **coupled**: the control port seems to bind only
  once the video RTP path is actively received, so F6/F7 likely have to come up
  together.

**Resolution:** validate the data plane from a **separate client machine or VM**
(the standard GameStream topology); same-host streaming is a known-hard setup.
Until then F6/F7/F8 transport code lands as scaffolding, not live-validated.

### What's confirmed (from RTSP SETUP, F5)
- Control port = 47999; **`X-SS-Connect-Data`** (u32) is the ENet connect data.
- **`X-SS-Ping-Payload`** (8 bytes) is sent to the video/audio UDP ports from the
  advertised client port to open the return path.
- The host **requests media encryption** (`encryptionRequested:1`) → control
  messages are AES-GCM with the RI key. Nonce/IV + message ids **[CAPTURE-LOCKED]**.

### Planned validation method (once the data plane is up)
Use **GCM tag verification as the oracle**: decrypt the host's inbound control
messages trying candidate IV constructions (seq ‖ rikeyid, …); a verifying tag
proves key+IV+AAD. Then encode keepalive + IDR-request to match.

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
