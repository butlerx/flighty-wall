# FlightWall fixtures

**This directory is empty on purpose.** Every fixture here has to come from a real
capture of the FlightWall app talking to its backend, taken by the owner of the wall on
the owner's own device and account. Nothing in this repository knows what the real
requests look like, so no fixture may be written by hand or guessed from a plausible REST
shape — an invented fixture would let U5 build a client against a contract that does not
exist, and the first real request would fail on the wall rather than in a test.

See `docs/flightwall-api-discovery.md` for the capture protocol. Run it, then produce the
fixtures with the command below.

## Producing fixtures

```bash
uv run flighty-wall sanitize-capture \
  --input captures/flightwall.har \
  --output-dir tests/fixtures/flightwall \
  --host api.example-flightwall-host \
  --redact-term "Friend Name" \
  --redact-term "My Wall"
```

To discover which hosts to allow, run it once with no `--host`: if nothing matches a given
allowlist, the command exits 1 and prints every host the capture touched. Pass one
`--redact-term` per Friend name, device label, or other literal string that only you can
recognise. Then rename each numbered output file to the operation it proves (see naming
below) and delete the entries that are not needed.

`captures/` is gitignored. Delete the raw HAR as soon as the fixtures are written.

## What the sanitizer removes

- **All header values and all query-string values.** Only names and keys survive. An
  unrecognised authorization scheme is exactly the case a pattern-based scrubber misses,
  so nothing is trusted to a pattern here.
- **Body values by key**, for keys whose contents are never safe: tokens, secrets,
  passwords, signatures, session and device identifiers, email, phone, serial numbers, and
  latitude/longitude. The key name stays so the contract shape is still readable; the value
  becomes `<redacted>`. Nested structure under such a key is discarded too.
- **Body and path values by pattern**: emails, URIs of any scheme, JWTs, UUIDs,
  `bearer`/`token`/`secret`-style credentials, opaque tokens of 32 characters or more,
  booking codes, seat numbers, and coordinates with four or more decimal places.
- **Bodies that are oversized or not JSON** are reduced to a size and MIME type.

Caller-supplied `--redact-term` values are the only defence against a Friend's name, so
supply them. The sanitizer cannot recognise a name on its own.

## What must never appear in a committed fixture

- A usable token, session, credential, or authorization header value of any scheme
- A device identifier, serial number, MAC address, or push token
- Your home coordinates, or any coordinate precise enough to locate a person
- A Friend's name, email address, or phone number
- An account identifier, subscription ID, or billing detail
- Anything from the interception CA, including its key or fingerprint

Read every file before committing. The sanitizer is deliberately over-broad, but it is a
second line of defence, not the first.

## Naming

One file per proven operation, named for the operation rather than the capture order:

| File | Proves |
| --- | --- |
| `list-empty.json` | An authoritative read of a wall with no tracked flights |
| `list-with-manual-and-tracked.json` | A read distinguishing a manually added flight from a synced one |
| `add-success.json` | Adding one flight, and the identifier the wall returns for it |
| `remove-success.json` | Removing exactly one flight by its identifier |
| `mode-area.json` | Reading and setting area tracking mode |
| `mode-tracking.json` | Reading and setting flight tracking mode |

Add further files as the capture requires; error responses (`add-conflict.json`,
`add-at-capacity.json`, `remove-already-gone.json`) are as valuable as successes, because
U5 has to handle them without guessing.

## Provenance

Every fixture needs a matching row in the provenance table in
`docs/flightwall-api-discovery.md`, recording the capture date, the app version and
platform, the firmware version if the app reports one, and which fields were removed
beyond the sanitizer's defaults. A fixture with no provenance row cannot be trusted later,
because there is no way to tell whether the contract has since changed.
