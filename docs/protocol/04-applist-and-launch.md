# Protocol 04 — App List & Session Launch

> Provenance: observation against **Sunshine 2026.516.143833**. Clean-room.
> Validated by a live session starting (`launch` returns the RTSP URL).

## ✅ Status — applist + launch work live (F4)
`starfire_core::launch::PairedClient` (the authenticated mTLS client) does
`/serverinfo`, `/applist`, `/launch`, `/resume`, `/cancel`. Confirmed by
`live_launch` + golden tests against captured fixtures.

### Live-validation note
- **2026-06-18** — over mTLS: `/applist` returned Desktop + Steam Big Picture;
  `/launch` (Desktop, `mode=1920x1080x60`) returned
  `sessionUrl0 = rtsp://127.0.0.1:48010`, `gamesession=1`; `/cancel` → 200.
  Fixtures: `tests/fixtures/{applist,launch}/`.

### `/applist` — real shape
```xml
<root status_code="200">
  <App><IsHdrSupported>1</IsHdrSupported><AppTitle>Desktop</AppTitle><ID>881448767</ID></App>
  <App>…Steam Big Picture…<ID>1093255277</ID></App>
</root>
```
`App = { ID, AppTitle, IsHdrSupported }`. **App IDs are host-assigned hashes**, not
sequential — you must read them from `/applist` (launching `appid=1` fails with an
inner `status_code="404"`, "Failed to start the specified application").

### `/launch` — confirmed params + response
Request (mTLS GET) — the param set that started a session:
```
/launch?uniqueid=<id>&appid=<id>&mode=<W>x<H>x<FPS>&additionalStates=1&sops=0
        &rikey=<hex16>&rikeyid=<i32>&localAudioPlayMode=0&surroundAudioInfo=<u32>
        &remoteControllersBitmap=0&gcmap=0&hdrMode=0|1
```
- **`rikey`** = 16 random bytes (hex) = the RI session key; **`rikeyid`** = an i32.
  Generated client-side; reused by control/input/video (F6+). The IV derivation is
  still **[CAPTURE-LOCKED]** (resolved in F6).
- `surroundAudioInfo=196610` worked for stereo; per-layout encoding [CAPTURE-LOCKED].
Response: `<sessionUrl0>rtsp://host:48010</sessionUrl0><gamesession>1</gamesession>`.
On failure, `<root status_code≠200 status_message=…>` (parsed as an error).

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
