# 02 — Architecture

> Built from scratch. Nothing below is inherited from an existing scaffold;
> every box is a layer we create and live-validate
> ([`03-bitexact-methodology.md`](03-bitexact-methodology.md)).

## Shape: protocol core is OS-agnostic; platform surface is trait-based

```
┌──────────────────────────────────────────────────────────────────────┐
│ app (Dioxus)                      consumer: fullscreen window, session │
│   - launches a session, owns the render surface + input capture        │
└───────────────▲───────────────────────────▲──────────────────────────┘
                │ decoded frames             │ input events
┌───────────────┴─────────┐     ┌────────────┴─────────────────────────┐
│ starfire-decode         │     │ starfire-core (THE PROTOCOL CORE)     │
│  trait VideoDecoder     │     │  discovery → pairing → serverinfo →   │
│   - videotoolbox (Mac)  │◄────│  applist/launch → RTSP → ENet control │
│   - media_foundation /  │ AV1 │  → RTP video+audio ingest + FEC +     │
│     d3d11 (Windows)     │ OBUs│  crypto → frame reassembly → input    │
│   - dav1d (SW fallback) │     │  encode. 100% Rust, permissive, no GPL.│
└─────────────────────────┘     └───────────────────────────────────────┘
```

## Crate layout (proposed)

A Cargo workspace. Names are provisional; keep the core free of any platform or
UI dependency so it can be extracted to a standalone OSS repo.

| Crate | Responsibility | Deps it may have |
|-------|----------------|------------------|
| `starfire-core` | The whole protocol core: discovery, pairing, crypto, serverinfo, applist/launch, RTSP, ENet control, RTP video/audio ingest, FEC, reassembly, input encode, session state machine. **No OS, no UI.** | `rusty_enet`, `reed-solomon-erasure`, `aes`/`aes-gcm`, `ring`, `sha2`, `rcgen`, `rustls`, `quick-xml`, async runtime |
| `starfire-decode` | `trait VideoDecoder` + per-OS impls (VideoToolbox, Media Foundation/D3D11VA, dav1d SW). Thin `unsafe` FFI behind a safe trait. | OS frameworks, `dav1d` |
| `starfire-render` | `trait Renderer` / present + fullscreen + HDR + pacing. Zero-copy from decode where the OS allows. | wgpu/Metal/D3D11 |
| `starfire-audio` | `trait AudioOutput`, Opus decode, channel layout, A/V sync clock. | `opus`/`audiopus`, `cpal` |
| `starfire-input` | `trait InputBackend` (XInput / IOKit-HID capture); encoding lives in core. | OS HID frameworks |
| `app` (Dioxus) | The consumer: window, session lifecycle, settings, stats overlay. | the crates above, `dioxus` |

> The protocol core intentionally owns **input *encoding*** (the wire format),
> while `starfire-input` owns **input *capture*** (reading the OS devices). The
> wire format is protocol; capture is platform.

## Trait seams (the community extension points)

Four traits are the entire platform surface. Each is selected at **runtime**
(mirror a `select.rs` pattern), never via `#[cfg]` scattered through logic.

- `VideoDecoder` — submit access units (AV1 OBUs / HEVC|H.264 NALs), receive
  decoded frames/surfaces. Impls: VideoToolbox, MediaFoundation/D3D11VA, dav1d.
- `Renderer` — present a decoded surface; own fullscreen, HDR, pacing.
- `AudioOutput` — accept decoded PCM, own device + channel layout.
- `InputBackend` — capture keyboard/mouse/gamepad events for the core to encode.

A community contributor adds a platform by implementing these — without touching
`starfire-core`.

## Threading & data-flow model

Latency-critical, so the hot path is explicit and lock-light:

- **Network RX threads** (per UDP stream) read packets into bounded ring buffers.
  No per-packet allocation in steady state.
- **Reassembly/FEC stage** consumes video RTP, runs RS recovery, reorders by
  sequence, emits complete access units to a bounded queue.
- **Decode stage** pulls AUs, submits to the `VideoDecoder`, pushes surfaces.
- **Render stage** presents on the display clock with frame pacing.
- **Audio path** is parallel: RX → FEC → Opus → `AudioOutput`, synced to the
  video clock.
- **Control path** (ENet) handles input TX, IDR requests, keepalive, stats,
  rumble/HDR/ABR messages — reliable-UDP, AES-GCM.
- **Session state machine** orchestrates the connection phases (below) and owns
  reconnect / teardown.

Hot-path rules: bounded + lock-free where possible, zero-copy decode→present
where the OS allows, **no `unwrap`/`panic`** on any network/decode input.

## Connection lifecycle (the state machine)

This is the order the protocol docs are written in:

```
Discover ─► Pair (or pre-trusted mTLS) ─► /serverinfo + negotiate ─►
/applist ─► /launch ─► RTSP (OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY) ─►
[extract RI key/IV + ports + FEC params] ─► ENet control up (AES-GCM) ─►
RTP video+audio ingest ──► decode ──► present + audio
        ▲                                         │
        └──────── IDR request / reconnect on loss ◄┘
Quit ─► clean teardown (/cancel)
```

Each arrow is a layer doc under [`protocol/`](protocol/00-overview.md).

## License & provenance posture (architectural constraints)

- **Apache-2.0** for the core (patent grant matters in codec/protocol territory;
  see [`08-open-source-and-license.md`](08-open-source-and-license.md)). No
  copyleft anywhere in the tree, gated by `cargo-deny` in CI.
- Every protocol module header carries the clean-room provenance line.
- The core compiles and tests **without** any OS framework, so it can be the
  standalone OSS artifact and the embedded dependency from the same source.
