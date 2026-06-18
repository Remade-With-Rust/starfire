# Protocol 05 — RTSP Stream Setup

> Provenance: observation against Sunshine vX.Y. Clean-room. RTSP on **TCP 48010**
> is plaintext in capture — the easiest layer to freeze verbatim. Method order,
> header names, and SDP grammar are **[CAPTURE-LOCKED]** to fixtures.

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
