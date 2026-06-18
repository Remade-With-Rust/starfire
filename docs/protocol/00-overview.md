# Protocol 00 — Overview

> Provenance: derived from protocol observation against Sunshine (version pinned
> per capture). Clean-room — no Moonlight source consulted. Read
> [`../03-bitexact-methodology.md`](../03-bitexact-methodology.md) first; every
> `[CAPTURE-LOCKED]` field below resolves to a committed fixture, not this prose.

## The protocol on one page

Starfire speaks the Sunshine GameStream protocol. A session moves through phases,
each on its own transport:

```
PHASE                     TRANSPORT                       DOC
─────────────────────────────────────────────────────────────────────────────
1. Discovery              mDNS UDP 5353 / HTTP 47989      01-discovery.md
2. Pairing (or mTLS)      HTTPS 47984 / HTTP 47989        02-pairing-and-crypto.md
3. Server capabilities    HTTPS 47984  (/serverinfo XML)  03-serverinfo-and-negotiation.md
4. App list & launch      HTTPS 47984  (/applist,/launch) 04-applist-and-launch.md
5. RTSP stream setup      RTSP/TCP 48010                  05-rtsp.md
6. Control channel        ENet/UDP 47999                  06-control-enet.md
7. Video ingest           RTP/UDP 47998                   07-video-rtp-fec.md
8. Audio ingest           RTP/UDP 48000                   08-audio-rtp.md
9. Input                  over the control channel        09-input.md
```

## Ports — [CAPTURE-LOCKED]

These are the conventional Sunshine defaults. The **authoritative** per-stream
ports come from the RTSP `SETUP` exchange (§05) and the live capture, because the
host can be configured with a different base port and the offsets are negotiated.

| Port | Proto | Use |
|------|-------|-----|
| 5353 | UDP | mDNS discovery (`_nvstream._tcp`) |
| 47984 | TCP | HTTPS control (paired, mTLS) |
| 47989 | TCP | HTTP control (unpaired / `/serverinfo` probe) |
| 48010 | TCP | RTSP session setup |
| 47998 | UDP | RTP **video** |
| 47999 | UDP | **Control** (ENet) |
| 48000 | UDP | RTP **audio** |

> Base port is conventionally 47989/47984; UDP media ports are conventionally
> base+9/+10/+11. **Do not hardcode** — read them from `SETUP`. Confirm the exact
> mapping against a capture before committing constants.

## Phase sequencing rules

- **Pairing is one-time per client identity.** Once paired (or pre-provisioned),
  reconnects skip straight to mTLS + `/serverinfo`.
- **`/serverinfo` is queried twice in spirit:** unauthenticated over HTTP to probe
  reachability/pair-status, and authenticated over HTTPS to read full caps.
- **Crypto material for the media/control planes** (RI key/IV) is established at
  **launch + RTSP**, not at pairing. Pairing establishes *identity*; launch
  establishes *session keys*. See §02 and §05.
- **Control must be up before media is useful** — IDR requests and keepalive ride
  the control channel; bring ENet up right after `PLAY`.

## What "bit-for-bit identical" means per phase

| Phase | What must match exactly | How proven |
|-------|------------------------|------------|
| Pairing | Challenge/response byte layout, AES-128 ECB framing, cert encoding, the hash chain | KDF/cipher known-answer vectors + plaintext fixtures |
| `/serverinfo` | XML element names, attribute semantics, flag bit positions | Verbatim XML fixture + parser golden test |
| Launch | Query param names, value encodings, key/IV hex formatting | Request-line fixture |
| RTSP | Method order, header names, SDP body grammar, port/crypto extraction | Verbatim RTSP+SDP fixtures |
| Control | ENet channel setup, AES-GCM framing, message type IDs + payloads | Plaintext message fixtures + cipher vectors |
| Video RTP+FEC | RTP header fields, fragment framing, **RS shard geometry + matrix**, AU assembly | Lossless framing fixture + deterministic loss-injection golden test |
| Audio RTP | RTP framing, FEC, Opus packet boundaries | Framing fixture + Opus decode check |
| Input | Per-device packet structs, scaling math, AES-GCM framing | Plaintext struct fixtures + cipher vectors |

## Codec negotiation (cross-cutting)

- AV1 is primary; HEVC and H.264 are fallback. Bit depth 8 and 10 (HDR).
- `/serverinfo`'s `ServerCodecModeSupport` advertises support; **AV1 = `0x40000`**.
- The negotiated codec/res/fps/bitdepth/HDR is carried into `/launch` and the
  RTSP `ANNOUNCE`. The decoder is selected at runtime to match. See §03, §04, §05.

## Crypto map (cross-cutting) — see §02 for detail

| Where | Primitive | Key source |
|-------|-----------|-----------|
| Pairing PIN challenge | AES-128 **ECB** | `SHA-256(salt ‖ pin)` truncated to 128-bit |
| Identity / transport | mTLS (P-256 self-signed via `rcgen`) | pairing-exchanged certs |
| Control + input | **AES-GCM** | RI key/IV from launch/RTSP |
| Video / audio payload | AES-GCM (where Sunshine encrypts media) — **[CAPTURE-LOCKED]** | RI key/IV |

> Whether and how media payloads are encrypted is **[CAPTURE-LOCKED]** — confirm
> from capture which streams are AES-GCM protected and with what nonce
> construction before implementing the depacketizer.
