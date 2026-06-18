# 03 — Bit-Exact Methodology (the core method)

> **Read this before any protocol doc.** "Bit-for-bit identical to Sunshine
> integration" is a *process*, not a paragraph of byte offsets. This doc defines
> that process. Every `[CAPTURE-LOCKED]` marker elsewhere in the corpus points
> back here.

## Why we can't just write the spec down

There is no authoritative public specification of the GameStream wire protocol,
and the one mature reference implementation (Moonlight) is GPLv3 — off-limits
under our clean-room rule. Therefore:

- We do **not** trust prose (including these docs) for byte-exact fields.
- We **do** trust a committed, verbatim capture of a real Sunshine exchange.
- Our code is correct **by definition** when it round-trips that capture.

This inverts the usual flow: the *test fixture is the spec*, and the prose docs
are a map to help humans navigate the fixtures.

## The loop

```
   ┌─────────────┐   ┌────────────┐   ┌───────────────┐   ┌───────────────┐
   │ 1. CAPTURE  │ → │ 2. FREEZE  │ → │ 3. GOLDEN TEST│ → │ 4. LIVE-VALID │
   │ live traffic│   │ as fixture │   │ round-trip eq │   │ against host  │
   └─────────────┘   └────────────┘   └───────────────┘   └───────────────┘
          ▲                                                        │
          └──────────────── re-capture on Sunshine upgrade ◄───────┘
```

### 1. Capture
- Run a **reference streaming session** against the live Sunshine host (using any
  lawful client to drive the host — we observe the *host's* bytes, we do not read
  the client's source).
- `tcpdump` / `pcap` on the host-facing interface (e.g. `virbr0` for the gaming
  VM). Capture the full session: mDNS, HTTP(S), RTSP/TCP (plaintext on 48010),
  and the RTP/ENet UDP streams.
- Record the **Sunshine version**, negotiated config (codec/res/fps/bitdepth),
  and host hardware alongside the capture. Captures are version-stamped.

### 2. Freeze as a fixture
- Store the raw bytes for each layer under `tests/fixtures/<layer>/<case>.bin`
  (or `.pcap` for whole-session captures), plus a sidecar `.meta.toml`:
  ```toml
  sunshine_version = "0.23.1"
  captured        = "2026-06-18"
  layer           = "pairing/getservercert"
  codec           = "AV1-Main8"
  notes           = "host=gaming-vm, res=3840x2160@120, hdr=on"
  ```
- Fixtures are **committed to the repo** and are the authoritative spec. They are
  small, redacted of secrets where needed (see Security below), and never
  regenerated casually.

### 3. Golden test
- For each layer, a test that proves our encoder/decoder matches the fixture
  **byte-for-byte**:
  - **Decode direction:** parse the fixture, assert the parsed struct equals the
    expected logical value.
  - **Encode direction:** build the same logical value, serialize, assert the
    output bytes `==` the fixture bytes. No "close enough"; exact equality.
  - **Round-trip:** `decode(encode(x)) == x` and `encode(decode(bytes)) == bytes`.
- A drift in our bytes turns the test red. This is the mechanism that *guarantees*
  bit-for-bit fidelity instead of hoping for it.

### 4. Live-validation
- Beyond fixtures, each layer must complete a real exchange with a running
  Sunshine host and the result is recorded in a dated **live-validation note** in
  the layer's doc. Mocks prove we match the *capture*; live proves the capture
  was representative.

## Crypto is special

Encrypted layers (AES-128 ECB pairing challenge, AES-GCM control/input/video) can
*not* be matched by raw byte equality of ciphertext alone, because nonces/IVs and
session keys differ per run. For these:

- Capture and freeze the **plaintext** structure plus the **key-derivation
  inputs** (salts, PINs, RI key/IV from RTSP) so the golden test can re-derive the
  same key and reproduce the exact ciphertext deterministically.
- Test the **KDF and cipher framing** with known-answer vectors derived from a
  captured session where all inputs are known.
- See [`protocol/02-pairing-and-crypto.md`](protocol/02-pairing-and-crypto.md).

## FEC is the long pole

Reed-Solomon FEC geometry (block sizes, shard counts, the generator matrix, and
the RTP framing around it) must match Sunshine **bit-for-bit** or recovery
silently corrupts frames. This gets the most capture budget:

- Capture lossless sessions to learn the *framing*, then **inject deterministic
  loss** in test to prove our recovery reconstructs the exact original shards.
- The golden test reconstructs known-dropped shards and asserts the recovered
  bytes equal the pre-loss fixture. See
  [`protocol/07-video-rtp-fec.md`](protocol/07-video-rtp-fec.md).

## Security & hygiene for fixtures

- **Never commit real long-lived secrets.** Pairing fixtures use a throwaway
  client identity and a throwaway host; redact or synthesize any field that is a
  durable credential. Document what was redacted in the `.meta.toml`.
- Fixtures are **inputs to tests**, never executed.
- Re-capture and re-stamp when the Sunshine version bumps; a green test against a
  stale fixture is a false sense of security if the host changed.

## What this buys us

- A new engineer can implement a layer purely from its fixture + golden test and
  be confident it's correct without ever touching Sunshine or Moonlight.
- Sunshine upgrades become a capture-diff exercise, not an archaeology dig.
- "Bit-for-bit identical" is a property the CI *enforces*, not a claim we make.
