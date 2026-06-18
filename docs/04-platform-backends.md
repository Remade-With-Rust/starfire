# 04 — Platform Backends

> The only non-Rust surface in the project. Four traits, each with per-OS impls
> selected at runtime. A community contributor adds a platform by implementing
> these **without touching `starfire-core`** ([`02-architecture.md`](02-architecture.md)).

## The four traits

| Trait | Crate | Responsibility |
|-------|-------|----------------|
| `VideoDecoder` | `starfire-decode` | AU in → decoded frame/surface out |
| `Renderer` | `starfire-render` | present surface, fullscreen, HDR, pacing |
| `AudioOutput` | `starfire-audio` | PCM in → speakers, channel layout |
| `InputBackend` | `starfire-input` | capture kbd/mouse/gamepad events |

Selection is a runtime `select.rs`-style factory, not `#[cfg]` scattered through
logic. Every `unsafe` FFI boundary is documented and isolated to its impl module.

## `VideoDecoder`

- **AV1** primary; **HEVC** + **H.264** fallback. 8-bit + 10-bit (HDR).
- **macOS:** **VideoToolbox** (hardware). Output to `CVPixelBuffer`/`IOSurface` for
  zero-copy into the renderer.
- **Windows:** **Media Foundation** and/or **D3D11VA** (hardware). Output to a
  D3D11 shared texture for zero-copy present.
- **Software fallback:** **`dav1d`** (BSD) for AV1 — used only as an explicit
  fallback, **never silently** ([`07-performance-budgets.md`](07-performance-budgets.md)).
- Contract: submit AUs in the framing §07 emits; surface IDR/decode errors back so
  the session can request recovery. No `unwrap`/`panic` on bad input.

## `Renderer`

- Low-latency present (immediate / mailbox); **exclusive or borderless
  fullscreen**.
- Color management: **BT.709 SDR** + **HDR10 / BT.2020 PQ** passthrough.
- Aspect-correct scaling; integer-scale option; optional sharpening.
- **Frame pacing** against the host clock to minimize judder + latency.
- **Zero-copy** decode→present wherever the OS allows (IOSurface on Mac, D3D11
  shared texture on Windows). Consider `wgpu` where it doesn't break zero-copy;
  drop to native Metal/D3D11 where it does.

## `AudioOutput`

- `cpal` (Apache/MIT) for device output; channel layout from §08. Owns the device,
  not the decode (Opus decode lives in `starfire-audio` logic / core-adjacent).

## `InputBackend`

- **Windows:** XInput (gamepad) + raw input (kbd/mouse).
- **macOS:** IOKit-HID.
- Captures events; **encoding stays in `starfire-core`** (§09) so the wire format
  is platform-independent.

## Why this split matters

The protocol core compiles and tests with **no OS framework linked**. That's what
lets the same source be both the standalone OSS crate and the embedded dependency,
and what lets the community add Linux/mobile/TV backends behind these traits.

## Per-backend risks

- VideoToolbox vs Media Foundation/D3D11VA are **two separate `unsafe` FFI
  integrations** with different surface and zero-copy models — budget them
  independently (Phase 1 Mac, Phase 2 Windows parity).
- HDR/10-bit correctness spans decode→present on both platforms — a dedicated
  Phase-3 task ([`05-build-plan.md`](05-build-plan.md)).
