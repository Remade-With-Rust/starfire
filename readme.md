# Starfire

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platforms: Windows · macOS](https://img.shields.io/badge/platforms-Windows%20%C2%B7%20macOS-informational)

> **Starfire** is a low-latency game-streaming client for the Sunshine/GameStream
> wire protocol — a ground-up **Rust** alternative to
> [Moonlight](https://github.com/moonlight-stream/moonlight-qt) (GPLv3/C++), under
> a permissive license, built for the lowest achievable latency and zero copyleft
> strings.

---

## Woah, That's Cool.

Same host, same stream, same hardware — we swapped the C++ client for Starfire and
measured the **whole pipeline**, end to end: Sunshine's encoder on the PC, the
network, and Starfire's hardware decode on a MacBook Pro.

**Per-frame decode — vs Moonlight:**

| | Moonlight (C++) | **Starfire (Rust)** | Change |
|---|---:|---:|:---:|
| Per-frame decode latency | 3.3 ms | **0.4–1.1 ms** | **~3–8× faster** |
| Decode → present | CPU copy / blit | **Zero-copy** (D3D11 / Metal) | path eliminated |
| Dropped frames (LAN) | baseline | **0–2 %** | needs work |

**Full glass-to-glass — Sunshine (PC) → Starfire (Mac), 1080p60 HEVC:**

| Stage | Latency |
|---|---:|
| Host encode (Sunshine) | 8.5–11 ms |
| Network — one way (Wi-Fi) | 3–5.5 ms |
| **Client decode (Starfire)** | **0.9–1.1 ms** |
| **Pipeline total** | **~11–15 ms** |

Sustained **57–58 fps** at 1080p60 with frame pacing locked to ~16.8 ms (a clean
60 Hz cadence), and Opus stereo audio at ~181 kbps decoded on its own thread —
**zero impact** on the video path.

<sub>Measured Sunshine host (Windows) → Starfire client (Apple Silicon, VideoToolbox
HEVC + zero-copy Metal present) over Wi-Fi LAN, GPU-driven 60 fps source. Host encode
latency is read from the per-frame stream header; network is the live control-channel
RTT; decode is the per-frame hardware decode measured client-side. Windows D3D11
decode lands at the ~0.4 ms end of the range. Methodology and raw captures live in
[`docs/07-performance-budgets.md`](docs/07-performance-budgets.md).</sub>

The decoder is the **cheapest stage in the entire chain** — a frame that took
Moonlight ~3.3 ms to decode lands in **under a millisecond** on Starfire and goes
straight from the decoder to the screen without ever touching the CPU. The encoder
and the network dominate; the client gets out of the way.

## What is Starfire?

Starfire connects to a [Sunshine](https://github.com/LizardByte/Sunshine) host and
streams your desktop or games to a **Windows or macOS** client at the highest
achievable performance and quality. It speaks the same GameStream wire protocol as
Moonlight — discovery, pairing, RTSP setup, encrypted control, RTP video/audio with
Reed-Solomon FEC — but it's **100% new Rust code**: no GPL, memory-safe on every
network and decode path, and shippable as a library other software can embed.

The whole point of the rewrite is the hot path. Hardware decode (Media Foundation /
D3D11VA on Windows, VideoToolbox/Metal on macOS) feeds a **zero-copy** present
surface, with a bounded, panic-free pipeline that treats loss, reorder, and
malformed packets as normal operating conditions — not crashes.

### MAde For MATA

**MATA** has built a proprietary ground up rust implementation of Sunshine to supercharge
our Starfire implementation. Codenamed comet, the outcomes hit sub-ms speeds for 
streaming video and audio, combined with Starfire provide state of the art performance
for rust deployments. The Gaming tool deploying both of these solutions is free for 
anyone using our Home Computer application.

## Remade With Rust

**Remade With Rust** is an initiative by [Mata Network](https://www.mata.network)
to rebuild essential C and C++ tools in Rust — for the memory safety, the
predictable performance, and the freedom of a permissive license. Each project is a
clean-room reimplementation, not a fork: same wire protocols and file formats, new
code you can actually depend on.

We build the core to production grade and open-source it so the community can
extend it. No copyleft. No surprises. Just the tools we rely on, made faster and
safer.

→ More projects: **[github.com/remade-with-rust](https://github.com/remade-with-rust)**

## Features

- **Codecs:** HEVC, and H.264 — 8-bit and 10-bit (HDR10 / BT.2020 PQ passthrough).
- **Hardware decode** on both platforms: Media Foundation / D3D11VA (Windows),
  VideoToolbox (macOS), with a `dav1d` software fallback that never engages silently.
- **Zero-copy decode → present** via D3D11 shared textures (Windows) and Metal/IOSurface (macOS).
- **Zero-touch auth for fleets:** authenticate with a **MATA mID** (no PIN, no
  interactive pairing) or the standard 4-digit PIN for any Sunshine host — [see below](#authentication--deployment).
- **Full input:** keyboard, mouse (absolute + relative, high-res scroll), and gamepad,
  with anti-cheat-safe input pacing as a first-class requirement.
- **Reed-Solomon FEC** matched bit-for-bit to the host, with clean IDR recovery on heavy loss.
- **Opus audio** with A/V sync; stereo / 5.1 / 7.1.
- **Permissive license** (Apache-2.0) — embed it in closed-source software freely.
- **100% safe Rust** on the protocol core; every `unsafe` FFI boundary (the OS
  decode/present APIs) documented and isolated behind a safe trait.

## Architecture

The protocol core is OS-agnostic and pure safe Rust; the platform surface
(decode / render / input / audio) is trait-based, selected at runtime, so a new
platform is a backend behind an existing trait — never a protocol-core change.

```
┌─────────────────────────────────────────────────────────────────────┐
│ desktop app            fullscreen window · render surface · input    │
└──────────────▲──────────────────────────────▲──────────────────────┘
               │ decoded frames                │ input events
┌──────────────┴──────────┐      ┌─────────────┴───────────────────────┐
│ video decoder (trait)   │      │ sunshine-client  (THE CORE)         │
│  D3D11VA / Media Found. │◄─────│  discovery → pairing → serverinfo → │
│  VideoToolbox · dav1d   │      │  launch → RTSP → ENet control →     │
│  zero-copy present      │ OBUs │  RTP ingest + FEC + crypto →        │
└─────────────────────────┘      │  frame reassembly → input encode    │
                                 └─────────────────────────────────────┘
```

Full engineering docs and the bit-exact wire spec: [`docs/README.md`](docs/README.md).

## Authentication & deployment

Pairing is where game streaming gets painful at scale — every stock GameStream host
expects a human to read a PIN off one screen and type it into another. Starfire
keeps that path for compatibility, but defaults to something built for fleets.

- **MATA mID — zero-touch identity (default for MATA deployments).** Starfire
  authenticates with a **MATA mID**: a decentralized cryptographic identity
  (a JWS-signed credential the host verifies *entirely locally* — genesis
  self-signature → roster chain → head VM → signature — with **zero runtime calls**
  to any MATA service). The host trusts the client's mID up front, so a session
  starts with **no human-typed PIN and no interactive pairing step**. That is the
  whole point for **programmatic, headless, and fleet deployments**: provision the
  identity once, then orchestrate thousands of sessions with no manual onboarding.

- **4-digit PIN — universal compatibility.** The standard GameStream pairing ladder
  (self-signed client cert + AES-128 PIN challenge) is fully retained, so Starfire
  pairs with **any** stock Sunshine host out of the box. The default PIN is `1234`
  for unattended bring-up; override with `STARFIRE_PIN`. Auto-PIN submission (the
  client generates and submits the PIN over the host's API) removes the human from
  this path too where the host supports it.

The result: **drop-in compatible** with the existing Sunshine/GameStream ecosystem,
and **zero-touch** wherever a MATA mID is provisioned.

## Building from source

```sh
git clone https://github.com/remade-with-rust/starfire
cd starfire
cargo build --release
```

**Requirements:** Rust (stable), and the platform decode SDKs that ship with the OS
(Media Foundation / D3D11 on Windows, VideoToolbox on macOS). On macOS, Homebrew
`cmake` must be on `PATH` for the audio dependency. Protocol tests run on the host
target, e.g. `cargo test -p sunshine-client --target aarch64-apple-darwin`.

## Platform support

| Platform | Status |
|---|---|
| Windows (x86-64) | Hardware HEVC decode + zero-copy D3D11 present ✅ |
| macOS (Apple Silicon + Intel) | Zero-copy Metal present ✅ |
| Linux / Android / iOS / TV | Community — implement the decoder/render/input traits |

## Roadmap

- [ ] Full Reed-Solomon FEC parity across all loss profiles + jitter-buffer tuning
- [ ] Gamepad parity (XInput / IOKit-HID), rumble, multi-controller, DS4/DS5 basics
- [ ] AV1 + H.264 at parity with the HEVC path on both platforms
- [ ] Live stats overlay, adaptive bitrate feedback, reconnect/resilience
- [ ] Standalone OSS release + "build your own platform backend" guide

## Contributing

We welcome platform backends, codec support, and hardware testing. Note the
**clean-room policy**: we reimplement from the GameStream protocol and live-server
observation only — **never** read or copy GPLv3 source from Moonlight or
`moonlight-common-c`. See [`docs/clean-room-policy.md`](docs/clean-room-policy.md).

## License

Apache-2.0 — the patent grant matters in codec/protocol territory. No GPL/LGPL
anywhere in the dependency tree, enforced in CI via `cargo-deny`.

## About Mata Network

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.
