# Google Calendar fixtures

Fixtures in this directory must be produced by the sanitizer before they are committed.
They must not contain Friend names, email addresses, booking references, seat numbers,
calendar links, Flighty deeplinks, event UUIDs, OAuth material, service-account keys, or
raw event identifiers.

## Producing a fixture

```bash
.venv/bin/flighty-wall inspect-calendar \
  --config config.toml \
  --output tests/fixtures/google_calendar/friend-flight.json \
  --lookahead-days 60 --lookback-days 3 \
  --redact-term "Friend Name"
```

Pass one `--redact-term` per Friend whose name appears in the window. The sanitizer removes
emails, URIs of any scheme, UUIDs, booking codes and seats automatically, and drops
`attachments`, `attendees`, `conferenceData`, `etag`, `hangoutLink`, `htmlLink` and
`iCalUID`; Friend names are only removed when supplied explicitly. Verify the result
contains no Friend name before committing.

## Observed Flighty export format

`friend-flight.json` is a real capture of two Friends' flights (one recently departed, one
a month out). The fields the parser depends on:

- `summary` — `"<Friend>: ✈ DUB​→​BCN • VY 8721"`. The route arrow is wrapped
  in zero-width spaces (`U+200B`) and the carrier code is separated from the flight number
  by a non-breaking space (`U+00A0`). Both must be normalised before parsing.
- `description` — airline and number, `"<Origin> to <Destination>"`, `↗`/`↘` local times
  with abbreviated or `GMT±N` zones, and a `Flight time` line. Arrival times may carry a
  `+N` day offset relative to departure.
- `start` / `end` — `dateTime` plus `timeZone`, both in the origin/destination local zone.
- `location` — origin airport or city name, not an IATA code.
- `status` is `confirmed`; `transparency` is `transparent`; `eventType` is `default`.

Flight numbers are not zero-padded (`BA 5`, not `BA0005`), so the FlightWall
identifier format must be confirmed by the U4 capture before it is assumed.
