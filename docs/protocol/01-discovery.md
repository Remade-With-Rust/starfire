# Protocol 01 — Discovery & Host Management

> Provenance: observation against Sunshine vX.Y. Clean-room. `[CAPTURE-LOCKED]`
> fields resolve to fixtures — see [`../03-bitexact-methodology.md`](../03-bitexact-methodology.md).

## Goal

Find Sunshine hosts on the LAN (or accept a manual address), probe reachability
and pairing status, and persist known hosts with their paired identity.

## 1. mDNS discovery

- Browse for the service type **`_nvstream._tcp`** over mDNS (UDP 5353).
- Resolve each instance to host name, IPv4/IPv6, and the control port from the
  SRV/A/AAAA/TXT records. **[CAPTURE-LOCKED]**: exact TXT keys and SRV port.
- Crate: a permissively-licensed mDNS crate (e.g. `mdns-sd`, MIT) — confirm
  license via `cargo-deny`.

## 2. Manual host entry

- Accept `host[:port]` directly; skip mDNS. Same downstream path.

## 3. Reachability & pair-status probe

- `GET http://<host>:47989/serverinfo` (unauthenticated) returns GameStream XML.
- From it read: `PairStatus` (0 = unpaired, 1 = paired), `hostname`, app version,
  HTTPS port, and a `uniqueid`/`mac`-style identifier. **[CAPTURE-LOCKED]**:
  exact element names — see [`03-serverinfo-and-negotiation.md`](03-serverinfo-and-negotiation.md).
- This unauthenticated probe is how we decide *pair* vs *connect*.

## 4. Persistence

Per the baseline requirement of **granular, per-entry storage** (no single-blob
formats), persist each known host as its own record:

```
known-hosts/
  <host-uuid>/
    identity.toml      # hostname, addresses, server uuid, last-seen
    client-cert.pem    # our identity cert for this host
    client-key.pem     # our private key (encrypted at rest, see below)
    host-cert.pem      # the host's cert, pinned at pairing
```

- Private key at rest: **Argon2id KDF + AES-256-GCM** (workspace crypto baseline).
- Pin the host cert at pairing time; on reconnect, mTLS validates against the
  pinned cert (trust-on-first-use, then strict).

## State machine

```
            ┌─────────── mDNS / manual ───────────┐
            ▼                                      │
        discovered ── probe /serverinfo ──► pair-status?
            │                                  │        │
            │                          unpaired│        │paired
            ▼                                  ▼        ▼
        persisted                          → §02 pair  → §03 serverinfo (mTLS)
```

## Tests

- **Fixture:** captured mDNS response + unauthenticated `/serverinfo` XML.
- **Golden:** parse fixture → assert host fields + pair-status.
- **Live:** discover the real gaming-VM host; dated note here.

## Open / to-confirm

- [ ] Exact `_nvstream._tcp` TXT record keys. **[CAPTURE-LOCKED]**
- [ ] IPv6 behavior and link-local handling.
- [ ] Whether the HTTPS port is always advertised or assumed base+(-5).
