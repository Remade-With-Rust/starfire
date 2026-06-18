# Protocol 03 — Server Capabilities & Negotiation

> Provenance: observation against Sunshine vX.Y. Clean-room. XML element names and
> flag bits are **[CAPTURE-LOCKED]** to a verbatim `/serverinfo` fixture.

## Goal

Read the host's capabilities from `/serverinfo` (GameStream **XML**, not JSON) and
choose the best mutually-supported session configuration.

## 1. `/serverinfo`

- Unauthenticated: `GET http://<host>:47989/serverinfo` (probe; §01).
- Authenticated (mTLS): `GET https://<host>:47984/serverinfo` — full capabilities.
- Response is a GameStream XML document. Parse with `quick-xml` (MIT).

### Fields we consume — [CAPTURE-LOCKED]
Exact element names come from the fixture; logically we need:

| Logical field | Use |
|---------------|-----|
| `hostname` | display |
| app/GfeVersion | compat checks |
| `PairStatus` | 0 unpaired / 1 paired (§01) |
| HTTPS port | where to do mTLS calls |
| `ServerCodecModeSupport` | codec bitfield (below) |
| max resolution / max FPS | negotiation ceiling |
| HDR support | enable HDR10/BT.2020 PQ |
| surround capability | audio channel layout |
| `currentgame` / running app | resume vs launch decision |
| server uuid | host identity / persistence key |

## 2. `ServerCodecModeSupport` bitfield

- **AV1 = `0x40000`** (the host here advertises AV1-Main8).
- HEVC and H.264 (and Main10/HDR variants) occupy other bits. **[CAPTURE-LOCKED]**:
  confirm each bit position from the fixture before trusting it; do not infer.
- We decode this into a `CodecCaps { av1, hevc, h264, main10, … }` struct, tested
  against the captured value.

## 3. Negotiation

Pick the highest-quality config both sides support, bounded by client display and
user settings:

```
codec     = first supported of [AV1, HEVC, H264] that the client can hw-decode
bit_depth = 10 if (HDR requested && host main10 && display HDR) else 8
resolution= min(user/display target, host max)
fps       = min(user/display target, host max)
hdr       = host HDR && display HDR && codec supports it
color     = BT.2020 PQ if hdr else BT.709
```

- The negotiated config flows into `/launch` (§04) and the RTSP `ANNOUNCE` (§05),
  and selects the runtime `VideoDecoder`.
- **Never silently fall back** from hardware to software decode, or from AV1 to
  H.264 — surface it (perf budgets, [`../07-performance-budgets.md`](../07-performance-budgets.md)).

## Tests

- **Fixture:** verbatim `/serverinfo` XML (one per codec/HDR variant captured).
- **Golden:** parse → assert all logical fields + the decoded `CodecCaps`.
- **Unit:** negotiation function over a matrix of (host caps × client caps).
- **Live:** read real host caps; dated note.

## Open / to-confirm

- [ ] Exact XML element names and namespaces. **[CAPTURE-LOCKED]**
- [ ] Full `ServerCodecModeSupport` bit map. **[CAPTURE-LOCKED]**
- [ ] How max-res/max-fps are expressed (single field vs per-codec). **[CAPTURE-LOCKED]**
