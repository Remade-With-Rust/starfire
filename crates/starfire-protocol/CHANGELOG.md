# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.0](https://github.com/Remade-With-Rust/starfire/releases/tag/starfire-protocol-v0.0.0) - 2026-10-09

### Other

- drop a redundant cast in the transit floor test (clippy)
- hold the transit floor's monotonic queue in a Vec with a head index
- compute the transit tracker's timeline in i64, not u128
- convert recorded latencies to microseconds in u64, not u128
- recover through borrowed shard views (Fec::recover_views)
- build the recovery source list with exact capacity
- key the FEC coder cache with a short vector, not a SipHash map
- assemble the RTP video header in registers and inline the writer
- inline the RTP video header parser into its per-packet caller
- wrap the latency ring index with a compare, not a division
- run the transit floor window on an integer nanosecond timeline
- compute latency percentiles by selection, not a full sort
- build each Cauchy row once per recovery, not once per lost shard
- recover lost shards with an e x e Cauchy solve, not a k x k inverse
- Wi-Fi low-latency pass -- instrument the pipeline, then remove the waits it found
- migrate pairing crypto + identity (shared with Comet)
- keep shared crate GameStream-only (move auth to Comet)
- negotiated security-profile vocabulary (auth)
- fix cross-crate build + test-helpers plumbing
- finished starfire
- finished starfire
