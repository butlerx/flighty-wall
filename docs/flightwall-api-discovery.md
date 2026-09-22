# FlightWall contract discovery

**Status: captured 2026-09-22. Contract known and probed. Capability gate closed; U5 and U6
may start. One item remains open (§8): where the app derives its per-user API key.**

This document is the U4 deliverable. It records what the vendor documents publicly, the
protocol for capturing the rest safely, what an authorized capture from the owner's own
Mac observed, and the capability gate that U5 and U6 depend on. Nothing in this repository
may assume a FlightWall endpoint, field, or identifier format that is not recorded here from
an observed request.

The rule for this unit, from the plan: **do not invent endpoints.** An invented contract
produces a client that fails against the physical wall instead of in a test.

---

## 1. What the vendor documents publicly

Read 2026-09-22. None of it was sufficient to write a client; the capture in §4 was.

| Fact | Source | Consequence for this project |
| --- | --- | --- |
| Individual flights are tracked by "flight number, callsign, or tail number" | theflightwall.com FAQ | Confirmed by capture: the wall stores a `flight_number` string exactly as typed (`EI61`, `BA5`). No zero-padding, no space. Matches U3's designator directly. |
| The wall is controlled by a mobile app through a cloud account: "all connected users can view and control it from any network" | theflightwall.com FAQ | Confirmed: control is one cloud API, `api.theflightwall.com`. No local interface. |
| "Both the Mini and WideScreen display up to 5 flights at a time" | theflightwall.com FAQ | Client-side only: the app hides Add at 5, but the server stored 10 when asked (§4.5). The daemon enforces five. |
| No public API, webhook, Home Assistant integration, or developer documentation is offered | theflightwall.com FAQ | Capture was the only route. |
| `AxisNimble/TheFlightWall_OSS` is a DIY ESP32 build with no companion app and no local HTTP API | github.com/AxisNimble/TheFlightWall_OSS | Does not share a contract with the commercial Mini. Not used. |

### Questions the public documentation did not answer — now settled

1. **Are area tracking and flight tracking mutually exclusive? No.** The Flight Tracking
   screen says: *"Tracked flights will show regardless of area settings."* Both live in one
   configuration document. **There is no mode to lease or restore. R9 is not applicable and
   U6 drops the mode-lease design.**
2. **What happens after a tracked flight lands? The server removes it.** `EI61` (DUB→SFO,
   departed 12:00 the day of the capture) was in `tracked_flights` at 11:15 and gone by 17:58
   with the app closed the whole time and no client POST. "Will auto-remove" is a server-side
   behaviour. **The daemon may find its own entries gone without having removed them; that
   is normal, not drift.** The *tracking history* list is client-only: AsyncStorage key
   `fw_tracked_flight_history`, `[{flight_number, lastAddedAt}]`, five most recent. It is
   not in the API.
3. **How is a manually added flight distinguished from an API-added one? It isn't.** A
   tracked flight is `{flight_number, created_at, show_distance_travelled, show_metrics}`.
   No actor, source, or ID. **Ownership must be daemon-side, by `flight_number`.**
4. **What identifier does the wall return? None.** The list is keyed by `flight_number`
   only, and the document has no per-entry ID, ETag, or enforced revision (§4.3).

---

## 2. Capture protocol

Run once from the Mac; about 60 minutes including the open sequences in §8.

### Safety rules

These are constraints on the capture itself, not advice:

- Capture only your own device, your own account, and your own wall.
- Bind the proxy to `127.0.0.1`. Never expose it.
- Flows go to `captures/` (gitignored); delete them once fixtures are produced.
- Rotate anything captured that can be rotated: sign out of the app afterwards.
- Remove the interception CA from the keychain when finished. A CA left installed is a
  standing vulnerability on that device.
- No capture CA, and no disabled TLS verification, may reach any non-capture code path.

### Setup

`TheFlightWall.app` (iOS build 3.0.0, Expo 54 / React Native) runs natively on Apple Silicon
and is already signed in to the wall. `NSAllowsArbitraryLoads = true`, no pinning code in
`main.jsbundle`, and the 2026-09-22 capture confirmed interception works with zero TLS errors.

1. `mise install` (pulls `mitmproxy` alongside the other pinned tools), then
   `mise run capture:start` — generates the mitmproxy CA on first run, trusts it in the
   *login* keychain, sets the Wi-Fi web + secure-web proxy to `127.0.0.1:8080` (sudo prompt),
   and runs `mitmdump` headless in the foreground with flows streaming to
   `captures/flightwall.flow`. **Ctrl-C writes `captures/flightwall.har`.** Pass `--web` for the
   mitmweb UI on http://127.0.0.1:8081 instead.
2. Quit and relaunch `TheFlightWall.app` so it picks up the proxy. That relaunch is sequence 1.
3. The app is not AppleScript-scriptable, but with Accessibility granted to the terminal it
   can be driven through System Events (`entire contents of window 1`, match `AXButton` by
   `description`, `perform action "AXPress"`). Typing into the Add field needs a coordinate
   click on the `AXTextField` followed by `keystroke` and Return.

**Fallback: Android.** Install `com.axisnimble.theflightwall` from Google Play, replug the wall
to show the QR code, scan it, and proxy the phone. Not needed so far.

### Teardown

1. Ctrl-C the proxy; `captures/flightwall.har` is written on exit. If the proxy died first,
   `mise exec -- mitmdump -nr captures/flightwall.flow --set hardump=captures/flightwall.har -q`
   converts the flow file.
2. `mise run capture:stop` — turns the proxy off (sudo prompt) and removes the CA from the
   keychain.
3. `mise run capture:sanitize -- --host api.theflightwall.com` (see
   `tests/fixtures/flightwall/README.md`). Read every produced fixture by hand.
4. `mise run capture:stop --purge` — deletes the raw flow and HAR once fixtures are committed.
5. Sign out of the app and back in so the captured `x-user-id` / `x-api-key` pair is dead.

---

## 3. Controlled capture sequences

| # | Sequence | Result 2026-09-22 |
| --- | --- | --- |
| 1 | Cold start: launch, authenticate, connect | **done** — no auth round-trip; the app presents `x-api-key` and `x-user-id` headers from local storage. Which host serves what: §4.1 |
| 2 | Read the full tracked-flight list | **done** — one `GET /configuration`; the list is `request_config.tracked_flights`, complete in one response |
| 3 | Read the current mode | **done** — there is no mode. Area and tracked flights coexist in the same document |
| 4 | Add one flight, then read | **done** — `POST /configuration` with the full document; response echoes it |
| 5 | Add a second flight, then read | **done** — `['EI61', 'BA5']`; order is insertion order |
| 6 | Manual vs API-added | **done** — indistinguishable (§1 Q3) |
| 7 | Remove exactly one flight | **done** — `POST` the document with that entry omitted; `['EI61']` echoed |
| 8 | Remove an already-gone flight | **not needed** — with whole-document writes the daemon GETs, finds nothing to remove, and does not POST |
| 9 | Fill to capacity, then add a sixth | **done** — the server accepts 6 and 10 entries and echoes them back. **The cap is client-side only.** The daemon must enforce five itself |
| 10 | Reschedule or replace | **not applicable** — entries have no schedule field; a reschedule is remove + add by `flight_number` |
| 11 | Switch mode and back | **not applicable** — no mode exists |
| 12 | Interrupt a mutation mid-flight | **done** — full body sent then socket closed before response: **write applied**. Half the body sent then closed: **write not applied**, document intact. Re-POST of the same content is a clean recovery either way |
| 13 | Let a tracked flight go active, then land | **done** — `EI61` was removed server-side after departure with no client involved (§1 Q2) |
| 14 | Idle until the token expires, then act | **done** — the per-user `x-api-key` still worked ~7 h later and **after a sign-out/sign-in cycle in the app**. It is per-install, not per-session |

---

## 4. Findings

From observed requests only. Fixtures in `tests/fixtures/flightwall/`.

### 4.1 Hosts and transport

| Host | Purpose | Notes |
| --- | --- | --- |
| `api.theflightwall.com` | Everything the daemon needs | HTTPS, HTTP/2, behind Cloudflare (`cf-ray`, `cf-cache-status` response headers). Response header `x-fw-backend` present on every response. |
| `cdn.theflightwall.com` | Static assets | Seen in the app's URL cache; not exercised by any configuration operation. Not needed. |
| `plus.theflightwall.com`, `*.supabase.co`, `accounts.google.com` | Sign-in / "Plus" subscription | Present in the JS bundle; **not contacted** during the capture because the app was already signed in. Not needed for a daemon that reuses an existing key pair. |
| `192.168.4.1` | Wall's own setup hotspot | Seen in the URL cache from initial device provisioning. Not part of the API. |

User-agent observed: `TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0`.

### 4.2 Operations

| Operation | Method | Path | Required headers (names) | Request body | Response body | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| Read configuration | `GET` | `/configuration` | `x-api-key`, `x-user-id` | none | the configuration document (§4.2.1) | `meta` absent on GET |
| Write configuration | `POST` | `/configuration` | `x-api-key`, `x-user-id`, `content-type: application/json` | the full configuration document plus `userId` (same value as `x-user-id`) | the document as stored, plus `meta: {savedAtEpochMs, version}` | Whole-document replace. Add and remove are both this call. |
| App feature flags | `GET` | `/feature-flags/app` | `x-api-key` (the short app key) | none | `[{feature_flag_id, enabled, disabled_message, state_changed_at}]` | Observed `individual_flight_tracking: true`, `plus_sync: false`. Useful as a pre-flight check. |
| App messages | `GET` | `/messages/app` | `x-api-key` (app key) | none | `[]` | Not needed. |
| Plus sync poll | `GET` | `/plus/sync` | `x-api-key` (user key) | none | `{status: "none"}` | Polled every ~30 s by the app. Not needed. |

Two distinct `x-api-key` values were observed: a 29-character key on `/feature-flags/app`
and `/messages/app` (an app-level key) and a 43-character key on `/configuration` and
`/plus/sync` (per-install). **The 43-character key is the only credential.** Probed
2026-09-22 (read-only unless noted):

| Request | Result |
| --- | --- |
| correct key, correct `x-user-id` | `200`, document |
| correct key, **wrong** `x-user-id` | `200`, the same document |
| correct key, **no** `x-user-id` | `200`, the same document |
| correct key, wrong `x-user-id` **and** wrong body `userId`, POST | `200`, **write applied** |
| wrong key | `401 {"success": false, "errors": [{"code": 1102, "message": "Invalid API key"}]}` |
| no key | `401 {"success": false, "errors": [{"code": 1101, "message": "Missing API key"}]}` |
| correct key, **no `user-agent`** | `403` Cloudflare error 1010 `browser_signature_banned`, `retryable: false` |

So `x-user-id` and body `userId` are decorative — the daemon must still send them (the app
does) but they authorize nothing. The key survives sign-out and sign-in, so it is bound to
the install, not the session. **Cloudflare blocks requests without the app's `user-agent`;
the daemon must send `TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0` or an equally
app-shaped string.**

`x-user-id` (`fw_ios_` + 22 chars) is stored in the app's AsyncStorage as `fw_user_id`. The
43-character key is **not** in AsyncStorage, NSUserDefaults, or the login keychain (§8).

#### 4.2.1 The configuration document

Top-level: `display_config`, `request_config`, `version`. POST adds `userId`; POST responses
add `meta`.

`request_config.tracked_flights` — the only part the daemon writes:

```json
[
  {
    "flight_number": "EI61",
    "created_at": "2026-09-21T10:49:06.351Z",
    "show_distance_travelled": true,
    "show_metrics": true
  }
]
```

Everything else in the document is the owner's display and area-tracking settings
(`radius_request` with home coordinates, brightness, sleep window, layout, clock). **The
daemon must send these back byte-for-byte unchanged**; a POST replaces the whole document.

### 4.3 Identity, revisions, and provenance

| Question | Finding |
| --- | --- |
| Identifier for a tracked flight | `flight_number` string. Nothing else. |
| Is that identifier stable across reads? | Yes — it is the value the user typed. |
| Revision, ETag, or version field | `version` is **decorative**. Probed: POST with `version: 1`, `version: 99`, and no `version` key all returned `200` and applied; the server stores whatever `version` it is sent (GET returned `99` after the `99` write) and defaults to `2` when absent. `meta.version` in POST responses is always `1`. No `ETag`, no `If-Match`. **Writes are unconditional last-writer-wins on the whole document.** |
| Actor or source field | None. |
| Active-status field | None. The server removes landed flights from `tracked_flights` on its own (§1 Q2); the daemon observes the removal on its next read. |
| Mode provenance | Not applicable — no mode. |

### 4.4 Pagination and completeness

| Question | Finding |
| --- | --- |
| Is the list paginated? | No. `tracked_flights` is a complete array of at most 5. |
| How is the last page signalled? | Not applicable. |
| Can a partial read be distinguished from a complete one? | A `200` with a parseable document is complete. Anything else is non-authoritative. |

### 4.5 Capacity

| Question | Finding |
| --- | --- |
| Documented cap | 5 flights (vendor FAQ, §1) |
| Client behaviour at 5 | The Add field is removed from the screen once 5 entries are staged. |
| Server behaviour when POSTing 6 | **Accepted.** 6 and 10 entries were stored and echoed back unchanged. **There is no server-side cap.** What the wall displays with more than five is unknown and must not be relied on; the daemon enforces five itself. |
| Does the wall ever evict an existing entry? | Only landed flights, server-side (§1 Q2). Otherwise the only eviction path is a client POSTing a shorter list. |

### 4.6 Errors observed

| Condition | Status | Body shape | How the daemon must treat it |
| --- | --- | --- | --- |
| Wrong `x-api-key` | `401` | `{"success": false, "errors": [{"code": 1102, "message": "Invalid API key"}]}` | Credential error; stop, do not retry, report without the key value |
| Missing `x-api-key` | `401` | same shape, `code: 1101, "Missing API key"` | Configuration error |
| Missing / non-app `user-agent` | `403` | Cloudflare error 1010, `error_name: browser_signature_banned`, `retryable: false` | Fatal misconfiguration; never retry |
| Stale or absurd `version` | `200` | normal document | Not an error — `version` is decorative |
| Six or more `tracked_flights` | `200` | normal document | Not an error server-side; the daemon must never send more than five |
| Interrupted POST, full body delivered | connection error client-side | — | **The write may have applied.** Re-read and compare; do not blindly retry |
| Interrupted POST, partial body | connection error client-side | — | Write not applied; document intact. Re-read and compare |
| Rate limit | not observed | — | ~40 requests in 10 minutes and a burst of ~15 in 30 s drew no `429` |

### 4.7 Contract fingerprint

| Candidate | Where it appears | Finding |
| --- | --- | --- |
| `display_config.model` | configuration document | `"mini-v1"`. Check on every read; refuse to write if it changes. |
| Top-level key set | configuration document | `{display_config, request_config, version}` on GET. Refuse to write if keys appear or disappear. |
| `tracked_flights[]` key set | configuration document | `{flight_number, created_at, show_distance_travelled, show_metrics}`. Refuse to write if it changes. |
| `x-fw-backend` | response header | Present on every response; value not yet compared across days. |

---

## 5. If the capture is not possible

Not needed — the app is not pinned and the Mac capture worked. Kept for the Android fallback:
do **not** patch the app to weaken TLS. Inspect the owned APK for hostnames and paths first,
ask the vendor second, and return for a scope decision third.

---

## 6. Fixture provenance

| Fixture | Captured | App version | Platform | Firmware | Fields removed beyond sanitizer defaults |
| --- | --- | --- | --- | --- | --- |
| `get-configuration.json` | 2026-09-22 | 3.0.0 (Expo 54) | iOS build on macOS 26.7, arm64 | `display_config.model = mini-v1`; firmware not exposed | none; `radius_request.id/latitude/longitude` and all header values redacted by default |
| `post-configuration-add.json` | 2026-09-22 | 3.0.0 | as above | as above | `userId` redacted by key (rule added after this capture found it leaking) |
| `post-configuration-remove.json` | 2026-09-22 | 3.0.0 | as above | as above | as above |
| `get-feature-flags.json` | 2026-09-22 | 3.0.0 | as above | as above | none |

All four were read by hand and scanned against the raw HAR for the two API keys, the user id,
the radius id, and the home coordinates: zero hits.

---

## 7. Capability gate

Re-evaluated against the capture. Rows that read **pass (reduced)** pass because the contract
is *simpler* than the gate assumed, not because the guarantee was weakened — but the reduced
design has one race the original did not (§8, sequence 12).

| # | Capability required | Result |
| --- | --- | --- |
| 1 | Complete, authoritative read with partial reads detectable | **pass** — one unpaginated document; `200` + schema match is authoritative |
| 2 | Stable remote identifier per tracked flight | **pass (reduced)** — `flight_number` is the identifier. Two Friends on the same flight collapse to one entry, which U3 already does |
| 3 | At least two simultaneous tracked flights | **pass** — two observed, five supported |
| 4 | Conditional remove that cannot delete the wrong entry | **pass (reduced)** — there is no conditional write. Safe remove is: fresh `GET` → drop only `flight_number`s in the daemon's own journal → `POST` everything else unchanged. The GET→POST window is a last-writer-wins race with the app; see §8 |
| 5 | Manual entries distinguishable, or safely inferable from owned records | **pass (reduced)** — not distinguishable remotely. The journal of `flight_number`s the daemon added is the only ownership record. A `flight_number` present on the wall before the daemon first saw it is manual and never removed |
| 6 | Recoverable state after an uncertain mutation | **pass** — sequence 12. A full-body interruption applied the write; a partial-body interruption did not; in both cases a fresh GET showed a valid document and a content-identical re-POST converged. Recovery is read-compare-replan, never blind retry |
| 7 | Defined behaviour at capacity, no eviction of non-owned entries | **pass (reduced)** — sequence 9. The server has no cap; it stored 10. Nothing is evicted server-side except landed flights. The five-entry limit is therefore the daemon's responsibility, enforced before every POST |
| 8 | Reschedule semantics mappable onto the U3 key | **pass** — the wall has no schedule; a reschedule within a day is a no-op, across days is remove + add |
| 9 | Mode read and set, restorable | **not applicable** — no mode exists. R9 is withdrawn |
| 10 | No committed artifact contains a token, device secret, CA key, location, or account id | **pass** — §6; `userId` rule added and tested after the first sanitizer pass leaked it |

**Gate verdict: closed. U5 and U6 may start with no behaviour disabled.** The contract is
weaker than the original gate wanted (no conditional writes, no server cap, no ownership
signal) but every weakness has a daemon-side answer: read-modify-write with the owner's
settings passed through untouched, a self-enforced cap of five, and a journal as the only
ownership record.

---

## 8. Still open

One item, and it does not block U5:

**Where does the app get the 43-character per-user `x-api-key`?** It is not in AsyncStorage
(`fw_user_id` is there; the key is not), not in NSUserDefaults, and not in the login keychain.
`main.jsbundle` is Hermes bytecode (magic `c61fbc03`), so the derivation is not readable as
text; the bundle does reference `expo-crypto`, `sha256`, and `getRandomBytes`, and an
`/authenticatedeviceSetup`-adjacent path, so the key is probably minted during device setup
and held in the iOS data-protection keychain that `security` cannot enumerate. Two ways to
close this, in order of preference:

1. **Copy it once from a capture** — the daemon's credential file is the key plus the user
   id, both already in `captures/flightwall.flow`. This is what U5 will document as the
   setup step. It survives sign-out, so it does not need refreshing.
2. **Recover the derivation** — decompile the Hermes bundle (`hermes-dec` or `hbc-decompiler`)
   and read the setup flow. Only worth it if the key ever stops working.

Everything else from the original list is done and recorded in §3, §4.2, §4.3, §4.5, §4.6.
The wall was returned to `tracked_flights = []` after the probes; the owner's `EI61` had
already been removed by the server on landing before the probes began.
