# FlightWall Mini API

The commercial FlightWall app has no public API. This is the contract as observed from the
owner's own app (`TheFlightWall.app` iOS 3.0.0, Expo 54, running on macOS 26.7) talking to its
backend on 2026-09-22, then probed from the shell with the captured credentials. Every claim
below traces to a sanitized fixture in `tests/fixtures/flightwall/` or to a probe recorded in
§9. Nothing is inferred from the vendor's open-source ESP32 project, which shares a name and
nothing else.

**This is a private, undocumented contract.** The vendor may change it at any time. The
client (`src/flighty_wall/flightwall.py`) checks the fingerprint in §7 on every read and
refuses to write when it changes; when that happens, re-run the capture in `docs/capture.md`
and update this document.

---

## 1. Transport

| Property | Value |
| --- | --- |
| Host | `api.theflightwall.com` — the only host any configuration operation touched |
| Scheme | HTTPS, HTTP/2 offered; the client uses HTTP/1.1 |
| Edge | Cloudflare (`cf-ray`, `cf-cache-status` on every response) |
| Backend marker | `x-fw-backend` response header, present on every response |
| Redirects | None observed. The client refuses to follow any |
| Rate limit | None observed at ~55 requests in an afternoon or ~15 in 30 s |

**`user-agent` is required.** A request without an app-shaped user-agent gets Cloudflare
`403` error 1010 (`browser_signature_banned`, `retryable: false`). The value the app sends
and the client reuses:

```
TheFlightWall/1 CFNetwork/3860.700.1 Darwin/25.6.0
```

Other hosts seen from the app but **not part of this contract**: `cdn.theflightwall.com`
(layout example images), `plus.theflightwall.com`, `wvlaidatdjufalntsqsw.supabase.co`, and
`accounts.google.com` (all sign-in / Plus subscription; never contacted once signed in), and
`192.168.4.1` (the wall's own setup hotspot during first provisioning).

---

## 2. Authentication

Two headers on every request:

| Header | Value | Role |
| --- | --- | --- |
| `x-api-key` | 43-character opaque string, per install | **The only credential.** |
| `x-user-id` | 29-character string, `fw_ios_` + 22 chars, per install | Decorative. Sent by the app; authorizes nothing. |

Probed (§9): a wrong or missing `x-user-id` on GET returns the same document; a wrong
`x-user-id` and wrong body `userId` on POST still applies the write. A wrong `x-api-key`
returns `401`.

A second, 29-character `x-api-key` appears on `/feature-flags/app` and `/messages/app` only.
It is an app-level key, not per install, and the daemon does not need it.

**Lifetime.** The per-install key survived ~7 hours idle and a sign-out / sign-in cycle in the
app. It is bound to the install, not the session. Uninstalling and reinstalling the app is
expected to mint a new pair (not tested).

**Where it lives.** `x-user-id` is in the app's AsyncStorage as `fw_user_id`. The 43-character
key is **not** in AsyncStorage, NSUserDefaults, or the login keychain; the Hermes bundle is
bytecode and the derivation was not recovered. The daemon obtains it once from a capture
(`mise run capture:credentials`). This is the one open item in this contract.

---

## 3. The configuration resource

There is one resource. Everything the app shows and controls is one JSON document.

```
GET  /configuration     → 200, document
POST /configuration     → 200, document + meta
```

### 3.1 Document shape

Top-level keys on GET: `display_config`, `request_config`, `version`. POST responses add
`meta`. POST requests add `userId` (same value as `x-user-id`).

```jsonc
{
  "display_config": {
    "model": "mini-v1",                 // fingerprint, §7
    "layout_template": "TECHNICAL",
    "loading_template": "CLOCK",
    "max_flights_behavior": "text",
    "brightness_percent": 70,
    "border_enable": false,
    "border_brightness_percent": 100,
    "hide_max_flights_screen": false,
    "clock": { "style": "DEFAULT", "show_date": true, "additional_timezones": null },
    "clock_is_24hr": true,
    "clock_dst_enabled": true,
    "clock_utc_offset_minutes": 0,
    "time_zone": { "dst_rule": "europe", "standard_utc_offset_minutes": 0 },
    "sleep_time": { "start": "22:00", "end": "07:00", "utc_offset_minutes": 60 },
    "metric_display": { "alt_b_unit": "meters", "dist_unit": "meters", "trk_unit": "deg",
                        "vel_unit": "kmh", "vr_unit": "kmh" },
    "anchor_airports": []
  },
  "request_config": {
    "radius_request": {                 // the owner's home area — contains coordinates
      "id": "<uuid>", "name": "Home", "type": "radius",
      "latitude": <float>, "longitude": <float>, "radius_km": 7.6,
      "min_altitude": null, "max_altitude": null
    },
    "radius_requests": null,
    "geo_request": null,
    "geo_requests": null,
    "data_filters": null,
    "tracked_flights": [                // §4 — the only part the daemon writes
      { "flight_number": "EI61", "created_at": "2026-09-21T10:49:06.351Z",
        "show_distance_travelled": true, "show_metrics": true }
    ]
  },
  "version": 2
}
```

Everything outside `tracked_flights` is the owner's display and area-tracking configuration.
**A POST replaces the whole document**, so a client that changes tracked flights must send
every other byte back exactly as read. The client does this by deep-copying the GET response
and mutating only `request_config.tracked_flights`; the test
`test_replace_preserves_every_non_tracked_byte_of_the_document` pins it.

### 3.2 POST response `meta`

```json
{ "meta": { "savedAtEpochMs": 1790072518713, "version": 1 } }
```

`meta.version` was `1` on every write observed. It does not track the document's `version`.

---

## 4. Tracked flights

`request_config.tracked_flights` is an array of at most five (see §5) entries:

| Field | Type | Notes |
| --- | --- | --- |
| `flight_number` | string | Stored as typed by the user: `EI61`, `BA5`. No zero-padding, no space. Case not tested. This is the only identity an entry has. |
| `created_at` | string | RFC 3339 with milliseconds and `Z`, e.g. `2026-09-22T10:49:06.351Z`. Set by the client; the server stores it. |
| `show_distance_travelled` | bool | App default `true` |
| `show_metrics` | bool | App default `true` |

There is **no per-entry id, revision, actor, or source**. A flight added by the app and one
added by this daemon are indistinguishable on the server. Any notion of "who added this" has
to live in the client — the daemon keeps a journal (`owned_flights` in its SQLite state) and
only ever removes `flight_number`s it recorded adding.

**Add** = POST the document with the new entry appended. **Remove** = POST the document with
the entry omitted. **Reschedule** = there is no schedule field; nothing to do unless the
flight number changes.

Order is preserved as sent. The app appends new entries.

---

## 5. Capacity

The app hides the "Add" field at five entries and the vendor FAQ says the Mini displays up to
five. **The server does not enforce this.** POSTing six and ten entries returned `200` and
the entries were stored and echoed back on the next GET (§9). What the wall *displays* with
more than five is undefined and was not tested on the owner's device.

The daemon enforces five itself (`MAX_TRACKED_FLIGHTS`) and refuses to POST more, before any
request is made.

---

## 6. Write semantics

**Last-writer-wins on the whole document. No conditional writes.**

- `version` is decorative. POST with `version: 1`, `version: 99`, and no `version` key all
  returned `200` and applied. The server stores whatever it is sent (GET returned `99` after
  the `99` write) and defaults to `2` when the key is absent.
- No `ETag`, no `If-Match`, no `Last-Modified` on the resource.
- Two clients writing concurrently: the later POST silently replaces the earlier. The window
  between a client's GET and its POST is the race. The daemon keeps it to one read and one
  write per cycle and re-reads after every write.

**Interrupted writes** (§9, sequence 12):

| Client sends | Then | Server state |
| --- | --- | --- |
| Full request incl. body, closes before reading the response | — | **Write applied** |
| Headers + half the body, closes | — | Write not applied; document intact and parseable |

So a transport failure on POST has an unknown outcome and must be settled by re-reading and
comparing, never by blind retry. POST is idempotent on content, so re-sending the same
document is a safe recovery once the comparison says it did not apply.

**Server-side removal.** The server removes tracked flights that have landed, on its own,
with no client involved. `EI61` (DUB→SFO, departed 12:00) was present at 11:15 and gone by
17:58 with the app closed. The app labels this "Will auto-remove". A client will find entries
it added missing without having removed them; that is normal, not drift.

The app's "tracking history" list (last five added) is client-only AsyncStorage
(`fw_tracked_flight_history`) and is not in the API.

---

## 7. Fingerprint

The client treats a document as this contract only if all three hold:

| Check | Expected |
| --- | --- |
| `display_config.model` | `"mini-v1"` |
| Top-level key set | `{display_config, request_config, version}` on GET, plus `meta` after a POST |
| `tracked_flights[]` key set | `{flight_number, created_at, show_distance_travelled, show_metrics}` (checked only when the list is non-empty) |

Any mismatch makes the read non-authoritative with reason
`flightwall_contract_drift:<field>`, and the client refuses to write. This is deliberately
narrow: a new optional key inside `display_config` does not trip it, because the daemon passes
that through untouched anyway; a new key in a tracked-flight entry does, because the daemon
constructs those.

`x-fw-backend` is present on every response and is a candidate for a stronger fingerprint;
its value has not been compared across days.

---

## 8. Errors

| Condition | Status | Body | Client treatment |
| --- | --- | --- | --- |
| Wrong `x-api-key` | `401` | `{"success": false, "errors": [{"code": 1102, "message": "Invalid API key"}]}` | `flightwall_credentials_rejected:1102`. Stop; never retry; never log the key |
| Missing `x-api-key` | `401` | same shape, `"code": 1101, "message": "Missing API key"` | `flightwall_credentials_rejected:1101` |
| Missing / non-app `user-agent` | `403` | Cloudflare problem+json: `error_code: 1010`, `error_name: "browser_signature_banned"`, `retryable: false`, `cloudflare_error: true` | `flightwall_blocked:cloudflare_1010`. Fatal misconfiguration |
| Wrong `x-user-id` | `200` | normal document | Not an error |
| Bad `version` | `200` | normal document | Not an error |
| Six or more entries | `200` | normal document | Not an error server-side; the client refuses to send it |
| Rate limit | not observed | — | Client maps `429` to `flightwall_rate_limited` if it ever appears |
| Server error | not observed | — | Client maps `5xx` to `flightwall_server_error:<status>` |

The full reason vocabulary is `flightwall.WallFailure`.

---

## 9. Probe log

All probes ran 2026-09-22 from the shell with the captured key pair; the wall was returned to
`tracked_flights = []` afterwards (the owner's `EI61` had already been removed by the server on
landing before the probes began).

| # | Probe | Result |
| --- | --- | --- |
| 1 | Cold start via the app | No auth round-trip; the app presents the stored key pair. Calls: `GET /feature-flags/app`, `GET /messages/app`, `GET /plus/sync` (polled ~30 s), `GET /configuration` on device screen |
| 2–3 | Read list, read mode | One `GET /configuration`. There is no mode: area tracking and tracked flights coexist ("Tracked flights will show regardless of area settings") |
| 4–5 | Add one, add a second (app) | `POST /configuration`, full document, `['EI61','BA5']` echoed |
| 6 | Manual vs API-added | Indistinguishable |
| 7 | Remove one (app) | `POST` with the entry omitted; `['EI61']` echoed |
| 9 | Capacity (shell) | POST 5 → stored. POST 6 → stored. POST 10 → stored. No server cap |
| 12 | Interrupted POST (shell) | Full body then close → applied. Half body then close → not applied |
| 13 | Post-landing | `EI61` removed server-side, app closed |
| 14 | Key lifetime | Works after ~7 h and after sign-out / sign-in |
| — | `version: 1 / 99 / absent` | All `200`, all applied; stored as sent; defaults to `2` |
| — | Wrong / missing `x-user-id` | `200` on GET and POST |
| — | Wrong / missing `x-api-key` | `401` codes 1102 / 1101 |
| — | Missing `user-agent` | `403` Cloudflare 1010 |

---

## 10. Other endpoints

Observed from the app; not used by the daemon.

| Method | Path | Auth | Body | Note |
| --- | --- | --- | --- | --- |
| `GET` | `/feature-flags/app` | app-level `x-api-key` | `[{feature_flag_id, enabled, disabled_message, state_changed_at}]` | Observed `individual_flight_tracking: true`, `plus_sync: false`. A reasonable pre-flight check if the vendor ever disables tracking |
| `GET` | `/messages/app` | app-level key | `[]` | In-app announcements |
| `GET` | `/plus/sync` | per-install key | `{"status": "none"}` | Plus subscription poll, every ~30 s |

Paths seen in the bundle but never observed on the wire: `/device/`, `/device-setup`,
`/devices/firmware-version`, `/authentication/options`, `/authentication/verifyOtp`,
`/token`. These likely cover first-time device pairing and account sign-in.

---

## 11. Fixtures

| File | Captured | App | Proves |
| --- | --- | --- | --- |
| `tests/fixtures/flightwall/get-configuration.json` | 2026-09-22 | 3.0.0 / iOS-on-macOS 26.7 arm64 | The document shape; `tracked_flights = [EI61]`; `version = 2` |
| `tests/fixtures/flightwall/post-configuration-add.json` | 2026-09-22 | as above | Whole-document POST with `[EI61, BA5]`; `200`; `meta` in response |
| `tests/fixtures/flightwall/post-configuration-remove.json` | 2026-09-22 | as above | Same call with `[EI61]` |
| `tests/fixtures/flightwall/get-feature-flags.json` | 2026-09-22 | as above | App-level pre-flight read |

Sanitized by `flighty-wall sanitize-capture`: every header and query value dropped, body
values redacted by key (`userId`, `latitude`, `longitude`, `id`, and the usual credential
names) and by pattern. Scanned against the raw HAR for both API keys, the user id, the radius
id, and the home coordinates: zero hits. `tests/fixtures/flightwall/README.md` has the rules.

---

## 12. Open items

1. **Key derivation.** How the app mints the 43-character `x-api-key`. Only matters if the
   captured key ever stops working; then either decompile the Hermes bundle (`hermes-dec`) or
   re-capture.
2. **Display above five.** What the wall shows when the server holds six or more entries.
   Never test on the owner's wall; the daemon refuses to send it.
3. **`flight_number` case sensitivity.** Untested. The daemon sends upper-case designators
   as Flighty exports them.
4. **`x-fw-backend` as a fingerprint.** Value not yet compared across days.
