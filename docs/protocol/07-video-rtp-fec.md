# Protocol 07 — Video Ingest, FEC & Reassembly

> Provenance: observation against Sunshine vX.Y. Clean-room. **This is the long
> pole.** The Reed-Solomon geometry and RTP framing must match Sunshine
> **bit-for-bit** or recovery silently corrupts frames. Everything here is
> **[CAPTURE-LOCKED]** and gets the most capture budget
> ([`../03-bitexact-methodology.md`](../03-bitexact-methodology.md) §FEC).

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

## 1. RTP framing — [CAPTURE-LOCKED]

- Standard-ish RTP header (version, payload type, sequence, timestamp, SSRC) plus
  a **Sunshine-specific** payload header carrying frame index, fragment index /
  count, and FEC metadata. Exact fields/offsets come from the capture.
- Fragmentation: one frame spans many RTP packets; the payload header tells us how
  to stitch fragments back in order.

## 2. Reed-Solomon FEC — the bit-exact core

- Crate: **`reed-solomon-erasure`** (MIT/Apache).
- A FEC **block** = `k` data shards + `m` parity shards; recover any up to `m`
  losses. The block geometry (shard size, `k`, `m`, how shards map onto RTP
  packets, and the **generator matrix / field convention**) must match Sunshine
  exactly. **[CAPTURE-LOCKED]** — a mismatch produces plausible-but-wrong recovered
  bytes that corrupt the decoder silently.
- Methodology (from §03):
  1. Capture a **lossless** session → learn the framing + shard layout.
  2. In test, **inject deterministic loss** (drop known shards).
  3. Run RS recovery → assert recovered shards `==` the pre-loss originals,
     byte-for-byte.
- Confirm the matrix convention matches `reed-solomon-erasure`'s (it must, or we
  reconstruct different bytes). If conventions differ, that's the bug to find
  here, not in production.

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
