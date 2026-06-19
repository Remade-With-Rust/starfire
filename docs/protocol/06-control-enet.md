# Protocol 06 — Control Stream (ENet over UDP)

> Provenance: observation against **Sunshine 2026.516.143833**, plus a one-time,
> owner-approved read of the **Sunshine server** source (`rtsp.cpp`/`stream.cpp`/
> `platform/windows/misc.cpp`) for the data-plane wire format. Never Moonlight
> (the client). Clean-room w.r.t. the client. Crate: `rusty_enet` (MIT).

## ✅ Status — data plane LIVE (control + video), 2026-06-19
The ENet control channel **connects** and the full data plane streams **plaintext
HEVC** end-to-end, validated from a real Mac client against a Windows Sunshine
host (`live_explore_video`: 2060 video packets / 2.9 MB in 8 s, HEVC NALs seen).

**The arming sequence that makes it work (order matters):**
1. RTSP `OPTIONS → DESCRIBE → SETUP×3 → ANNOUNCE → PLAY`. The **ANNOUNCE** is
   mandatory to arm the session (see [05-rtsp.md]). Without it PLAY 200s but the
   host never streams and the ENet `connect` gets no VERIFY.
2. **ENet control `connect`** to the control port with the `X-SS-Connect-Data`
   token. This is what sets the host's `session->localAddress` — derived from the
   control peer's *server-side* socket address. [SOURCE: stream.cpp
   `control_server_t::get_session()`]
3. **Ping** the video+audio UDP ports (see below). The host records each stream's
   client peer from the ping's source, then starts capture.
4. Plaintext HEVC RTP flows to the ping's source socket.

**Why control must connect _before_ video can flow:** the host sends RTP with
`WSASendMsg` using `localAddress` as the IP_PKTINFO source. Until the control
peer connects, `localAddress` is unset and **every send fails `WSAEINVAL`
(10022)** — the encoder runs but no packets leave the host. This is the single
biggest non-obvious dependency in the whole data plane.

**Encryption:** with `lan_encryption_mode=0` and the ANNOUNCE carrying
`a=x-ss-general.encryptionEnabled:0`, control + video + audio are all
**plaintext** (no AES-GCM) — matching the "less overhead" goal. (Note: the
DESCRIBE still advertises `encryptionRequested:1`, but that is the host's
*request*, not a requirement, when its mode is not MANDATORY.)

### The ping (hole-punch)
`recv_ping` accepts two formats [SOURCE: stream.cpp]:
- **legacy**: the datagram must equal the literal `"PING"` (4 bytes) — used when
  `mlFeatureFlags` lacks `ML_FF_SESSION_ID_V1`;
- **new**: the datagram must *contain* `av_ping_payload` (the `X-SS-Ping-Payload`).

Starfire sends `a=x-ml-general.featureFlags:0` (no `ML_FF_SESSION_ID_V1`), so it
uses the simple legacy `"PING"`. Both video and audio ports must be pinged or the
audio timeout tears the session down.

### Historical note — earlier blockers (now resolved)
The transport below was structurally complete long before it was live; the data
plane was blocked first by environment (same-machine), then by the missing
ANNOUNCE (arming), the wrong ping payload, and the `localAddress` ordering above.

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

### Update — cross-machine attempt (real Mac client → Windows host)
Ran the client from a **real second machine** (Apple-Silicon Mac, macOS 14.6) over
the LAN against the Windows Sunshine. Results:
- ✅ **Control plane works cross-machine + on macOS**: discover→pair→serverinfo→
  applist→launch→RTSP all succeed from the Mac (the client core builds + runs on
  arm64 macOS — validates that target). Test infra is `STARFIRE_TEST_HOST`-aware.
- ✅ Sunshine **creates the encoder** for the Mac session (`Creating encoder
  [hevc_qsv]`) — the host is willing to stream.
- ⛔ **Data plane still doesn't connect.** ENet connect to 47999 **times out**
  (vs same-machine's ICMP-unreachable). Enabling ENet's **CRC32 checksum** (the
  Moonlight customization, `rusty_enet::crc32`) did not fix it.
- 🔑 **Key finding:** `netstat -an` on the host shows Sunshine **does not bind
  47998/47999/48000 as UDP listeners** at all, even with the encoder running. So
  the RTSP `SETUP` `server_port` values are **not** plain UDP listen ports — the
  real port/socket model is unknown and **must be read from a capture**.

### ✅ Captured a working Moonlight↔Sunshine session (2026-06-18)
Ran Moonlight on the Mac → streamed the Windows Sunshine (720p HEVC, decoded fine
via VideoToolbox) and captured it with `tcpdump`, then sliced it with our own
`fixture-slicer` (which **validated the Phase-0 tool on real data**). Capture +
slices live under `.sunshine/` (gitignored). The ground-truth data-plane map:

| Layer | Flow | Detail |
|-------|------|--------|
| RTSP | client → :48010, **7 separate TCP connections** | confirms one-request-per-connection; one client request is **1684 B and binary** (encrypted config/ANNOUNCE — `encryptionRequested:1`) |
| Control | client `:50319` → `:47999`, 79 ENet datagrams | **ENet CONNECT = 52 B** (4 hdr + **4 CRC32** + 44 cmd) → server **VERIFY_CONNECT = 48 B**. Confirms ENet **with CRC32**. |
| Video | client `:50683` → `:47998`, 496 datagrams, ~694 KB | RTP/HEVC |
| Audio | client `:56488` → `:48000`, 263 datagrams | RTP/Opus |

### Corrected data-plane model (the thing I had wrong)
- The client binds a **separate ephemeral UDP port per stream** (video/audio/control
  each get their own). It is **not** a fixed `50000`/`X-GS-ClientPort`.
- Sunshine **does not bind 47998/48000 as listeners** — it **sends from** them to
  the client's ephemeral port (which is why host `netstat` showed nothing). Control
  (47999) is genuinely bidirectional ENet.
- The RTSP **ANNOUNCE/encrypted config is present** in Moonlight's flow — Starfire
  skips it, which is the likely reason the host never arms the data plane for us.

### Next phase — implement F6/F7 against the capture
With the capture in hand the work is well-defined: (1) send the RTSP
ANNOUNCE/config; (2) confirm rusty_enet emits the 52-B CRC32 CONNECT and completes;
(3) decrypt control via the GCM-tag-verify oracle; (4) depacketize the RTP video.
The `ControlChannel` (ENet + CRC32) is committed as scaffolding pending this.

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
