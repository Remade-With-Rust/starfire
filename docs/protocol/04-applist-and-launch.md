# Protocol 04 — App List & Session Launch

> Provenance: observation against Sunshine vX.Y. Clean-room. Query-param names and
> value encodings are **[CAPTURE-LOCKED]** to request-line fixtures.

## Goal

Enumerate launchable apps and start (or rejoin) a session, carrying the negotiated
config and seeding the session crypto.

## 1. `/applist`

- `GET https://<host>:47984/applist` (mTLS) → GameStream XML list of apps
  (Desktop, Steam Big Picture, custom). Each has an **app id** and title.
- Parse with `quick-xml`; expose `Vec<App { id, title }>`.

## 2. `/launch`

Starts a fresh session. A GET with a query string of session parameters:

```
GET https://<host>:47984/launch?appid=<id>&mode=<W>x<H>x<FPS>&...
```

Parameters we must send — **[CAPTURE-LOCKED]** names/encodings:

| Param (logical) | Meaning |
|-----------------|---------|
| app id | which app (from `/applist`) |
| mode | `WxHxFPS` |
| bitrate | target kbps |
| packet size | media packet MTU sizing |
| HDR | on/off (+ HDR metadata mode) |
| surround / audio config | channel layout |
| gamepad mask | which controller slots are present |
| client cert hash | binds the session to our identity |
| **RI key / IV** | session crypto material (hex) — keys control+input AES-GCM |
| FEC / refresh flags | per-host options |

- The **RI key + IV** sent/derived here are the session keys §02 names and §06/§09
  consume. **[CAPTURE-LOCKED]**: whether the client supplies them or the host
  returns them, and the exact hex encoding/length.

## 3. `/resume`

- Rejoin a session already running on the host (the host was left streaming).
  `GET .../resume?...` with the subset of params needed to re-attach + fresh
  crypto. Decide resume vs launch from `currentgame` in `/serverinfo` (§03).

## 4. `/cancel`

- `GET .../cancel?...` to terminate the running session cleanly. Sent on quit and
  on unrecoverable error before teardown.

## Output of this phase

A launched (or resumed) session on the host, plus the negotiated config and the
RI key/IV in hand, ready for RTSP (§05) to set up the actual media/control streams.

## Tests

- **Fixture:** the exact `/applist` XML and the `/launch` request line (with
  secrets redacted/synthesized per fixture hygiene).
- **Golden:** build the launch URL from a config struct, assert it equals the
  captured request line byte-for-byte (param order included).
- **Live:** launch Desktop on the real host; dated note.

## Open / to-confirm

- [ ] Full launch param set + exact names and order. **[CAPTURE-LOCKED]**
- [ ] RI key/IV direction (client-supplied vs host-returned) + encoding. **[CAPTURE-LOCKED]**
- [ ] `/resume` param subset. **[CAPTURE-LOCKED]**
- [ ] Gamepad mask bit semantics. **[CAPTURE-LOCKED]**
