# Clean-Room Policy

> Process risk, not technical risk — but the most expensive kind. A single GPL
> source peek can poison the permissive license of the entire codebase. This
> policy is binding on every contributor.

## The rule

**Do not read, reference, copy from, or "check how they did it" in Moonlight,
`moonlight-common-c`, or any other GPL/LGPL GameStream client while contributing
to Starfire.** Not the code, not the comments, not a stranger's gist that quotes
them. If you have read that source recently for unrelated reasons, do not work on
the corresponding Starfire layer from memory of it.

## Lawful sources of truth

| Allowed | Why |
|---------|-----|
| Live wire captures (`tcpdump`/`pcap`) of a Sunshine session | We observe bytes on the wire; observation isn't derivation. |
| Sunshine **server** behavior / its public docs | Interoperating with a GPL server is lawful; we are not a derivative of it. |
| Permissively-licensed component crates (MIT/Apache/BSD) | Vetted by `cargo-deny`. |
| Public, non-GPL protocol write-ups and RFCs (RTP, RTSP, Opus, AV1) | Standards, not Moonlight. |
| Our own committed fixtures and golden tests | The spec, per [`03-bitexact-methodology.md`](03-bitexact-methodology.md). |

## Provenance headers

Every protocol module starts with:

```rust
//! Derived from protocol observation against Sunshine vX.Y.
//! Clean-room: no Moonlight / moonlight-common-c source was consulted.
//! See docs/clean-room-policy.md.
```

This is cheap legal insurance and a genuine selling point of the project.

## On Sunshine being GPLv3

Sunshine (the host) is GPLv3. **Interoperating** with it over the wire is fine;
**deriving** from it is not. We do not link Sunshine, copy its source into ours,
or translate its code. Reading the server to understand what bytes it emits is
the same lawful interop any client does — but prefer the *capture* as the record
of truth, and never paste server code into Starfire.

## CI enforcement

- `cargo-deny` fails the build on any GPL/LGPL (or otherwise copyleft) dependency
  anywhere in the tree. See [`08-open-source-and-license.md`](08-open-source-and-license.md).
- PR template includes a clean-room attestation checkbox.
- Module-header lint (CI grep) ensures every `protocol/` module carries the
  provenance line.

## If in doubt

Stop and ask. A delayed layer is cheaper than a relicensing event.
