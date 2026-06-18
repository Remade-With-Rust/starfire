# fixture-slicer

Cuts a live Sunshine **session pcap** into per-layer fixtures for the bit-exact
methodology ([docs/03](../../docs/03-bitexact-methodology.md),
[docs/06](../../docs/06-testing.md)). Std-only, zero dependencies.

## Capture

```sh
# On the host-facing interface (e.g. virbr0 for the gaming VM).
# Classic pcap only — pcapng is not supported yet.
sudo tcpdump -i virbr0 -w session.pcap \
  'udp port 5353 or tcp port 47984 or tcp port 47989 or tcp port 48010 \
   or udp port 47998 or udp port 47999 or udp port 48000'
```

Drive a reference session against the host while capturing. We observe the
**host's** bytes — clean-room is preserved ([clean-room-policy](../../docs/clean-room-policy.md)).

## Inspect first

```sh
cargo run -p starfire-fixture-slicer -- session.pcap --list
```

Prints packets-per-layer and the reassembled connection/datagram summary, writing
nothing. Run this before slicing to sanity-check the capture.

## Slice into fixtures

```sh
cargo run -p starfire-fixture-slicer -- session.pcap \
  --out tests/fixtures \
  --sunshine-version 0.23.1 \
  --captured 2026-06-18 \
  --notes "host=gaming-vm, 3840x2160@120, hdr=on; secrets redacted"
```

Output under `tests/fixtures/<layer>/`:

| Layer | Transport | Fixture file |
|-------|-----------|--------------|
| `rtsp`, `http-control` | TCP | `<ip>-<port>-c2s.bin`, `-s2c.bin` (reassembled transcripts) |
| `video`, `audio`, `control`, `mdns` | UDP | `<ip>-<port>.frames` (u32-LE length-prefixed datagrams) |

Each file gets a sibling `.meta.toml` (version-stamped; loads via
`starfire_testkit::Fixture::load`).

## Limits (by design)

- **Classic pcap only** (`tcpdump -w`), not pcapng.
- **HTTPS (47984) is counted, not sliced** — it's encrypted. Capture the
  plaintext HTTP control port (47989) for discovery/pairing/serverinfo/launch.
- IP fragments and IPv6 extension-header chains are skipped.
- Default Sunshine ports assumed ([docs/protocol/00](../../docs/protocol/00-overview.md));
  non-standard ports need a code/CLI override (TODO until a capture needs it).

## Hygiene

Fixtures are committed and are the spec — **redact durable secrets** and use a
throwaway identity/host. Re-capture + re-stamp on Sunshine upgrade.
