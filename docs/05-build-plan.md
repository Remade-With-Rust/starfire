# 05 — Build Plan & Milestones

> **From scratch.** There is no scaffold to inherit — the repo is empty but for
> docs. Every layer starts at zero and is live-validated against a running
> Sunshine host ([`03-bitexact-methodology.md`](03-bitexact-methodology.md)).
> One strong Rust engineer, AI-assisted.

## Sequencing principle

Build in **connection order** (the §protocol/ docs), Mac first then Windows, one
codec (AV1) first. Each layer lands with its fixture + golden test + live note
before the next begins. Bring up a *thin vertical slice* to interactive video
fast, then thicken it to daily-driver quality, then harden.

## Feature breakdown (the unit-of-work list)

Each is discrete and independently testable; maps 1:1 to a protocol/platform doc.

| # | Unit | Doc |
|---|------|-----|
| F1 | mDNS + manual discovery + reachability/pair-status probe | protocol/01 |
| F2 | Client identity + `/pair` ladder + auto-PIN + pre-provisioning | protocol/02 |
| F3 | `/serverinfo` XML parse + codec/HDR negotiation | protocol/03 |
| F4 | `/applist` + `/launch` + `/resume` + `/cancel` | protocol/04 |
| F5 | RTSP `OPTIONS..PLAY` + SDP + crypto/port/FEC extraction | protocol/05 |
| F6 | ENet control up + AES-GCM + keepalive + IDR request | protocol/06 |
| F7 | RTP video ingest + reassembly (happy path) → AV1 OBUs | protocol/07 |
| F8 | Reed-Solomon FEC matched to Sunshine + loss/IDR handling | protocol/07 |
| F9 | VideoToolbox decode → present fullscreen (Mac) | 04 |
| F10 | Basic input (kbd/mouse) encode + send | protocol/09 |
| F11 | Opus audio ingest + A/V sync (stereo → 5.1/7.1) | protocol/08 |
| F12 | Gamepad (XInput/IOKit-HID), rumble, multi-controller | protocol/09 + 04 |
| F13 | Windows decode (Media Foundation/D3D11VA) at Mac parity | 04 |
| F14 | HEVC + H.264 fallback codecs | 04 + protocol/07 |
| F15 | Stats overlay + adaptive bitrate + reconnect | protocol/06 + 12-render |
| F16 | HDR10 / 10-bit correctness; zero-copy decode→present | 04 |
| F17 | Soak, fuzz, anti-cheat-safe input-timing audit | 06 + protocol/09 |

---

## Phase 0 — Foundations (greenfield bootstrap)

Everything below is **net-new** (the readme's "mostly done" applies to a separate
codebase, not this repo).

- ✅ Cargo **workspace** scaffold: `starfire-core`, `starfire-decode`,
  `starfire-render`, `starfire-audio`, `starfire-input`, `starfire-testkit`, `app`.
- ✅ **`cargo-deny`** license gate (`deny.toml`) + CI (`.github/workflows/ci.yml`,
  fmt/clippy/test on macOS+Windows, license gate on Linux); SPDX + clean-room headers.
- ✅ Test scaffolding: `starfire-testkit` fixture loader + `.meta.toml` convention,
  golden byte-eq, loss-injection (`drop_indices`); `wire::Wire` + `assert_roundtrip`.
- ✅ Decoder/renderer/input/audio **trait definitions** + `select.rs` (placeholder impls).
- ✅ **Capture harness — pcap→fixture slicer** (`tools/fixture-slicer`, std-only):
  parses classic pcap, demuxes by Sunshine port, reassembles RTSP/HTTP TCP
  transcripts, frames UDP media/control datagrams, writes per-layer fixtures +
  `.meta.toml` (validated to load back through `starfire-testkit`). Unblocks every
  protocol layer the moment a capture exists.
- **Exit:** ✅ `cargo fmt`/`clippy`/`test` (22 tests) + `cargo deny` all green on the
  scaffold. ☐ one *real* captured session sliced into per-layer fixtures (needs host).

## Phase 1 — Interactive E2E demo (~3–5 weeks)
*One codec (AV1), Mac first, basic input, minimal FEC. The thin vertical slice.*

> **Local Sunshine host stood up** (portable 2026.516.143833, gitignored under
> `.sunshine/`) — protocol layers now live-validate against a real server.

- 🟡 **F1 in progress:** ✅ manual host + `/serverinfo` probe + parser, golden-
  tested against a real capture and live-validated; ☐ mDNS browse (needs a
  Windows packet-capture tool); ☐ host persistence. See
  [`protocol/01`](protocol/01-discovery.md), [`protocol/03`](protocol/03-serverinfo-and-negotiation.md).
- ✅ **F2 pairing works live:** the full `/pair` ladder (ECDSA P-256 cert, PIN
  KDF + AES-128-ECB, SHA-256 hash chain, ECDSA signature) pairs a fresh identity
  against Sunshine — host lists us as trusted (`live_pair_full`). ☐ auto-PIN
  integrated in-process (needs the TLS client from F3); ☐ identity persistence;
  ☐ deterministic request-encoding golden; ☐ pre-provisioning. See
  [`protocol/02`](protocol/02-pairing-and-crypto.md).
- F3, F4 — get to a launched session.
- F5 (RTSP) → extract crypto + ports + FEC params.
- F6 (ENet control up + AES-GCM; keepalive + IDR request).
- F7 (RTP video happy path → AV1 OBUs).
- F9 (VideoToolbox decode → present fullscreen).
- F10 (basic mouse + keyboard).
- **Exit criterion:** play the Windows desktop interactively from the app, mouse +
  keyboard, **no separate Moonlight process**, on macOS.

## Phase 2 — Daily-driver quality (~+4–8 weeks)

- F8 (Reed-Solomon FEC matched to Sunshine + robust loss/IDR handling) — **the
  long pole; budget real capture time** ([`protocol/07-video-rtp-fec.md`](protocol/07-video-rtp-fec.md)).
- Jitter buffer + frame pacing to budget ([`07-performance-budgets.md`](07-performance-budgets.md)).
- F11 (Opus audio + A/V sync; 5.1/7.1).
- F12 (gamepad XInput/IOKit-HID, rumble, multi-controller).
- F13 (Windows decode Media Foundation/D3D11VA at parity with Mac).
- F14 (HEVC + H.264 fallback).
- F15 (stats overlay + adaptive bitrate + reconnect).
- **Exit criterion:** meets all §6 budgets; a multi-hour session feels
  indistinguishable from the reference client.

## Phase 3 — Production hardening & OSS launch (~+weeks, ongoing)

- F16 (HDR10 / 10-bit correctness; zero-copy decode→present both platforms).
- F17 (soak, fuzz the depacketizer/FEC, **anti-cheat-safe input-timing audit**).
- Public API freeze, docs, examples, CONTRIBUTING + clean-room policy.
- Extract `starfire-core` to a standalone permissively-licensed repo; the embedded
  consumer depends on it as a normal dependency.
- **Exit criterion:** shippable embedded **and** a credible standalone OSS release
  the community can build platform backends on.

---

## Rough total

**~3–5 months to a genuinely shippable native client**, internal demo around the
**~1-month** mark (end of Phase 1). Community contributions are *upside on the
long tail*, not a schedule input.

## Interim ship strategy (de-risk the timeline)

If a gaming feature must ship before Starfire hits Phase-2 quality, ship first with
a **reference GPL client bundled as a separate, GPL-compliant process** (source
offer, no static linking), and swap in Starfire once it reaches Phase-2. This
decouples "ship gaming" from "finish the 3–5 month client." Everything in the
host-side stack (masked VM, pairing, pre-provisioning, host networking) is reused
either way.

## Critical-path risks (where weeks become months)

1. **FEC geometry bit-match** (F8) — the long pole; capture-budget it heavily.
2. **Latency & pacing tuning** — "it works" → "it feels great" is a large gap.
3. **Two separate HW-decode FFI integrations** (VideoToolbox vs MF/D3D11VA).
4. **Input fidelity + anti-cheat-safe timing.**
5. **HDR / 10-bit correctness** across decode→present.
6. **Clean-room discipline** — one GPL peek poisons the license
   ([`clean-room-policy.md`](clean-room-policy.md)).
