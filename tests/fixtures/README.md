# Capture fixtures

Committed, version-stamped captures from a live Sunshine host. **These are the
spec** — golden tests assert our bytes match them (docs/03-bitexact-methodology.md).

Layout: one subdir per protocol layer, e.g.

```
pairing/getservercert.bin        # verbatim captured bytes
pairing/getservercert.meta.toml  # sunshine_version, captured date, layer, notes
serverinfo/desktop-av1.bin
serverinfo/desktop-av1.meta.toml
video/lossless-gop.bin
...
```

`.meta.toml` schema (see `starfire-testkit::Meta`):

```toml
sunshine_version = "0.23.1"
captured        = "2026-06-18"
layer           = "pairing/getservercert"
codec           = "AV1-Main8"   # optional, for media
notes           = "host=gaming-vm, res=3840x2160@120, hdr=on; secrets redacted"
```

Rules:
- **No durable secrets.** Throwaway identity/host; document redactions in notes.
- Re-capture + re-stamp on Sunshine upgrade — a green test on a stale fixture is
  a false positive.
- Raw session `.pcap` files are NOT committed (see `.gitignore`); slice the
  per-layer `.bin` fixtures from them.
