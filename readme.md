# Catching Starfire

> **Starfire** is MATA's (www.mata.network) clean-room, permissively-licensed 
> Rust client for the Sunshine GameStream wire protocol — our Moonlight. 
> It connects to a Sunshine host (the encoder running in the MATA masked Windows > gaming VM) and streams the session to a **Windows or macOS** client at the 
> highest achievable performance and quality. We build the PC + Mac client to 
> production grade; we open-source it so the community builds the rest (Linux, 
> mobile, TV, exotic input).

Moonlight source: https://github.com/moonlight-stream/moonlight-qt

**Status:** Design / build plan. **Greenfield — built from scratch.** This repo
currently contains only docs; no code scaffold yet. The full engineering plan and
the bit-for-bit Sunshine wire spec live in [`docs/`](docs/README.md) — start with
[`docs/README.md`](docs/README.md) and [`docs/03-bitexact-methodology.md`](docs/03-bitexact-methodology.md).

**Codename theme:** Sunshine (server, ☀️) → Starfire (client, 🌠). Do **not** use
the name "Moonlight" anywhere in code, brand, or docs.

---

## 1. Why we're building this

### The strategic bet
- **There is no mature, permissively-licensed GameStream client.** Moonlight is
  GPLv3; `moonlight-common-c` is GPLv3. A clean **Rust** implementation under a
  permissive license is genuinely novel and fills a real gap the homelab,
  emulation, cloud-gaming, and Rust communities actively want.
- **The client is a commodity enabler, not our moat.** MATA's differentiation is
  the masked VM + anti-cheat presentation, the home-computer infrastructure,
  identity, and the integrated one-click UX. Giving away an expensive-to-build
  commodity so the community helps maintain it (cross-platform decoders, FEC
  edge cases, oddball hardware) is a good trade.
- **Open-source pays down our worst cost: the long tail.** The community tests
  across hardware and Sunshine configs we will never own.
- **Brand fit.** A permissive, self-hostable, no-GPL-strings streaming client is
  on-narrative for MATA's sovereignty ethos, and a strong systems-engineering
  recruiting signal.

### Dual deployment
Starfire ships **two ways from one codebase**:
1. **Open-source crate(s)** — standalone repo, permissive license, clean public
   API. This is the marketing + community artifact.
2. **Embedded in the MATA toolkit** — `packages/desktop` depends on the crate
   like any other consumer. The permissive license is what lets us embed it in
   our closed-source app freely.

### Non-negotiable: clean-room provenance
- **Never read Moonlight / `moonlight-common-c` GPL source while writing
  Starfire.** Implement from observed wire behavior + permissively-licensed
  component crates only. This is exactly how the scaffold was built
  (reverse-engineered against live Sunshine).
- Document provenance ("derived from protocol observation against Sunshine
  vX.Y") in module headers. It's cheap legal insurance and a selling point.
- Sunshine (the server) is GPLv3 — *interoperating* with it is fine; we are not
  derived from it.

---

## 2. Scope

### In scope (this effort — production grade)
- **Clients:** Windows (x86-64) and macOS (Apple Silicon + Intel).
- **Host:** Sunshine (as run in the MATA gaming VM): GameStream protocol,
  `/serverinfo` XML, ports 47984/47989 (HTTPS/HTTP), 48010 (RTSP/TCP),
  47998–48000 (RTP video/audio + control/UDP).
- **Codecs in:** AV1 (primary — Sunshine here advertises AV1-Main8 via
  `ServerCodecModeSupport & 0x40000`), HEVC, H.264 (fallback). Opus audio.
- **Every Moonlight capability required to connect, stream, and play** (see §5).

### Out of scope (hand to the community)
- Linux / Android / iOS / web / TV / embedded clients.
- Exotic input (gyro/touchpad passthrough beyond DS4/DS5 basics, racing wheels).
- Co-op / multi-session UI, game library management, account systems.
- Being the *host* (that's Sunshine; replacing the GPL server is a separate,
  larger project — explicitly not here).

### Explicit principle
> Build the **Windows + Mac client to production grade today**; design every
> seam so a community contributor can add a platform decoder/render/input
> backend behind an existing trait without touching the protocol core.

---

## 3. Coding Requirements (baseline)

- All code must be **production-grade** with **security-first** design
  decisions. **No bandaging problems** — create tests, validate outcomes, build
  production grade only.
- **Dioxus** for Rust development across web, PWA, native desktop
  (Windows/macOS), and mobile.
- **Rust in all aspects** of the program (or WASM for browser targets).
- Encryption primitives: Argon2id KDF + AES-256-GCM authenticated encryption
  (use the same vetted crates already in the workspace).
- Per-entry / granular storage where state is persisted (no single-blob formats).

### Starfire-specific additions
- **100% Rust** for the protocol core, FEC, crypto, depacketization, session
  state, and input encoding. The *only* permitted non-Rust surface is the OS
  hardware-decode + present API boundary (VideoToolbox / Media Foundation /
  D3D11), accessed through thin `unsafe` FFI behind a safe Rust trait. Document
  and isolate every such boundary.
- **Permissive license only** in the dependency tree. **Zero GPL/LGPL.** CI gate
  on `cargo-deny` to fail the build on any copyleft dependency.
- **Clean-room provenance** header in every protocol module (see §1).
- **Test every protocol layer against live Sunshine**, not just unit mocks.
  Each layer lands with: unit tests (wire encode/decode round-trips) **and** a
  captured-from-live fixture **and** a live-validation note. There is no clean
  spec — the live server is ground truth.
- **Performance budgets are acceptance criteria, not aspirations** (see §6).
  A layer is not "done" until it meets its budget on real hardware.
- **Cross-platform via traits, not `#[cfg]` soup.** Decoder, renderer, input,
  and audio-output are each a trait with per-OS impls selected at runtime
  (mirror the existing `mata-av1-decoder` `select.rs` pattern).
- **No `unwrap()`/`panic!` on the hot path** or on any network/decode input.
  Loss, reorder, malformed packets, and decoder hiccups are normal operating
  conditions and must degrade gracefully (request IDR, drop frame, reconnect).

---

## 4. Architecture

Three crates, one consumer. The protocol core is OS-agnostic; the platform
surface is trait-based.

```
┌──────────────────────────────────────────────────────────────────────┐
│ packages/desktop (Dioxus)         consumer: fullscreen window, session │
│   - launches a session, owns the render surface + input capture        │
└───────────────▲───────────────────────────▲──────────────────────────┘
                │ frames (decoded)           │ input events
┌───────────────┴─────────┐     ┌────────────┴─────────────────────────┐
│ mata-av1-decoder        │     │ mata-sunshine-client (THE CORE)       │
│  trait VideoDecoder     │     │  discovery → pairing → serverinfo →   │
│   - videotoolbox.rs Mac │◄────│  applist/launch → RTSP → ENet control │
│   - media_foundation.rs │ AV1 │  → RTP video+audio ingest + FEC +     │
│     / D3D11 Windows     │ OBUs│  crypto → frame reassembly → input    │
│   - software.rs (dav1d) │     │  encode. 100% Rust, BSD-2, no GPL.    │
└─────────────────────────┘     └───────────────────────────────────────┘
```

### Crate inventory & current state
| Crate / module | Purpose | State |
|---|---|---|
| `mata-sunshine-client/discovery.rs` | mDNS + manual host discovery | scaffold |
| `…/pairing/{crypto,http,tls_cert,mod}.rs` | AES-128 PIN challenge, cert exchange, `/serverinfo` XML | **pairing solved live**; pre-provision in flight |
| `…/rtsp.rs`, `…/transport/rtsp_client.rs` | RTSP session over TCP 48010 | parser done; live exchange = next |
| `…/transport/enet_control.rs` (`rusty_enet 0.4`) | reliable-UDP control channel | scaffold |
| `…/frame/{mod,reassembly}.rs` | RTP depacketize + FEC + frame reassembly | scaffold |
| `…/input.rs` | keyboard/mouse/gamepad encode | scaffold |
| `mata-av1-decoder/{videotoolbox,media_foundation,software,select}.rs` | HW/SW decode behind a trait | scaffold incl. Mac + Win backends |
| `packages/desktop/{main,frame}.rs` | render + fullscreen + input capture + wiring | scaffold |

### Permissive component crates (the leverage)
- Control transport: **`rusty_enet`** (MIT) — already a dep.
- FEC: **`reed-solomon-erasure`** (MIT/Apache) — *must match Sunshine's RS block
  geometry exactly* (see §7 risks).
- Audio: **`opus`** / `audiopus` (BSD) for decode; `cpal` (Apache/MIT) for output.
- Video: **VideoToolbox** (Mac) / **Media Foundation / D3D11VA** (Windows) for
  hardware AV1/HEVC/H.264; **`dav1d`** (BSD) software fallback.
- Crypto: **`aes`/`aes-gcm`**, **`ring`**, **`sha2`** (already deps).
- TLS / certs: **`rcgen`** (already a dep), `rustls`.

---

## 5. Feature breakdown — everything Moonlight does to connect to Sunshine

Each item is a discrete, independently-testable unit. Status reflects the
current scaffold.

### 5.1 Discovery & host management
- mDNS discovery of Sunshine hosts (`_nvstream._tcp`) + manual IP entry.
- Persist known hosts + their paired cert/identity.
- Reachability probe (`/serverinfo` over HTTP 47989).

### 5.2 Pairing & identity — **solved live**
- Generate/persist a client identity (P-256 self-signed cert via `rcgen`).
- `/pair` ladder: `getservercert` → AES-128 PIN challenge
  (`SHA-256(salt ‖ pin)` KDF, ECB) → `clientchallenge` → `serverchallengeresp`
  → `clientpairingsecret` → cert added to the host's trusted set.
- **Auto-PIN** (no human types the PIN — the client generates it, the agent
  submits it via Sunshine's `/api/pin`).
- **Pre-provisioning** (inject the client cert into the host's trust store at VM
  setup → connect pre-trusted over mTLS, zero runtime pairing). *In active
  debugging.*

### 5.3 Server capabilities
- Parse `/serverinfo` **GameStream XML** (not JSON): hostname, app version, pair
  status, HTTPS port, `ServerCodecModeSupport` (AV1 = `0x40000`), max resolution
  / FPS, HDR support, surround capability, current game.
- Negotiate the best mutually-supported config (codec, resolution, FPS, bit
  depth, HDR, color space).

### 5.4 App list & session launch
- `/applist` (Desktop, Steam Big Picture, custom apps).
- `/launch?appid=…&mode=WxH x FPS&…` with full launch params (bitrate, packet
  size, HDR, surround, gamepad mask, client cert hash, RI key/IV).
- `/resume` (rejoin a running session) and `/cancel` (terminate).

### 5.5 RTSP session setup (TCP 48010)
- `OPTIONS` → `DESCRIBE` (parse SDP: supported formats, FEC params, audio
  config) → `SETUP` (video/audio/control streams + ports) → `ANNOUNCE` (push
  negotiated config) → `PLAY`.
- Extract per-stream crypto material (RI key/IV) + FEC parameters + stream
  ports from the exchange.

### 5.6 Control stream (ENet over UDP)
- Reliable-UDP channel via `rusty_enet`, **AES-GCM encrypted** with the
  RTSP-negotiated key.
- Carries: input events (§5.9), **IDR/keyframe requests** on loss, periodic
  ping/keepalive, loss + RTT stats, HDR mode changes, rumble/haptics from host,
  adaptive-bitrate feedback, graceful termination.

### 5.7 Video ingest & reassembly (UDP 47998)
- RTP receive → depacketize → **Reed-Solomon FEC** recovery → reorder by
  sequence → reassemble into complete frames → emit codec access units (AV1
  OBUs / HEVC or H.264 NAL units in the right format for the decoder).
- Loss handling: detect missing/unrecoverable frames → request IDR over control
  → drop to the next keyframe; never feed the decoder a corrupt frame.
- Stats: per-frame receive time, FEC recovery rate, decode queue depth.

### 5.8 Audio ingest (UDP 48000)
- RTP receive → FEC → **Opus** decode → channel layout (stereo / 5.1 / 7.1) →
  output via `cpal` with A/V sync against the video clock.

### 5.9 Input
- **Keyboard** (scancodes + modifiers), **mouse** (absolute + relative motion,
  buttons, high-res scroll), **gamepad** (XInput on Windows / IOKit-HID on Mac;
  multiple controllers, analog triggers, rumble, and DS4/DS5 basics:
  battery/touchpad/gyro where feasible).
- Encode into Sunshine's input packet formats, AES-GCM encrypt, send over the
  control stream with correct coordinate scaling for the stream resolution.
- **Input timing must not look synthetic** (anti-cheat-safe pacing) — a
  first-class requirement, not an afterthought.

### 5.10 Decode (trait: `VideoDecoder`)
- **AV1** primary; **HEVC** + **H.264** for compatibility. 8-bit + 10-bit (HDR).
- Hardware: **VideoToolbox** (Mac), **Media Foundation / D3D11VA** (Windows).
- Software fallback: **`dav1d`**.
- Zero-copy from decode → render surface where the platform allows (IOSurface /
  D3D11 shared texture).

### 5.11 Render, present & fullscreen
- Low-latency present (immediate / mailbox), exclusive or borderless fullscreen.
- Color management: BT.709 SDR + **HDR10 / BT.2020 PQ** passthrough.
- Aspect-correct scaling; integer-scale option; optional sharpening.
- Frame pacing against the host clock to minimize judder + latency.

### 5.12 Session management & resilience
- Live stats overlay (RTT, packet loss, FEC recovery, decode time, render FPS,
  bitrate, frame drops).
- Adaptive-bitrate feedback to the host.
- Reconnect on transient network loss; clean teardown on quit; IDR recovery.

---

## 6. Performance & quality targets (acceptance criteria)

> "Optimize for absolute highest performance and quality." These are gates, not
> aspirations. A layer is not done until it meets them on real hardware.

- **Added latency:** client-introduced latency (wire-arrival → photons) within a
  small fixed budget (target ≤ 1 frame-time over the network RTT at the session
  FPS; measure decode-in→present-out and publish it).
- **Resolution / FPS:** support up to **4K @ 120 FPS** and **HDR10** end-to-end
  where the host + display allow; never the bottleneck below the host's caps.
- **Decode:** hardware path on both platforms; software (`dav1d`) only as
  fallback, never silently.
- **Loss resilience:** smooth playback through realistic packet loss (target:
  no visible artifacts up to the FEC's design recovery rate; clean IDR recovery
  beyond it — no green-screen / persistent corruption).
- **Pacing:** no judder from client-side timing; jitter buffer tuned for
  latency-vs-smoothness, exposed as a setting.
- **Zero-copy** decode→present wherever the OS allows; bounded, lock-free hot
  path; no allocs per packet in steady state.
- **Input:** sub-frame input encode + send latency; correct scaling; pacing that
  does not trip anti-cheat.
- **Stability:** zero panics on malformed/lossy input; soak-test a multi-hour
  session without leak or drift.

---

## 7. Risks (where weeks become months)

1. **FEC geometry must match Sunshine exactly** — the Reed-Solomon block sizes /
   matrix and the RTP framing have to match bit-for-bit. This is the long pole;
   budget real time to capture + match against live captures.
2. **Latency & pacing tuning** — "it works" → "it feels great" is a large gap
   (jitter buffer, render pacing, loss concealment).
3. **Cross-platform hardware decode** — VideoToolbox vs Media Foundation/D3D11VA
   are two separate `unsafe` FFI integrations with different surface/zero-copy
   models.
4. **Input fidelity + anti-cheat-safe timing** — packet formats, scaling,
   encryption, multi-controller, and humanlike pacing.
5. **HDR / 10-bit color correctness** across decode → present pipelines.
6. **Clean-room discipline** — a single GPL-source peek poisons the permissive
   license. Process risk, mitigated by §1 governance.

---

## 8. Build plan & milestones

One strong Rust engineer, AI-assisted, each layer **live-validated against the
running Sunshine in the MATA gaming VM** (the data plane is already de-risked).

### Phase 0 — Foundations (mostly done)
- ✅ Pairing solved live (auto-PIN); pre-provisioning in debug.
- ✅ `/serverinfo` GameStream-XML parsing.
- ✅ Decoder + render scaffolds with Mac + Windows backends present.
- ☐ `cargo-deny` CI gate (no GPL/LGPL); clean-room headers; capture harness.

### Phase 1 — Interactive E2E demo (~3–5 weeks)
*One codec (AV1), Mac first then Windows, basic input, minimal FEC.*
- RTSP session live (DESCRIBE/SETUP/ANNOUNCE/PLAY) → extract crypto + ports.
- ENet control up + AES-GCM; send keepalive + IDR request; basic input.
- RTP video ingest + reassembly (happy path) → AV1 OBUs.
- VideoToolbox decode → present fullscreen.
- **Exit criterion:** play the Windows desktop interactively from the MATA app,
  mouse + keyboard, no separate Moonlight process.

### Phase 2 — Daily-driver quality (~+4–8 weeks)
- Reed-Solomon FEC recovery (matched to Sunshine) + robust loss/IDR handling.
- Jitter buffer + frame pacing to budget.
- Opus audio + A/V sync; 5.1/7.1.
- Gamepad (XInput / IOKit-HID), rumble, multi-controller.
- Windows decode (Media Foundation / D3D11VA) at parity with Mac.
- HEVC + H.264 fallback codecs.
- Stats overlay + adaptive bitrate + reconnect.
- **Exit criterion:** meets all §6 budgets; a multi-hour gaming session feels
  indistinguishable from reference Moonlight.

### Phase 3 — Production hardening & open-source launch (~+weeks, ongoing)
- HDR10 / 10-bit correctness; zero-copy decode→present on both platforms.
- Soak tests, fuzzing the depacketizer/FEC, anti-cheat-safe input timing audit.
- Public API freeze, docs, examples, CONTRIBUTING + clean-room policy.
- Extract to a standalone permissively-licensed repo; MATA toolkit consumes it
  as a dependency.
- **Exit criterion:** shippable in the MATA toolkit **and** a credible
  standalone OSS release the community can build platform backends on.

### Rough total
**~3–5 months to a genuinely shippable native client**, internal demo around the
**1-month** mark. Community contributions are *upside on the long tail*, not a
schedule input.

### Interim ship strategy (de-risk the timeline)
Ship the MATA gaming feature first with **reference Moonlight bundled as a
separate GPL-compliant process** (source offer, no static linking), and swap in
Starfire once it hits Phase-2 quality. Decouples "ship gaming" from "finish the
3–5 month client"; everything built this session (masked VM, pairing,
pre-provisioning, host networking) is reused either way.

---

## 9. Testing & validation strategy

- **Per-layer:** unit tests (wire encode/decode round-trips) + a verbatim
  captured-from-live fixture + a live-validation note. (The repo already does
  this — e.g. the `/serverinfo` XML fixture.)
- **Capture harness:** `tcpdump` on `virbr0` of a reference session (RTSP/TCP
  48010 plaintext, RTP framing) as the ground-truth corpus for FEC + framing.
- **Loss injection:** deterministic packet-loss/reorder tests against the
  reassembly + FEC layer.
- **Soak:** multi-hour session leak/drift watch.
- **Fuzz:** the depacketizer + FEC decoder against malformed input.
- **Build target discipline:** the workspace defaults to wasm; protocol tests
  run on the host target, e.g.
  `cargo test -p mata-sunshine-client --target aarch64-apple-darwin`.

---

## 10. Open-source & community strategy

- **License:** **Apache-2.0** for the core (patent grant matters in
  codec/protocol territory) — the existing crate is BSD-2-Clause, which is also
  fine; pick one and apply consistently. **No copyleft anywhere.**
- **Repo:** standalone, own brand (not "Moonlight"), clean public API, CI,
  examples, a "build your own platform backend" guide pointing at the decoder /
  render / input / audio traits.
- **Governance:** DCO or CLA so MATA can keep relicensing/embedding flexibility;
  documented clean-room policy; `cargo-deny` license gate in CI.
- **Commitment honesty:** either staff light-touch stewardship or label it
  "source-available, best-effort." An abandoned thrown-over-the-wall repo
  generates ill will, not goodwill.
- **What the community builds:** Linux/Android/iOS/web/TV clients, additional
  platform decoder/render/input backends, exotic input devices — all behind the
  traits the PC/Mac client already defines.