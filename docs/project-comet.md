# Project Comet — a permissive Rust streaming host

> Status: **exploration / vision**. Nothing here is committed engineering yet.
> This doc frames the idea space so we can pick experiments deliberately.
> Companion to Starfire (the client). Comet is the *host* — a clean-room,
> Apache-2.0 rebuild of what Sunshine does.

## Why Comet

Starfire proved the client side is already at hardware speed: **1.1 ms decode,
0 drops, 60 fps the moment the host can supply it**. Our own benchmarks then
found the ceiling isn't the protocol or the client — it's the **host's software
encoder** (48 fps cap, ~1 s encode latency, because the test host has no usable
hardware encoder). See the knob sweep in the bench tooling.

That reframes the opportunity. We don't need a faster client; we need to **own
the other end**. Comet is that: a Rust streaming host that

1. uses **hardware encoders** (NVENC / QuickSync / AMF / VideoToolbox) so the
   encode bottleneck disappears,
2. is **Apache-2.0 top to bottom** (Sunshine is GPLv3 — viral copyleft; a clean
   host makes the *entire* Comet⇄Starfire stack permissive), and
3. lets us **co-design both ends** — a native low-latency mode we can't build
   against a host we don't control.

Comet does **not** make the Starfire client faster by itself. Its value is
removing the host bottleneck and unlocking transport/codec/pacing co-design.

## Guiding principles

- **Hardware-first.** Capture and encode stay on the GPU; the frame should never
  touch the CPU between desktop-duplication and the encoder. Software encode is a
  fallback, never the default.
- **Pluggable everything.** The point of Comet is to *play with* transports,
  codecs, and FEC. Each is a trait with swappable backends so the bench harness
  can A/B them head-to-head on the same link (see [Transport trait](#the-key-idea-a-pluggable-transport-seam)).
- **Keep the interop oracle.** Real Sunshine and real Moonlight remain our
  correctness ground truth. Comet ships a **GameStream-compat mode** so existing
  Moonlight clients work with it, *and* a **Comet-native mode** for Starfire.
  Never test only Starfire-against-Comet — a bug in shared protocol code passes
  both ends silently.
- **Clean-room, carefully.** Reading GPLv3 Sunshine source to interop a *client*
  is one thing; reading it to build a *competing host* is riskier (derivative-work
  territory). Comet's host code must be clean-roomed from the **protocol** (wire
  observation + the SDP/RTSP facts we already documented), not from Sunshine's
  code structure. Resolve this before host code is written with their source open.
  See [`clean-room-policy.md`](clean-room-policy.md).

## The transport question (the fun part)

Today GameStream is a pile of separate channels: **RTSP over TCP** (handshake) +
**ENet reliable-UDP** (control/input) + **raw RTP/UDP** (video, with our Cauchy
FEC) + **UDP** (audio). Four mechanisms, a bespoke pairing/mTLS ladder, and
encryption we currently negotiate *off* on the LAN. It works, but it's a museum.

### Idea on the table: iroh / QUIC over LAN

[iroh](https://iroh.computer) is a Rust P2P library over **QUIC** (Quinn under
the hood). Adopting it would collapse the whole channel zoo into **one QUIC
connection** carrying video, audio, control, and input as multiplexed streams +
datagrams. What it buys us:

- **Encryption for free, always on.** QUIC = TLS 1.3. Our entire "encryption is a
  gap" problem evaporates — and AES-NI makes it ~free on the CPU. Measure, but
  expect negligible.
- **Identity replaces the PKI.** iroh addresses *are* Ed25519 public keys. The
  bespoke mTLS + PIN pairing ladder we reverse-engineered could become a pubkey
  handshake + a short PIN/SAS confirmation. Much simpler, modern.
- **Connection migration.** Roam Wi-Fi → Ethernet mid-stream without a reconnect.
  GameStream can't.
- **NAT traversal + relay fallback** (the WAN story, free). Moonlight needs manual
  port-forwarding; iroh hole-punches and falls back to a relay automatically.
  For LAN we force the **direct** path; relay is a future WAN bonus.
- **One handshake, one congestion domain, one socket.** Far less moving state than
  RTSP+ENet+two RTP flows.

**The critical nuance — datagrams, not streams.** Video must ride QUIC's
**unreliable datagram** extension (RFC 9221, supported by Quinn), *not* reliable
streams. Reliable ordered streams head-of-line-block: one lost packet stalls every
later frame — fatal for live video. Use datagrams for video/audio (drop late data,
keep our FEC on top), reliable streams only for control/input/handshake. Get this
wrong and QUIC is *worse* than our raw UDP.

**Things to test before trusting it:**
- Datagram max size vs path MTU (no QUIC fragmentation — same 1392-ish constraint
  we already handle).
- **Tame or disable congestion control** for a fixed-bitrate stream — Quinn's
  loss-based CC will otherwise fight a constant 20 Mbps feed.
- iroh's relay must not steal the LAN path — verify it picks the direct route and
  measure setup latency vs our ENet connect.
- Per-packet userspace-QUIC overhead at 60 fps / 20 Mbps (should be noise on LAN).

### The full options menu (things to play with)

Transport is one layer of several. Each row is a knob we can swap behind a trait
and benchmark:

**Transport**
- **iroh / QUIC datagrams** — encryption + identity + migration + NAT, batteries
  included. (The proposal above.)
- **Raw Quinn (QUIC) without iroh** — same QUIC benefits, but we keep our own
  identity/discovery and skip iroh's relay opinion. Lighter, more control.
- **GameStream (RTSP + ENet + RTP)** — the compat baseline; what Moonlight speaks.
- **WebRTC** (`str0m` sans-IO, or `webrtc-rs`) — the industry standard for
  low-latency media (Stadia, GFN-in-browser, Parsec-class). Brings SRTP, GCC
  congestion control, NACK/PLI/FEC, *and browser clients*. Heavy, but proven for
  exactly this workload.
- **Media-over-QUIC (MoQ)** — emerging IETF standard for live media over QUIC with
  relays; Rust impls exist. Forward-looking, fast-moving.
- **Plain UDP + Noise** — keep full control of the wire, add the
  [Noise Protocol](https://noiseprotocol.org) (`snow` crate, WireGuard's handshake)
  for crypto. Essentially "GameStream done right."

**Security / pairing**
- iroh Ed25519 node IDs · Noise (`snow`) · TLS 1.3 (`rustls`) · GameStream mTLS+PIN
  (compat). UX: PIN, **QR code**, or **SAS** (numeric short-auth-string compare),
  with keys in the OS keychain.

**Video encode (the actual bottleneck)**
- Hardware: **NVENC** (NVIDIA), **AMF** (AMD), **QuickSync / oneVPL** (Intel),
  **VideoToolbox-encode** (macOS — symmetric to our decoder), **Media Foundation**
  (generic Windows).
- Codecs: H.264 (compat) · HEVC (current) · **AV1** (40-series NVENC / RDNA3 —
  better bandwidth, the future) · VP9.
- Pure-Rust (`rav1e`) is CPU-only → too slow for realtime 1080p60; HW FFI is the
  path. Wrap them all behind one `Encoder` trait.

**Capture (keep it zero-copy)**
- Windows: DXGI Desktop Duplication or Windows.Graphics.Capture → GPU texture
  straight into the encoder. macOS: ScreenCaptureKit. Linux: PipeWire / KMS.
  The latency win is **never copying the frame to the CPU** between capture and
  encode.

**Loss recovery / FEC**
- Keep our **Cauchy RS** (byte-exact, already built) · **adaptive FEC** (scale %
  to measured loss — the thing the static 50% doesn't do) · **NACK/PLI feedback**
  (request resend or IDR on loss; lower overhead on clean links) · **RaptorQ**
  fountain codes (`raptorq`). Our sweep showed static 50% FEC is likely wasteful
  on a clean LAN — this layer is ripe.

**Congestion / bitrate adaptation**
- Static (current) · **GCC** (WebRTC's delay+loss) · SCReAM / NADA · QUIC's BBR
  repurposed · **custom**: feed back `frame_processing_latency` + RTT + loss and
  let the host ramp bitrate. We already surface host encode latency on every frame
  — the signal is sitting there unused.

**Pacing / sync**
- Client frame-pacing to display refresh · host capture→encode pipelining ·
  clock-sync for true **glass-to-glass** latency measurement · present-mode
  (immediate vs triple-buffer) tuning.

## The key idea: a pluggable transport seam

To actually "play with" these without rewrites, the architecture is a **trait**
the rest of the stack is written against:

```rust
/// One Comet session's wire, regardless of mechanism. Video/audio are lossy
/// (late data is dropped); control/input are reliable. Implementations:
/// GameStreamTransport, QuicTransport (iroh/Quinn), WebRtcTransport, …
trait Transport {
    fn send_video(&mut self, frame: &EncodedFrame) -> Result<()>;   // lossy
    fn send_audio(&mut self, pkt: &[u8]) -> Result<()>;             // lossy
    fn send_control(&mut self, msg: &[u8]) -> Result<()>;          // reliable
    fn poll_input(&mut self) -> Option<InputEvent>;                 // reliable
    fn stats(&self) -> LinkStats;                                   // rtt, loss, cwnd
}
```

With this seam, the **bench harness we already built** becomes a transport
A/B lab: run the same captured/encoded content over GameStream vs iroh-QUIC vs
WebRTC on the same link, and read FPS / bitrate / loss / RTT side-by-side — exactly
how we compared Starfire vs Moonlight. That's the experiment platform.

## Proposed crate layout

```
comet-protocol   shared wire types, FEC, pairing, input/control codecs   (shared with Starfire)
comet-transport  the Transport trait + backends (gamestream, quic, webrtc)
comet-encode     the Encoder trait + backends (nvenc, amf, qsv, videotoolbox)
comet-capture    desktop capture per-OS (dxgi/wgc, screencapturekit, pipewire)
comet-host       the host binary: capture → encode → transport → input-inject
comet (CLI)      `comet host` / `comet bench` / `comet pair`  (unify with `starfire`)
```

Note `comet-protocol` is largely the *same code* as `starfire-core`'s wire layer —
the FEC, RTSP, control, and input encoders are direction-agnostic. Extracting a
shared `*-protocol` crate (proposed independently for Starfire) is the natural
first step and serves both.

## Risks & open questions

- **Host is the harder half.** Capture, hardware-encode FFI per vendor, and
  **input injection** (Windows needs `SendInput` + a signed virtual-gamepad driver)
  are most of why Sunshine is a big project. Scope soberly.
- **Don't lose the oracle.** Keep validating against real Sunshine/Moonlight even
  after Comet exists.
- **Compat tax.** A Comet-native QUIC mode is *additive*; GameStream-compat stays
  for Moonlight. We don't get to freely redesign the protocol for everyone.
- **Licensing clean-room** (see principles) — settle before writing host code.
- **QUIC realtime tuning** — congestion control vs fixed-bitrate is the make-or-break
  detail for the iroh idea.

## Suggested first experiments (cheap → decisive)

1. **Extract `comet-protocol` / `starfire-protocol`** from `starfire-core`. Pure
   refactor, low risk, unblocks everything.
2. **Stand up the `Transport` trait** with the existing GameStream path as backend
   #1 (proves the seam without new wire code).
3. **iroh/QUIC spike**: a throwaway Comet that captures *nothing* — just blasts our
   already-encoded fixture frames over iroh QUIC datagrams to a Starfire that
   speaks the QUIC transport. Measure FPS/RTT/loss vs GameStream on the same LAN.
   This tests the single riskiest assumption (QUIC-datagrams-for-video) for almost
   no code.
4. **Minimal hardware-encode host**: capture + *one* encoder (VideoToolbox on the
   Mac is easiest, symmetric to our decoder) → existing GameStream transport. This
   finally measures Starfire's true ceiling against a non-software host.

Steps 3 and 4 are independent and each answers one big question (is QUIC viable?
is the client actually faster than we've been able to show?). Run whichever is
more interesting first.
