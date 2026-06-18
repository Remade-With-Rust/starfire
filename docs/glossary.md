# Glossary

| Term | Meaning |
|------|---------|
| **Sunshine** | The GPLv3 GameStream **host/server** we interoperate with. ☀️ The encoder. |
| **Starfire** | This project: the clean-room Rust **client**. 🌠 Never call it "Moonlight". |
| **Moonlight** | The existing GPLv3 reference client. **Off-limits source** under the clean-room policy. |
| **GameStream** | The NVIDIA-originated streaming wire protocol Sunshine implements and we speak. |
| **`/serverinfo`** | HTTP(S) endpoint returning **GameStream XML** (not JSON): host capabilities. |
| **`ServerCodecModeSupport`** | Bitfield in `/serverinfo`. AV1 support = bit `0x40000`. |
| **Pairing ladder** | The `/pair` request sequence that establishes mutual trust (PIN challenge + cert exchange). |
| **PIN KDF** | Key derivation for the pairing AES key: `SHA-256(salt ‖ pin)`, used as AES-128. |
| **RI key / IV** | "Remote Input" / session crypto material exchanged during launch/RTSP; keys the control+input AES-GCM. |
| **mTLS** | Mutual TLS. After pairing (or pre-provisioning), client+host authenticate by cert. |
| **Pre-provisioning** | Injecting the client cert into the host trust store at setup → connect pre-trusted, zero runtime pairing. |
| **RTSP** | Real-Time Streaming Protocol (TCP 48010). Negotiates streams: `OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY`. |
| **SDP** | Session Description Protocol — the body of RTSP `DESCRIBE`: formats, FEC params, audio config. |
| **ENet** | Reliable-UDP library/protocol used for the **control** channel. We use `rusty_enet` (MIT). |
| **RTP** | Real-time Transport Protocol — the UDP framing for video/audio media payloads. |
| **FEC** | Forward Error Correction. Sunshine uses **Reed-Solomon**; geometry must match bit-for-bit. |
| **RS block / shards** | A Reed-Solomon group: `k` data shards + `m` parity shards; recover up to `m` losses. |
| **Access Unit (AU)** | One coded frame's worth of bitstream handed to the decoder. |
| **OBU** | Open Bitstream Unit — AV1's packetization unit. |
| **NAL** | Network Abstraction Layer unit — HEVC/H.264's packetization unit. |
| **IDR** | Instantaneous Decoder Refresh — a clean keyframe; we request one on unrecoverable loss. |
| **Opus** | The audio codec; decoded via `opus`/`audiopus` (BSD). |
| **ABR** | Adaptive Bitrate — client feedback to the host to adjust encode bitrate. |
| **VideoToolbox** | macOS hardware decode/present framework. |
| **Media Foundation / D3D11VA** | Windows hardware decode frameworks. |
| **dav1d** | BSD AV1 software decoder — the fallback path. |
| **`cpal`** | Cross-platform audio output crate (Apache/MIT). |
| **`[CAPTURE-LOCKED]`** | A field whose authoritative value is a committed capture fixture, not prose. See [`03-bitexact-methodology.md`](03-bitexact-methodology.md). |
| **Golden test** | A test asserting our bytes equal a frozen fixture exactly. |
| **Clean-room** | Implementing from observed behavior only, never from GPL source. See [`clean-room-policy.md`](clean-room-policy.md). |
