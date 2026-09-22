# FlightWall fixtures

Every fixture here comes from a real, authorized capture of `TheFlightWall.app` (iOS 3.0.0,
running on the owner's Apple Silicon Mac) talking to `api.theflightwall.com`, taken on
2026-09-22 with the owner's own account and wall. Nothing may be written by hand or guessed
from a plausible REST shape — an invented fixture would let U5 build a client against a
contract that does not exist, and the first real request would fail on the wall rather than
in a test.

See `docs/flightwall-api-discovery.md` for the contract, the capture protocol, and what is
still open.

## What is here

| File | Proves |
| --- | --- |
| `get-configuration.json` | An authoritative read: the whole configuration document, `tracked_flights = [EI61]`, `version = 2` |
| `post-configuration-add.json` | Adding one flight: the full document with `[EI61, BA5]` POSTed, `200`, echoed with `meta` |
| `post-configuration-remove.json` | Removing one flight: the full document with `[EI61]` POSTed, `200`. Same call as add |
| `get-feature-flags.json` | The app-level pre-flight read: `individual_flight_tracking` enabled |

There is no `list-empty`, `mode-*`, or `remove-already-gone` fixture, because the contract
has no separate list call, no mode, and no per-entry delete: the daemon GETs the document,
edits `request_config.tracked_flights`, and POSTs it back.

Still to capture (see the discovery document §8): the server's response to a six-entry
document, an interrupted POST, a stale `version`, and post-landing behaviour.

## Producing fixtures

```bash
mise run capture:start          # proxy + CA; Ctrl-C writes captures/flightwall.har
# ... drive the app ...
mise run capture:stop           # proxy off, CA removed
mise run capture:sanitize -- --host api.theflightwall.com
```

Run `capture:sanitize` once with no `--host` to list every host the capture touched. Then
rename each numbered output file to the operation it proves, read it by hand, and delete the
raw capture with `mise run capture:stop --purge`.

## What the sanitizer removes

- **All header values and all query-string values.** Only names and keys survive. An
  unrecognised authorization scheme is exactly the case a pattern-based scrubber misses,
  so nothing is trusted to a pattern here.
- **Body values by key**, for keys whose contents are never safe: tokens, secrets,
  passwords, signatures, session / device / user / account identifiers, email, phone, serial
  numbers, and latitude/longitude. The key stays so the contract shape is readable; the
  value becomes `<redacted>`.
- **Body and path values by pattern**: emails, URIs of any scheme, JWTs, UUIDs,
  `bearer`/`token`/`secret`-style credentials, opaque tokens of 32 characters or more,
  booking codes, seat numbers, and coordinates with four or more decimal places.
- **Bodies that are oversized or not JSON** are reduced to a size and MIME type.

The 2026-09-22 capture found one gap: the POST body's `userId` (a 29-character opaque
string) survived the first pass. `userid` / `user_id` / `device_id` / `account_id` were added
to the key list with a regression test, and the fixtures were regenerated and re-scanned.

## What must never appear in a committed fixture

- The `x-api-key` or `x-user-id` header values, or the `userId` body value
- `radius_request.id`, `latitude`, or `longitude` — the owner's home
- A device serial, MAC address, or push token
- A Friend's name, email address, or phone number
- Anything from the interception CA

The committed fixtures were scanned against the raw HAR for every one of these: zero hits.

## Provenance

Every fixture has a row in §6 of `docs/flightwall-api-discovery.md` recording capture date,
app version, platform, and which fields were removed beyond the sanitizer's defaults. A
fixture with no provenance row cannot be trusted later.
