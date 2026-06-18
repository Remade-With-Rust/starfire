# 06 — Testing & Validation Strategy

> The test strategy *is* the spec strategy. See
> [`03-bitexact-methodology.md`](03-bitexact-methodology.md) for the capture loop;
> this doc covers the concrete test types every layer ships.

## Per-layer requirement (the bar for "done")

Every protocol layer lands with **all four**:

1. **Unit tests** — wire encode/decode **round-trips** (`decode(encode(x)) == x`
   and `encode(decode(bytes)) == bytes`).
2. **Captured-from-live fixture** — verbatim bytes under
   `tests/fixtures/<layer>/`, version-stamped (`.meta.toml`).
3. **Golden test** — our bytes `==` the fixture bytes, exactly.
4. **Live-validation note** — dated record of a successful real exchange.

A layer is not "done" until it passes its golden test **and** meets its
[performance budget](07-performance-budgets.md).

## Capture harness

- `tcpdump` / `pcap` on the host-facing interface (e.g. `virbr0` for the gaming
  VM). RTSP/TCP 48010 is plaintext — easy ground truth; RTP/ENet UDP captured raw.
- A small tool to slice a session pcap into per-layer fixtures + `.meta.toml`.
- Captures are version-stamped to the Sunshine release and re-taken on host upgrade
  (a green test against a stale fixture is a false positive).
- Fixture hygiene: **no durable secrets** committed; throwaway identity/host;
  redactions documented. Fixtures are inputs to tests, never executed.

## Loss injection (FEC + reassembly)

- Deterministic packet-loss/reorder harness over the §07 reassembler + FEC.
- Drop every combination up to `m` shards → assert exact reconstruction.
- Assert IDR is requested **exactly** when a frame is unrecoverable, and a corrupt
  AU is **never** emitted.

## Crypto vectors

- Known-answer vectors for: PIN KDF + AES-128-ECB (pairing, §02), AES-GCM framing
  (control §06, input §09). Re-derive keys from captured inputs, reproduce
  ciphertext byte-for-byte.

## Fuzzing

- Fuzz the **depacketizer** and **FEC decoder** (and RTSP/SDP parser) against
  malformed input. Target: **zero panics**, correct drop/abort behavior.

## Soak

- Multi-hour live session; watch for memory leak, clock/A-V drift, FEC degradation,
  reconnect correctness.

## Build-target discipline

- If the workspace defaults to a wasm target, protocol tests run on the host
  target explicitly, e.g.:
  ```
  cargo test -p starfire-core --target aarch64-apple-darwin
  cargo test -p starfire-core --target x86_64-pc-windows-msvc
  ```

## CI gates

- `cargo test` (host targets) — unit + golden + loss-injection.
- `cargo deny` — **license gate** (no GPL/LGPL anywhere; §08).
- Clean-room provenance-header lint on every `protocol/` module.
- (Phase 3) fuzz smoke + soak job.
