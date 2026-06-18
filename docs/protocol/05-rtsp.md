# Protocol 05 — RTSP Stream Setup

> Provenance: observation against **Sunshine 2026.516.143833**. Clean-room. RTSP
> on **TCP 48010** is plaintext. Validated by a live handshake returning the
> stream binding.

## ✅ Status — RTSP handshake works live (F5)
`starfire_core::rtsp::{RtspClient, RtspSession}` walks the exchange and returns
everything the media planes need. Confirmed by `live_rtsp_handshake` + golden test.

### Live-validation note
- **2026-06-18** — `OPTIONS → DESCRIBE → SETUP×3 → PLAY` against local Sunshine
  returned: ports video=47998, audio=48000, control=47999; `Session=DEADBEEFCAFE`;
  `X-SS-Ping-Payload` (8 bytes); `X-SS-Connect-Data`; `PLAY` → 200. SDP fixture:
  `tests/fixtures/rtsp/describe-sdp.bin`.

### Sunshine RTSP quirks (confirmed)
- **One request per TCP connection** — the host closes after each response;
  `RtspClient` opens a fresh socket per request (CSeq still increments globally).
- **No `Content-Length`** on responses — body runs to connection close (read to EOF).
- **`X-GS-ClientVersion`** header required, else the host RSTs the connection.
- **ANNOUNCE is not required** — `PLAY` after SETUP returns 200; the host uses the
  `/launch` config. A minimal ANNOUNCE got a clean 400.

### The handshake
| Step | Request | Yields |
|------|---------|--------|
| OPTIONS | `OPTIONS rtsp://host:48010` | liveness (`CSeq` only) |
| DESCRIBE | + `Accept` | SDP: `x-ss-general.{featureFlags,encryptionSupported,encryptionRequested}`, `sprop-parameter-sets`, `fmtp:97 surround-params` |
| SETUP×3 | `…/streamid={audio,video,control}` + `Transport: unicast;X-GS-ClientPort=…` | `Transport: server_port=<n>`, `Session`, `X-SS-Ping-Payload` (a/v), `X-SS-Connect-Data` (control) |
| PLAY | + `Session` | 200 → host ready to stream |

### ⚠️ Media encryption — key finding for F6/F7
The DESCRIBE SDP advertises **`encryptionRequested:1`** (supported `:5`). The host
requests media encryption (AES-GCM, RI key from `/launch`). The exact bit→stream
mapping + nonce/IV are **[CAPTURE-LOCKED]**, resolved when control/video land.

### Still ahead (with F6/F7)
- The **ping payload** must be sent to each media UDP port to open the return path.
- The **`X-SS-Connect-Data`** seeds the ENet control connect.
- `surround-params` / `sprop-parameter-sets` decode for audio layout + video config.

## Goal

Negotiate the concrete media + control streams: their formats, FEC parameters,
ports, and per-stream crypto, by walking the RTSP exchange.

## The exchange

```
OPTIONS   → capabilities / liveness
DESCRIBE  → host returns SDP: supported formats, FEC params, audio config
SETUP     → per stream (video / audio / control): negotiate transport + ports
ANNOUNCE  → client pushes the negotiated config (codec/res/fps/bitdepth/HDR/…)
PLAY      → start streaming
```

> Sunshine's RTSP is a customized dialect (non-standard methods/headers and an
> `ANNOUNCE` that carries the session config). Treat the captured exchange as
> authoritative grammar, not RFC 2326. **[CAPTURE-LOCKED]** end to end.

## What we extract

From `DESCRIBE`'s **SDP** and the `SETUP` responses:

| Extracted | Used by |
|-----------|---------|
| video format(s) + codec params | §07 depacketizer / decoder |
| audio format + Opus config (rate/channels) | §08 audio |
| **FEC parameters** (RS block geometry hints) | §07 FEC — cross-check vs capture |
| per-stream **ports** (video/audio/control) | binds §06/§07/§08 sockets |
| per-stream **crypto** (RI key/IV confirmation) | §06 control, §09 input AES-GCM |
| RTP payload types / SSRC expectations | RTP parsing |

## What we push (`ANNOUNCE`)

The negotiated config from §03/§04: codec, resolution, FPS, bit depth, HDR,
color space, bitrate, packet size, audio channel layout. **[CAPTURE-LOCKED]**:
exact SDP attribute names and value formats for each.

## Implementation notes

- Parser is a small, strict RTSP/SDP reader — **no `unwrap` on malformed input**;
  a bad response aborts the session cleanly, never panics.
- Sequence numbers (`CSeq`) and session IDs must be tracked exactly as the host
  expects. **[CAPTURE-LOCKED]**.
- This is the layer that *binds everything together*: after `PLAY`, we have ports
  + crypto + FEC params and can bring up control (§06) and start ingest (§07/§08).

## Tests

- **Fixture:** the verbatim RTSP request/response transcript + SDP body.
- **Golden:** parse the transcript → assert extracted ports/crypto/FEC/formats;
  serialize our `ANNOUNCE` → assert == captured `ANNOUNCE` bytes.
- **Live:** complete `OPTIONS..PLAY` against the real host; dated note.

## Open / to-confirm

- [ ] Exact method set + custom headers. **[CAPTURE-LOCKED]**
- [ ] SDP attribute grammar for FEC + crypto + formats. **[CAPTURE-LOCKED]**
- [ ] Port-assignment scheme (from SETUP vs derived). **[CAPTURE-LOCKED]**
- [ ] Session-id / CSeq handling rules. **[CAPTURE-LOCKED]**
