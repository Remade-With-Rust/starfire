# 08 — Open-Source & License

## License

- **Apache-2.0** for the core. The patent grant matters in codec/protocol
  territory — that's why Apache over MIT/BSD here. Apply it consistently across
  the whole tree.
- **No copyleft anywhere.** Zero GPL/LGPL (or other reciprocal) dependencies.
- Every source file carries the SPDX header: `// SPDX-License-Identifier: Apache-2.0`
  plus, for protocol modules, the clean-room provenance line
  ([`clean-room-policy.md`](clean-room-policy.md)).

## CI license gate (`cargo-deny`)

- `cargo deny check licenses` **fails the build** on any GPL/LGPL/copyleft license
  in the dependency graph.
- Maintain an allowlist of acceptable licenses (Apache-2.0, MIT, BSD-2/3,
  ISC, Zlib, Unicode). Anything else requires explicit review.
- Run in CI on every PR; run locally pre-commit.

## Dependency posture (the permissive leverage)

| Need | Crate | License |
|------|-------|---------|
| Control transport | `rusty_enet` | MIT |
| FEC | `reed-solomon-erasure` | MIT/Apache |
| Audio decode / output | `opus`/`audiopus` / `cpal` | BSD / Apache·MIT |
| SW video decode | `dav1d` | BSD |
| Crypto | `aes`/`aes-gcm`, `ring`, `sha2` | permissive |
| TLS / certs | `rcgen`, `rustls` | permissive |
| XML | `quick-xml` | MIT |
| mDNS | (e.g.) `mdns-sd` | MIT |

Confirm every actual license via `cargo-deny`, not this table.

## Governance

- **DCO or CLA** so the project can keep relicensing/embedding flexibility (the
  dual-deployment model in [`01-overview.md`](01-overview.md) depends on it).
- Documented **clean-room policy**; PR template includes a clean-room attestation.
- **Commitment honesty:** either staff light-touch stewardship or label it
  "source-available, best-effort." An abandoned thrown-over-the-wall repo
  generates ill will, not goodwill.

## Repo & API

- Standalone repo, own brand — **never "Moonlight"** in code, brand, or docs.
- Clean public API on `starfire-core`; the four platform traits
  ([`04-platform-backends.md`](04-platform-backends.md)) are the documented
  extension surface.
- Ship a **"build your own platform backend"** guide pointing at the decoder /
  renderer / input / audio traits — that's what the community extends.

## What the community builds (the long tail we hand off)

Linux / Android / iOS / web / TV clients, additional platform decoder/renderer/
input/audio backends, and exotic input devices — all behind the traits the
Windows/Mac client already defines.
