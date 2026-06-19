# Protocol 07 — Video Ingest, FEC & Reassembly

> Provenance: live capture against **Sunshine 2026.516.143833** + the Sunshine
> *server* sender/FEC source (owner-approved; never the moonlight-common-c
> client). Clean-room w.r.t. the client.

## ✅ Status — ingest + FEC LIVE & byte-exact (2026-06-19)
`starfire_core::video` ingests the real plaintext-HEVC stream end to end:
depacketize → reassemble, with **`nanors`-compatible Reed-Solomon recovery** on
loss. Validated against a captured fixture (`tests/fixtures/video/stream-hevc.fix`):
169 frames reassembled; a dropped data shard recovers to Sunshine's **exact
bytes**; the max (`m`) simultaneous drops recover byte-for-byte; the Depacketizer
heals a lossy frame to bytes identical to the lossless run. The former
"long pole" / silent-corruption risk is **retired**.

## Goal

Receive RTP video on UDP 47998 → depacketize → Reed-Solomon FEC recovery →
reorder by sequence → reassemble complete frames → emit codec access units (AV1
OBUs / HEVC or H.264 NAL units) to the decoder — **never feeding a corrupt frame**.

## Pipeline

```
UDP RX ─► RTP parse ─► group into FEC blocks ─► RS recover (if loss) ─►
reorder by seq ─► reassemble frame ─► emit AU ─► decoder (§04 platform)
   │                                                       │
   └──── unrecoverable? ──► request IDR over control (§06) ◄┘
```

## 1. RTP framing — RESOLVED (`video::rtp`)

`video_packet_raw_t` = RTP(12) + `reserved`(4) + NV_VIDEO_PACKET(16), then the
payload. Wire-confirmed offsets (the RTP sequence number is the ground truth):

| Field | Offset | Notes |
|------|--------|-------|
| RTP header byte | 0 | `0x90` = v2 + extension |
| RTP sequence (BE u16) | 2 | |
| `streamPacketIndex` (LE u32) | 16 | `== rtp_seq << 8` |
| `frameIndex` (LE u32) | 20 | monotonic; constant within a frame |
| `flags` (u8) | 24 | `0x01` PIC_DATA, `0x02` EOF, `0x04` SOF |
| `fecInfo` (LE u32) | 28 | `shardIdx=(>>12)&0x3ff`, `dataShards=(>>22)&0x3ff`, `pct=(>>4)&0xff` |
| payload | 32 | SOF packets prefix an 8-byte `video_short_frame_header` (`frameType@+3`: 2=IDR) |

One frame spans many packets (one per shard); the FEC **shard** is exactly
`bytes[32..]` (all padded to a fixed blocksize, 1376 B here).

## 2. Reed-Solomon FEC — RESOLVED, byte-exact (`video::fec`)

- **First-party, pure-Rust, no crate.** Sunshine encodes parity with the `nanors`
  library, whose matrix is a **systematic Cauchy** matrix over GF(2^8) —
  `P[j][i] = 1/((m + i) ⊕ j)` (poly `0x11d`, generator `2`), *not* Vandermonde.
  We proved `reed-solomon-erasure` (Vandermonde) reconstructs plausible-but-wrong
  bytes, then dropped it for a matching Cauchy decoder (Gauss-Jordan inverse).
- A FEC **block** = `k` data shards `0..k-1` then `m` parity `k..k+m-1`, all
  blocksize-padded; `m = ceil(k·pct/100)` (the host re-encodes the final pct, so
  the client derives `m` from `data_shards` and `pct` alone). Recover any ≤ `m`
  losses. Up to 4 independent FEC blocks per frame (multi-block: TODO).
- Acceptance (golden) tests live in `video.rs::fixture_tests`: deterministic loss
  injection → recovered shards `==` the real pre-loss bytes, byte-for-byte.

## 3. Reassembly & loss handling

- Reorder fragments by sequence within a frame; assemble the full coded frame.
- If a frame is **unrecoverable** (more than `m` losses in a block): **drop to the
  next keyframe** and **request IDR** over control (§06). Never emit a partial or
  guessed frame.
- Detect frame boundaries from the payload header; emit exactly one AU per frame.

## 4. Codec access-unit emission

- **AV1:** emit OBUs in the form the decoder expects (temporal-unit framing).
- **HEVC/H.264:** emit NAL units in Annex-B or length-prefixed form per the
  decoder's needs (§04). **[CAPTURE-LOCKED]**: any start-code vs length-prefix
  expectation per platform decoder.

## 5. Stats

- Per-frame receive time, FEC recovery rate, decode-queue depth → stats overlay +
  ABR feedback (§06, [`../07-performance-budgets.md`](../07-performance-budgets.md)).

## Hot-path rules

- Bounded ring buffers; **no per-packet allocation** in steady state.
- **No `unwrap`/`panic`** on any packet — loss/reorder/malformed are normal.
- Zero-copy from reassembly buffer into the decoder submission where possible.

## Tests

- **Fixture (framing):** verbatim lossless RTP video capture for one GOP.
- **Fixture (FEC):** a block's data+parity shards with a known original.
- **Golden (FEC):** drop each combination up to `m` → assert exact reconstruction.
- **Loss injection:** deterministic drop/reorder harness over the reassembler →
  assert IDR requested exactly when unrecoverable, never a corrupt AU emitted.
- **Fuzz:** malformed RTP/headers → assert no panic.
- **Live:** stream a GOP from the real host, decode it; dated note.

## Open / to-confirm — the highest-risk list in the project

- [ ] RTP + Sunshine payload header exact layout. **[CAPTURE-LOCKED]**
- [ ] RS `k`/`m`, shard size, packet↔shard mapping. **[CAPTURE-LOCKED]**
- [ ] RS field/matrix convention vs `reed-solomon-erasure`. **[CAPTURE-LOCKED]**
- [ ] Whether video payload is additionally AES-GCM encrypted. **[CAPTURE-LOCKED]**
- [ ] Per-codec AU framing the decoder expects. **[CAPTURE-LOCKED]**
