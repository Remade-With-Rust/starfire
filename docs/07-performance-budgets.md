# 07 — Performance & Quality Budgets (acceptance criteria)

> "Optimize for absolute highest performance and quality." These are **gates, not
> aspirations.** A layer is not done until it meets its budget on real hardware,
> measured and published — not asserted.

| # | Budget | Gate | How measured |
|---|--------|------|--------------|
| 1 | **Added latency** | client-introduced latency (wire-arrival → photons) ≤ ~1 frame-time over network RTT at session FPS | measure decode-in→present-out; publish the number |
| 2 | **Resolution / FPS** | up to **4K @ 120 FPS** end-to-end where host+display allow; never the bottleneck below host caps | live session at 3840×2160@120 |
| 3 | **HDR** | **HDR10 / BT.2020 PQ** correct end-to-end | reference HDR content, color-checked |
| 4 | **Decode path** | hardware on both platforms; `dav1d` SW **only as explicit fallback, never silent** | log + surface the active path |
| 5 | **Loss resilience** | no visible artifacts up to FEC design recovery rate; clean IDR recovery beyond it (no green-screen / persistent corruption) | deterministic loss injection (§06 testing) |
| 6 | **Pacing** | no judder from client timing; jitter buffer tuned + exposed as a setting | frame-time variance under threshold |
| 7 | **Zero-copy + hot path** | zero-copy decode→present where OS allows; bounded, lock-free hot path; **no allocs per packet** in steady state | profiler / alloc counter |
| 8 | **Input** | sub-frame encode+send; correct scaling; **anti-cheat-safe pacing** (non-synthetic timing) | latency probe + timing-distribution audit (§09) |
| 9 | **Stability** | **zero panics** on malformed/lossy input; multi-hour soak with no leak/drift | fuzz + soak (§06 testing) |

## Rules

- **Measure and publish**, don't claim. Each budget has a number checked in CI or
  a documented manual procedure.
- **Never silently degrade.** Falling back from HW→SW decode, AV1→H.264, or
  dropping resolution must be surfaced to the user/logs.
- Budgets are checked **per layer as it lands** and again at the **Phase exit
  criteria** ([`05-build-plan.md`](05-build-plan.md)).
