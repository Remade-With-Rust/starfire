# Protocol 02 — Pairing & Crypto

> Provenance: observation against Sunshine vX.Y. Clean-room. The byte layouts and
> hash chain here are **[CAPTURE-LOCKED]**; the authoritative form is the
> committed fixtures + known-answer crypto vectors
> ([`../03-bitexact-methodology.md`](../03-bitexact-methodology.md)).

This doc owns **all client crypto**: pairing identity, the PIN challenge, and the
session-key material that downstream control/input/media planes consume.

## 1. Client identity

- Generate a **P-256 self-signed certificate** via `rcgen` (Apache/MIT) once per
  client install; persist it (per-host, see §01) encrypted at rest.
- This cert is the durable client identity added to the host's trusted set during
  pairing, and used for mTLS thereafter.

## 2. The `/pair` ladder

The pairing flow is a sequence of HTTP requests (over 47989, escalating to mTLS
on 47984) that interleave a PIN-based challenge with certificate exchange. The
logical ladder:

```
1. getservercert      → client sends its cert + a salt; host returns its cert.
2. (PIN known)          PIN is generated client-side (auto-PIN, §4), not typed.
3. clientchallenge    → client encrypts a challenge with the PIN-derived key.
4. serverchallengeresp→ host proves it knows the PIN; returns its challenge.
5. clientpairingsecret→ client returns the paired secret; hashes are verified.
6. (success)            client cert is now in the host's trusted set.
```

**[CAPTURE-LOCKED]** for each step: exact request path + query params, exact
field order in the encrypted blobs, exact hash inputs, and the success/failure
signaling. Freeze the full ladder from one captured pairing run.

## 3. The PIN key-derivation & cipher

- **KDF:** the AES key for the challenge is derived as
  `SHA-256(salt ‖ pin)` and used as **AES-128** (i.e. the first 16 bytes).
  **[CAPTURE-LOCKED]**: confirm truncation vs full-width and the exact byte order
  of `salt ‖ pin`.
- **Cipher:** **AES-128 in ECB mode** for the challenge blocks. ECB is unusual and
  a footgun — match it exactly; do not "improve" it to CBC/GCM. It is what the
  host expects.
- **Hash chain:** the challenge/response chains SHA-256 over
  (challenge ‖ cert ‖ secret) elements. **[CAPTURE-LOCKED]**: the precise
  concatenation order is the whole game; derive it from a fixture where every
  input is known and assert the resulting hashes match.

### Crypto known-answer vectors
Because ciphertext varies only with known inputs here (salt + PIN are captured),
the golden test re-derives the key from the fixture's salt+PIN, re-encrypts, and
asserts the ciphertext equals the captured ciphertext **byte-for-byte**. That is
how we prove the KDF + ECB framing are correct without reading any GPL source.

## 4. Auto-PIN (no human types a PIN)

- The client **generates** the PIN itself and submits it to the host out-of-band
  via Sunshine's admin API (`POST /api/pin` or equivalent) so no user interaction
  is needed. **[CAPTURE-LOCKED]**: exact admin endpoint, auth, and payload.
- This makes pairing a one-click/zero-click step in the integrated UX.

## 5. Pre-provisioning (zero runtime pairing)

- Alternative to runtime pairing: **inject the client cert into the host's trust
  store at VM/host setup time**, so the first connection is already mutually
  trusted over mTLS and the entire `/pair` ladder is skipped.
- Where supported, this is the preferred path (no PIN dance, no race). The runtime
  pairing ladder remains as fallback and for hosts we don't provision.
- **[CAPTURE-LOCKED]**: the trust-store location/format Sunshine reads, so the
  injected cert is accepted identically to a runtime-paired one.

## 6. Session key material (RI key / IV)

- Pairing establishes **identity**. The **session keys** that protect the control,
  input, and (where applicable) media planes — the "RI key" and IV — are
  established at **launch** and surfaced through **RTSP** (§04, §05), not here.
- This doc is the canonical place that *names* them; their lifecycle:
  `/launch` carries/derives them → RTSP `SETUP`/`ANNOUNCE` confirm per-stream
  material → control + input use them for AES-GCM. **[CAPTURE-LOCKED]**: exact
  derivation and where each appears on the wire.

## State machine

```
have identity? ──no──► generate P-256 cert (rcgen), persist
      │yes
      ▼
pre-provisioned? ──yes──► connect mTLS (skip ladder) ──► §03
      │no
      ▼
run /pair ladder (auto-PIN) ──► cert trusted ──► persist host cert ──► §03
```

## Tests

- **Vectors:** KDF + AES-128-ECB known-answer from a captured pairing.
- **Fixture:** the full `/pair` ladder request/response bodies.
- **Golden:** re-derive key, re-encrypt challenge, assert == captured ciphertext;
  re-compute the hash chain, assert == captured hashes.
- **Live:** complete a real pairing against the gaming-VM host; dated note here.

## Open / to-confirm

- [ ] Exact salt length and `salt ‖ pin` byte order. **[CAPTURE-LOCKED]**
- [ ] Hash-chain concatenation order at each step. **[CAPTURE-LOCKED]**
- [ ] Auto-PIN admin endpoint + auth. **[CAPTURE-LOCKED]**
- [ ] Pre-provisioning trust-store format. **[CAPTURE-LOCKED]**
- [ ] RI key/IV derivation and on-wire location. **[CAPTURE-LOCKED]**
