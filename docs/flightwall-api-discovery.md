# FlightWall contract discovery

**Status: partially captured 2026-09-22. Contract known; U5 may start with a reduced design.
Sequences 9, 12, 13, 14 are still open — see §8.**

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
| "Both the Mini and WideScreen display up to 5 flights at a time" | theflightwall.com FAQ | Confirmed in the app: the Add field disappears at 5. Server-side behaviour at 6 is untested (§8). |
| No public API, webhook, Home Assistant integration, or developer documentation is offered | theflightwall.com FAQ | Capture was the only route. |
| `AxisNimble/TheFlightWall_OSS` is a DIY ESP32 build with no companion app and no local HTTP API | github.com/AxisNimble/TheFlightWall_OSS | Does not share a contract with the commercial Mini. Not used. |

### Questions the public documentation did not answer — now settled

1. **Are area tracking and flight tracking mutually exclusive? No.** The Flight Tracking
   screen says: *"Tracked flights will show regardless of area settings."* Both live in one
   configuration document. **There is no mode to lease or restore. R9 is not applicable and
   U6 drops the mode-lease design.**
2. **What happens after a tracked flight lands?** Each entry shows *"Will auto-remove"* in the
   app, and there is a *tracking history* list (grew from 1 to 3 entries during the capture
   as test flights were added). Whether auto-remove is a client-side or server-side action,
   and what it does to the document, is untested (§8, sequence 13).
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
| 9 | Fill to capacity, then add a sixth | **open** — reached 5 in the app (Add field disappears) but the Save did not fire before teardown. Server behaviour at 6 untested |
| 10 | Reschedule or replace | **not applicable** — entries have no schedule field; a reschedule is remove + add by `flight_number` |
| 11 | Switch mode and back | **not applicable** — no mode exists |
| 12 | Interrupt a mutation mid-flight | **open** |
| 13 | Let a tracked flight go active, then land | **open** — "Will auto-remove" and tracking history observed in the UI only |
| 14 | Idle until the token expires, then act | **open** — no expiry observed in ~20 minutes; key lifetime unknown |

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
and `/messages/app` (an app-level key baked into the bundle) and a 43-character key on
`/configuration` and `/plus/sync` (per-user). `x-user-id` is a 29-character opaque string
prefixed `fw_ios_`, so it is minted per install, not per account. The daemon needs the
per-user key and the user id from the owner's signed-in app; see §8 for how those are
obtained without a second capture.

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
| Revision, ETag, or version field | `version` is in the document and was **2 before and after two successful writes**. The POST response `meta.version` was `1` both times. Neither is a concurrency check: **writes are last-writer-wins on the whole document.** No `ETag`, no `If-Match`. |
| Actor or source field | None. |
| Active-status field | None in the document. The UI's "Will auto-remove" and tracking history are not in `/configuration`; source unknown (§8). |
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
| Server behaviour when POSTing 6 | **untested** (§8). |
| Does the wall ever evict an existing entry? | Not observed. Under last-writer-wins the only eviction risk is the daemon itself POSTing a stale document. |

### 4.6 Errors observed

None. Every request returned `200`. Error shapes for a bad key, a stale `version`, or six
entries are all untested (§8).

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
| 6 | Recoverable state after an uncertain mutation | **open** — sequence 12. Because POST is idempotent on content, re-reading and re-planning should recover, but this is untested |
| 7 | Defined behaviour at capacity, no eviction of non-owned entries | **open** — sequence 9. Client-side cap confirmed; server response to 6 entries untested |
| 8 | Reschedule semantics mappable onto the U3 key | **pass** — the wall has no schedule; a reschedule within a day is a no-op, across days is remove + add |
| 9 | Mode read and set, restorable | **not applicable** — no mode exists. R9 is withdrawn |
| 10 | No committed artifact contains a token, device secret, CA key, location, or account id | **pass** — §6; `userId` rule added and tested after the first sanitizer pass leaked it |

**Gate verdict: U5 may start.** Rows 6 and 7 stay open and are cheap to close (§8). U6 must
not enable automatic removal until row 6 is closed, and must not add a sixth flight until row
7 is.

---

## 8. Still to capture

Each is a few minutes with the proxy up. In priority order:

1. **Sequence 9 — server at capacity.** Stage `BA1 BA2 BA3 BA4`, Save (5 entries), then POST
   a 6-entry document from the shell using the captured headers. Record status and body.
   Then remove the four test flights. Closes gate row 7.
2. **Sequence 12 — interrupted write.** POST a document, kill the connection before the
   response, GET, compare. Closes gate row 6.
3. **Stale `version`.** POST with `version: 1` and with `version: 99`. If both return `200`
   and take effect, `version` is decorative and U5 must treat every write as unconditional.
4. **Sequence 13 — post-landing.** Leave `EI61` tracked until it lands. Diff `/configuration`
   before and after, and find what backs the tracking-history list (it was not in any
   captured request; probably local).
5. **Sequence 14 — key lifetime.** Re-run `GET /configuration` with the captured key pair
   after 24 h and after a sign-out. If it still works after sign-out, the key is per-install
   and the daemon can keep it; if not, the daemon needs the sign-in flow and this document
   needs a §4.2 row for it.
6. **Obtaining the key pair for the daemon.** The values live in the app's container:
   `~/Library/Containers/com.axisnimble.theflightwall/Data`. Find the store (likely
   AsyncStorage or Keychain) and document the extraction step; do not commit the values.
