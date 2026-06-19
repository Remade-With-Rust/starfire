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
- **ANNOUNCE _is_ required** — it **arms the data plane**. (An earlier note here
  said otherwise: PLAY 200s without it, but the host then never streams.) The
  body+headers must be sent in **one `write`** (the host reads the request in a
  single pass; a separate body write lands in a later segment and is missed).

### ANNOUNCE — the mandatory SDP attributes
Sunshine's ANNOUNCE handler `.at()`s a fixed set of SDP attributes and returns
**400** (no logged reason) if *any* is missing. [SOURCE: rtsp.cpp `cmd_announce`]
The complete mandatory set for 2026.516 spans three namespaces; the ones newer
than the classic GameStream SDP (and easy to miss) are:
`x-nv-video[0].clientRefreshRateX100`, `x-nv-video[0].maxNumReferenceFrames`,
`x-ml-general.featureFlags`, `x-ml-video.configuredBitrateKbps`,
`x-ss-general.encryptionEnabled`, `x-ss-video[0].chromaSamplingType`,
`x-ss-video[0].intraRefresh`. Built by `RtspClient::build_announce_sdp`.
- `a=x-ss-general.encryptionEnabled:0` → **plaintext** video/audio/control (valid
  while the host's encryption mode is not MANDATORY for this client).

### The handshake
| Step | Request | Yields |
|------|---------|--------|
| OPTIONS | `OPTIONS rtsp://host:48010` | liveness (`CSeq` only) |
| DESCRIBE | + `Accept` | SDP: `x-ss-general.{featureFlags,encryptionSupported,encryptionRequested}`, `sprop-parameter-sets`, `fmtp:97 surround-params` |
| SETUP×3 | `…/streamid={audio,video,control}` + `Transport: unicast;X-GS-ClientPort=…` | `Transport: server_port=<n>`, `Session`, `X-SS-Ping-Payload` (a/v), `X-SS-Connect-Data` (control) |
| ANNOUNCE | + `Content-type: application/sdp` + the mandatory-attr SDP | **200 arms the session** (400 = a missing attr) |
| PLAY | + `Session` | 200 → host ready to stream |

### Media encryption — resolved
The DESCRIBE advertises `encryptionRequested:1`, but that is the host's *request*,
not a hard requirement when its encryption mode is not MANDATORY. The client
chooses in the ANNOUNCE via `x-ss-general.encryptionEnabled`; Starfire sends `0`
and gets **plaintext** HEVC + control (no AES-GCM). See [06-control-enet.md] for
the data-plane bring-up (control connect → ping → video).

### `X-SS-Ping-Payload` note
The header is the hex of the host's random ping bytes, but Starfire sends the
legacy literal `"PING"` to the media ports (it advertises `featureFlags:0`, so
the host accepts it). Details in [06-control-enet.md].

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
