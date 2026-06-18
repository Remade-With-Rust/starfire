# Starfire — Documentation Plan

> **Starfire** is a clean-room, permissively-licensed **Rust** client for the
> Sunshine GameStream wire protocol. It connects to a Sunshine host and streams
> the session to **Windows and macOS** clients at the highest achievable
> performance and quality. It is a functional replicate of Moonlight's core
> streaming features — built without ever reading Moonlight's GPL source.

This directory is the **planning and specification corpus** for building
Starfire. It is the single source of truth for *what* we build, *how* the wire
protocol works, and *how we prove* our bytes match Sunshine exactly.

---

## 0. The one rule that shapes everything

**There is no clean public spec for this protocol. The live Sunshine server is
ground truth.** We achieve a "bit-for-bit identical" integration not by
transcribing byte layouts from memory or from Moonlight's source, but by a
disciplined loop:

```
capture live Sunshine traffic  →  freeze it as a fixture  →  write a golden
encode/decode test against the fixture  →  validate live against a running host
```

Every protocol doc in here documents *structure and flow* to the level we can
state with confidence, and explicitly marks every byte-exact field as
**[CAPTURE-LOCKED]** — meaning the authoritative value comes from a committed
capture fixture, not prose. See [`03-bitexact-methodology.md`](03-bitexact-methodology.md).
This is the heart of the project. Read it first.

---

## 1. Clean-room provenance (non-negotiable)

- **Never read Moonlight or `moonlight-common-c` GPL source while building
  Starfire.** Not for "just this one struct." A single peek poisons the
  permissive license of the whole codebase.
- Permitted sources: **live wire captures**, **Sunshine server behavior**
  (interop is lawful; we are not derived from it), permissively-licensed
  component crates, and public protocol documentation.
- Every protocol module and every spec doc carries a provenance line:
  `derived from protocol observation against Sunshine vX.Y`.
- See [`clean-room-policy.md`](clean-room-policy.md) for the full discipline.

---

## 2. How to read this corpus

Suggested order for a new engineer:

| # | Doc | What you get |
|---|-----|--------------|
| 1 | [`01-overview.md`](01-overview.md) | Why this exists, scope (Mac+PC), what's in/out. |
| 2 | [`03-bitexact-methodology.md`](03-bitexact-methodology.md) | The capture→fixture→golden→live loop. **The core method.** |
| 3 | [`02-architecture.md`](02-architecture.md) | Crates, traits, threads, data flow. |
| 4 | [`protocol/00-overview.md`](protocol/00-overview.md) | The whole wire protocol on one page: ports, phases, sequence. |
| 5 | `protocol/01..09` | One doc per protocol layer, in connection order. |
| 6 | [`04-platform-backends.md`](04-platform-backends.md) | Decode / render / input / audio per-OS impls behind traits. |
| 7 | [`05-build-plan.md`](05-build-plan.md) | Phases, milestones, task list, exit criteria. |
| 8 | [`06-testing.md`](06-testing.md) | Per-layer tests, capture harness, fuzz, soak. |
| 9 | [`07-performance-budgets.md`](07-performance-budgets.md) | Acceptance gates (latency, 4K120, HDR, loss). |
| 10 | [`08-open-source-and-license.md`](08-open-source-and-license.md) | License, governance, `cargo-deny`. |
| — | [`clean-room-policy.md`](clean-room-policy.md) | Legal/process discipline. |
| — | [`glossary.md`](glossary.md) | Terms (Sunshine, GameStream, RI key, OBU, FEC, …). |

---

## 3. The documentation set (what each doc is, and its status)

Status legend: **DRAFT** = written, needs live-capture verification ·
**SPEC** = byte-exact and fixture-backed · **TODO** = planned, not yet written.

### Top level
| Doc | Purpose | Status |
|-----|---------|--------|
| `01-overview.md` | Vision, strategic bet, scope boundaries. | DRAFT |
| `02-architecture.md` | Crate layout, trait seams, threading, data flow. | DRAFT |
| `03-bitexact-methodology.md` | The capture/fixture/golden-test engine for byte-exactness. | DRAFT |
| `04-platform-backends.md` | Per-OS decode/render/input/audio impls behind traits. | DRAFT |
| `05-build-plan.md` | Phased milestones, task breakdown, exit criteria. | DRAFT |
| `06-testing.md` | Test strategy, capture harness, loss injection, fuzz, soak. | DRAFT |
| `07-performance-budgets.md` | Acceptance criteria (gates, not aspirations). | DRAFT |
| `08-open-source-and-license.md` | License choice, governance, CI license gate. | DRAFT |
| `clean-room-policy.md` | Clean-room discipline and provenance rules. | DRAFT |
| `glossary.md` | Shared vocabulary. | DRAFT |

### Protocol (`protocol/`) — the bit-for-bit Sunshine integration spec
Documented in **connection order**. Each is the spec for one independently
testable layer; each lands with a fixture and a live-validation note before it
is promoted from DRAFT to SPEC.

| Doc | Layer | Transport | Status |
|-----|-------|-----------|--------|
| `00-overview.md` | Protocol map, ports, phase sequence, provenance. | — | DRAFT |
| `01-discovery.md` | mDNS `_nvstream._tcp`, manual hosts, reachability. | UDP 5353 / HTTP 47989 | DRAFT |
| `02-pairing-and-crypto.md` | `/pair` ladder + **all crypto** (PIN KDF, AES-128, RI key/IV, AES-GCM). | HTTPS 47984 / HTTP 47989 | DRAFT |
| `03-serverinfo-and-negotiation.md` | `/serverinfo` GameStream XML, codec flags, config negotiation. | HTTPS 47984 | DRAFT |
| `04-applist-and-launch.md` | `/applist`, `/launch`, `/resume`, `/cancel`. | HTTPS 47984 | DRAFT |
| `05-rtsp.md` | `OPTIONS/DESCRIBE/SETUP/ANNOUNCE/PLAY`, SDP, crypto+port extraction. | RTSP/TCP 48010 | DRAFT |
| `06-control-enet.md` | ENet reliable-UDP, AES-GCM, control message catalog. | UDP 47999 | DRAFT |
| `07-video-rtp-fec.md` | RTP video, **Reed-Solomon FEC geometry**, reassembly, codec AUs. | UDP 47998 | DRAFT |
| `08-audio-rtp.md` | RTP audio, FEC, Opus decode, channel layout, A/V sync. | UDP 48000 | DRAFT |
| `09-input.md` | Keyboard/mouse/gamepad packet formats, encryption, pacing. | (over control) | DRAFT |

> Port numbers above are the conventional Sunshine defaults and are themselves
> **[CAPTURE-LOCKED]** — confirm the exact base port and per-stream offsets from
> the RTSP `SETUP` exchange and live capture before relying on them in code.

---

## 4. Definition of "done" for a protocol doc

A layer's doc is **SPEC** (not DRAFT) only when all four exist and are linked
from the doc:

1. **Prose spec** — the structure, fields, and state machine described here.
2. **Fixture** — a verbatim, committed live capture under `tests/fixtures/<layer>/`.
3. **Golden test** — a round-trip test that encodes to / decodes from the fixture
   byte-for-byte (`cargo test` red if our bytes drift from the capture).
4. **Live-validation note** — a dated note recording a successful exchange with a
   real Sunshine host (version pinned).

No layer is "done" until it both passes its golden test **and** meets its
[performance budget](07-performance-budgets.md) on real hardware.

---

## 5. Relationship to the root `readme.md`

The root [`../readme.md`](../readme.md) is the **strategic brief** (the why, the
business case, the dual-deployment model). This `docs/` tree is the **engineering
plan and spec** (the how). Where they disagree on detail, `docs/` wins; where
they disagree on intent, the readme wins. Keep the readme short; put depth here.
