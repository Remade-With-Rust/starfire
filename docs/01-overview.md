# 01 — Overview & Scope

> Provenance: distilled from the strategic brief in [`../readme.md`](../readme.md).
> This doc is the engineering-facing statement of *what we are building and what
> we are deliberately not*.

## What Starfire is

A from-scratch, 100% Rust client that speaks the Sunshine GameStream wire
protocol well enough to **discover → pair → negotiate → launch → stream → play**
a remote desktop/game session, at production quality, on **Windows and macOS**.
It is a functional replicate of Moonlight's *core streaming features*; it is not
a clone of Moonlight's code or UI, and it never reads Moonlight's GPL source.

UI, where required, is **Dioxus** (Rust). The protocol core has no UI.

## The strategic bet (one paragraph)

There is no mature, permissively-licensed GameStream client — Moonlight and
`moonlight-common-c` are GPLv3. A clean **Rust** implementation under a
permissive license is genuinely novel and fills a gap the homelab, emulation,
cloud-gaming, and Rust communities want. The client is a commodity enabler, not
a moat, so we build the expensive Windows+Mac core to production grade and
open-source it so the community builds the long tail (Linux, mobile, TV, exotic
input) behind our trait seams.

## In scope (build to production grade)

- **Clients:** Windows (x86-64) and macOS (Apple Silicon + Intel).
- **Host:** Sunshine. GameStream protocol, `/serverinfo` XML, the HTTPS/HTTP
  control ports, RTSP/TCP, and the RTP video/audio + ENet control UDP streams.
- **Codecs in:** **AV1** (primary — Sunshine advertises AV1 via
  `ServerCodecModeSupport & 0x40000`), **HEVC**, **H.264** (fallback). **Opus**
  audio.
- **Every capability required to connect, stream, and play** — see the protocol
  layer docs and [`05-build-plan.md`](05-build-plan.md) §feature breakdown.

## Out of scope (hand to the community, behind traits)

- Linux / Android / iOS / web / TV / embedded clients.
- Exotic input beyond DS4/DS5 basics (racing wheels, full gyro/touchpad chains).
- Co-op / multi-session UI, game-library management, account systems.
- Being the **host** — that's Sunshine. Replacing the GPL server is a separate,
  larger project, explicitly not here.

## The governing principle

> Build the **Windows + Mac client to production grade**; design every seam so a
> community contributor can add a platform decoder / renderer / input / audio
> backend behind an existing trait **without touching the protocol core**.

## Baseline engineering requirements

- **Production-grade, security-first. No bandaging.** Every layer lands with
  tests and validated outcomes.
- **100% Rust** for protocol, FEC, crypto, depacketization, session state, input
  encoding. The *only* permitted non-Rust surface is the OS hardware-decode /
  present API boundary (VideoToolbox / Media Foundation / D3D11), reached through
  thin `unsafe` FFI behind a safe Rust trait. Document and isolate every such
  boundary.
- **Permissive license only** in the whole dependency tree. **Zero GPL/LGPL.**
  CI fails the build on any copyleft dep (`cargo-deny`).
- **Cross-platform via traits, not `#[cfg]` soup.** Decoder, renderer, input,
  audio-output are each a trait with per-OS impls selected at runtime.
- **No `unwrap()` / `panic!` on the hot path** or on any network/decode input.
  Loss, reorder, malformed packets, and decoder hiccups are *normal* operating
  conditions and must degrade gracefully (request IDR, drop frame, reconnect).
- **Performance budgets are acceptance criteria**, not aspirations
  ([`07-performance-budgets.md`](07-performance-budgets.md)).

## Dual deployment (informs the public API)

Starfire ships two ways from one codebase, which is why the core must have a
clean public API and zero closed-source assumptions:

1. **Open-source crate(s)** — standalone repo, permissive license. Marketing +
   community artifact.
2. **Embedded** — a closed-source consumer app depends on the crate like any
   other dependency. The permissive license is what makes embedding lawful.
