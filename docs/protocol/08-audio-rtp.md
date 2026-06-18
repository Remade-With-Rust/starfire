# Protocol 08 — Audio Ingest

> Provenance: observation against Sunshine vX.Y. Clean-room. RTP framing, FEC, and
> Opus packetization are **[CAPTURE-LOCKED]**.

## Goal

Receive RTP audio on UDP 48000 → FEC recovery → **Opus** decode → map to channel
layout (stereo / 5.1 / 7.1) → output via `cpal`, synced to the video clock.

## Pipeline

```
UDP RX ─► RTP parse ─► FEC recover ─► Opus decode ─► channel map ─►
A/V sync ─► cpal output
```

## 1. RTP audio framing — [CAPTURE-LOCKED]

- RTP header + payload carrying one (or more) Opus packet(s) per RTP packet.
  Exact payload type, timestamp clock (48 kHz for Opus), and any Sunshine-specific
  header come from capture.

## 2. FEC

- Audio also carries FEC (often a lighter RS scheme than video, and/or Opus's own
  in-band FEC). **[CAPTURE-LOCKED]**: which FEC applies and its geometry. Reuse the
  §07 RS engine where the scheme matches; confirm, don't assume.

## 3. Opus decode

- Crate: **`opus`/`audiopus`** (BSD). Decoder configured from the SDP audio config
  (§05): sample rate (48 kHz), channel count, frame size.
- Handle packet loss with Opus PLC (packet-loss concealment) when a packet is
  truly lost and FEC can't recover it.

## 4. Channel layout

- Stereo / 5.1 / 7.1 per negotiated surround capability (§03/§04). Map Opus
  channels to the `cpal` output device's channel order correctly. **[CAPTURE-LOCKED]**:
  the channel ordering convention.

## 5. A/V sync

- Audio is the typical clock master *or* slaves to the video presentation clock —
  pick one model and tune. Maintain a small jitter buffer; correct drift by
  resampling or dropping/duplicating conservatively (no audible glitches).
- Coordinate with render pacing ([`../07-performance-budgets.md`](../07-performance-budgets.md)).

## Hot-path rules

- Bounded buffers; no per-packet alloc in steady state; no `unwrap`/`panic` on
  inbound audio.

## Tests

- **Fixture:** captured RTP audio for a few seconds + the SDP audio config.
- **Golden:** parse framing → assert Opus packet boundaries; decode → assert PCM
  length/channel layout matches expectation.
- **Sync:** simulated jitter → assert buffer keeps A/V within budget.
- **Live:** hear correct, synced audio from the real host; dated note.

## Open / to-confirm

- [ ] RTP audio header + Opus packing. **[CAPTURE-LOCKED]**
- [ ] Audio FEC scheme + geometry. **[CAPTURE-LOCKED]**
- [ ] Channel ordering for 5.1/7.1. **[CAPTURE-LOCKED]**
- [ ] Whether audio payload is AES-GCM encrypted. **[CAPTURE-LOCKED]**
