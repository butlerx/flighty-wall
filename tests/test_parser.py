"""Characterization tests for Flighty calendar parsing, driven by sanitized fixtures."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast

import orjson

from flighty_wall.calendar import CalendarLimits, CalendarReader
from flighty_wall.models import Snapshot, SnapshotAuthority
from flighty_wall.parser import ParseOutcome, parse_cycle

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence

    from flighty_wall.parser import DesiredFlight

FIXTURES = Path(__file__).parent / "fixtures" / "google_calendar"

# The real export separates the carrier code from the flight number with U+00A0 and
# brackets the route arrow with U+200B. Every special character below is built with
# chr() so the literals stay plain ASCII and no invisible character hides in this file.
NBSP = chr(0x00A0)
ZWSP = chr(0x200B)
PLANE = chr(0x2708)
ARROW = chr(0x2192)
BULLET = chr(0x2022)
FRIEND_DESIGNATOR = f"VY{NBSP}8721"
FRIEND_SUMMARY = f"<redacted-name>: {PLANE} DUB{ZWSP}{ARROW}{ZWSP}BCN {BULLET} {FRIEND_DESIGNATOR}"


class StubGateway:
    """Serve fixture events as a single complete calendar page."""

    def __init__(self, events: Sequence[Mapping[str, Any]]) -> None:
        self.events = list(events)

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, Any]:
        return {"items": self.events}


def load_fixture_events(name: str) -> list[dict[str, Any]]:
    payload = cast("dict[str, Any]", orjson.loads((FIXTURES / name).read_bytes()))
    assert payload["authority"] == "authoritative"
    events = payload["events"]
    assert isinstance(events, list)
    return cast("list[dict[str, Any]]", events)


def snapshot_of(events: Sequence[Mapping[str, Any]]) -> Snapshot:
    """Build a snapshot through the real reader so parsing sees production shapes."""
    reader = CalendarReader(
        gateway=StubGateway(events),
        calendar_id="friends@example.invalid",
        lookahead_days=60,
        lookback_days=3,
        limits=CalendarLimits(
            max_pages=10,
            max_events=500,
            max_field_chars=8_192,
            max_snapshot_bytes=1_048_576,
        ),
    )
    return reader.read_snapshot(datetime(2026, 9, 21, 6, 0, tzinfo=UTC))


def friend_event(
    event_id: str,
    *,
    summary: str = FRIEND_SUMMARY,
    start: str = "2026-09-21T09:55:00+01:00",
    status: str = "confirmed",
    description: str = f"Vueling{NBSP}8721\nDublin to Barcelona\n\nSynced by Flighty\nwww.flighty.app",
) -> dict[str, Any]:
    return {
        "id": event_id,
        "status": status,
        "summary": summary,
        "description": description,
        "start": {"dateTime": start, "timeZone": "Europe/Dublin"},
        "end": {"dateTime": "2026-09-21T13:35:00+02:00", "timeZone": "Europe/Madrid"},
        "updated": "2026-09-20T18:00:00Z",
    }


def outcomes(snapshot: Snapshot) -> dict[str, ParseOutcome]:
    cycle = parse_cycle(snapshot)
    return {item.event_id: item.outcome for item in cycle.interpretations}


def first_flight(events: Sequence[Mapping[str, Any]]) -> DesiredFlight:
    return parse_cycle(snapshot_of(events)).flights[0]


def test_live_fixture_parses_into_two_distinct_flights() -> None:
    cycle = parse_cycle(snapshot_of(load_fixture_events("friend-flight.json")))

    assert cycle.authority is SnapshotAuthority.AUTHORITATIVE
    assert cycle.reason is None
    assert [flight.designator for flight in cycle.flights] == ["VY8721", "BA5"]
    assert [flight.route for flight in cycle.flights] == ["DUB-BCN", "LHR-HND"]
    assert cycle.flights[0].scheduled_departure == datetime(2026, 9, 21, 8, 55, tzinfo=UTC)
    assert cycle.flights[1].scheduled_departure == datetime(2026, 10, 24, 11, 40, tzinfo=UTC)
    assert all(item.outcome is ParseOutcome.FLIGHT for item in cycle.interpretations)


def test_parsed_flights_carry_no_friend_or_reservation_text() -> None:
    cycle = parse_cycle(snapshot_of(load_fixture_events("friend-flight.json")))

    rendered = repr(cycle.flights)

    assert "redacted-name" not in rendered
    assert "Vueling" not in rendered
    assert "Dublin" not in rendered
    assert "flighty" not in rendered.casefold()
    assert NBSP not in rendered
    assert ZWSP not in rendered


def test_flight_number_is_normalized_without_padding_or_separators() -> None:
    padded = friend_event(
        "event-padded",
        summary=FRIEND_SUMMARY.replace(FRIEND_DESIGNATOR, f"VY{NBSP}0005"),
    )

    cycle = parse_cycle(snapshot_of([padded]))

    assert cycle.flights[0].carrier == "VY"
    assert cycle.flights[0].number == "5"
    assert cycle.flights[0].designator == "VY5"


def test_two_friends_on_one_flight_aggregate_into_a_single_entry() -> None:
    cycle = parse_cycle(snapshot_of([friend_event("event-a"), friend_event("event-b")]))

    assert len(cycle.flights) == 1
    assert cycle.flights[0].source_event_ids == ("event-a", "event-b")


def test_removing_one_of_two_source_events_retains_the_flight() -> None:
    both = first_flight([friend_event("event-a"), friend_event("event-b")])
    remaining = first_flight([friend_event("event-a")])

    assert remaining.key == both.key
    assert remaining.source_event_ids == ("event-a",)


def test_daylight_saving_transition_is_resolved_by_the_events_own_offset() -> None:
    # 01:30 local occurs twice in Dublin on 25 October 2026; only the offset disambiguates.
    before = first_flight([friend_event("event-before", start="2026-10-25T01:30:00+01:00")])
    after = first_flight([friend_event("event-after", start="2026-10-25T01:30:00+00:00")])

    assert before.scheduled_departure == datetime(2026, 10, 25, 0, 30, tzinfo=UTC)
    assert after.scheduled_departure == datetime(2026, 10, 25, 1, 30, tzinfo=UTC)
    assert before.key == after.key


def test_departure_key_uses_the_utc_day_not_the_local_calendar_day() -> None:
    late = first_flight([friend_event("event-late", start="2026-09-22T00:30:00+01:00")])

    assert late.scheduled_departure == datetime(2026, 9, 21, 23, 30, tzinfo=UTC)
    assert late.key == "VY8721:DUB:2026-09-21"


def test_cancelled_event_contributes_no_source_reference() -> None:
    cancelled = load_fixture_events("cancelled-flight.json")

    cycle = parse_cycle(snapshot_of([friend_event("event-live"), *cancelled]))

    assert cycle.authority is SnapshotAuthority.AUTHORITATIVE
    assert [flight.source_event_ids for flight in cycle.flights] == [("event-live",)]
    assert outcomes(snapshot_of(cancelled))["event-b8cdee8f30dc"] is ParseOutcome.CANCELLED


def test_cancelled_flighty_event_does_not_fail_the_cycle_when_unparsable() -> None:
    marked = friend_event("event-cancelled", summary=f"<redacted-name>: {PLANE} Cancelled flight")

    cycle = parse_cycle(snapshot_of([marked]))

    assert cycle.authority is SnapshotAuthority.AUTHORITATIVE
    assert cycle.flights == ()
    assert cycle.interpretations[0].outcome is ParseOutcome.CANCELLED


def test_reschedule_within_the_day_updates_one_flight_key() -> None:
    original = first_flight([friend_event("event-a")])
    delayed = first_flight([friend_event("event-a", start="2026-09-21T14:20:00+01:00")])

    assert delayed.key == original.key
    assert delayed.scheduled_departure == original.scheduled_departure + timedelta(hours=4, minutes=25)


def test_reschedule_to_another_day_produces_a_new_flight_key() -> None:
    original = first_flight([friend_event("event-a")])
    moved = first_flight([friend_event("event-a", start="2026-09-23T09:55:00+01:00")])

    assert moved.key != original.key


def test_codeshare_duplicate_fails_the_cycle_instead_of_guessing_two_flights() -> None:
    codeshare = friend_event(
        "event-codeshare",
        summary=FRIEND_SUMMARY.replace(FRIEND_DESIGNATOR, f"IB{NBSP}5432"),
    )

    cycle = parse_cycle(snapshot_of([friend_event("event-a"), codeshare]))

    assert cycle.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert cycle.flights == ()
    assert cycle.reason is not None
    assert cycle.reason.startswith("parse_codeshare_ambiguous:")


def test_malformed_flight_number_fails_the_whole_cycle_closed() -> None:
    malformed = friend_event(
        "event-bad",
        summary=FRIEND_SUMMARY.replace(FRIEND_DESIGNATOR, f"VY{NBSP}87X1"),
    )

    cycle = parse_cycle(snapshot_of([friend_event("event-good"), malformed]))

    assert cycle.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert cycle.flights == ()
    assert cycle.reason == "parse_flighty_event_unrecognized:event-bad"


def test_failed_cycle_reason_contains_no_event_text() -> None:
    malformed = friend_event(
        "event-bad",
        summary=f"<redacted-name>: {PLANE} secret routing note {BULLET} VY{NBSP}87X1",
    )

    cycle = parse_cycle(snapshot_of([malformed]))

    assert cycle.reason is not None
    assert "secret routing note" not in cycle.reason
    assert "redacted-name" not in cycle.reason


def test_all_day_event_is_skipped_without_failing_the_cycle() -> None:
    all_day = friend_event("event-all-day")
    all_day["start"] = {"date": "2026-09-21"}
    all_day["end"] = {"date": "2026-09-22"}

    cycle = parse_cycle(snapshot_of([all_day]))

    assert cycle.authority is SnapshotAuthority.AUTHORITATIVE
    assert cycle.flights == ()
    assert cycle.interpretations[0].outcome is ParseOutcome.ALL_DAY


def test_event_without_any_start_is_skipped_without_failing_the_cycle() -> None:
    undated = friend_event("event-undated")
    del undated["start"]

    cycle = parse_cycle(snapshot_of([undated]))

    assert cycle.authority is SnapshotAuthority.AUTHORITATIVE
    assert cycle.flights == ()
    assert cycle.interpretations[0].outcome is ParseOutcome.MISSING_DEPARTURE


def test_missing_route_context_fails_the_cycle_closed() -> None:
    routeless = friend_event(
        "event-routeless",
        summary=f"<redacted-name>: {PLANE} {FRIEND_DESIGNATOR}",
    )

    cycle = parse_cycle(snapshot_of([routeless]))

    assert cycle.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert cycle.reason == "parse_flighty_event_unrecognized:event-routeless"


def test_non_flighty_event_is_ignored_and_keeps_the_cycle_authoritative() -> None:
    unrelated = {
        "id": "event-dentist",
        "status": "confirmed",
        "summary": "Dentist",
        "start": {"dateTime": "2026-09-21T09:00:00+01:00", "timeZone": "Europe/Dublin"},
        "end": {"dateTime": "2026-09-21T09:30:00+01:00", "timeZone": "Europe/Dublin"},
    }

    cycle = parse_cycle(snapshot_of([friend_event("event-a"), unrelated]))

    assert cycle.authority is SnapshotAuthority.AUTHORITATIVE
    assert [flight.designator for flight in cycle.flights] == ["VY8721"]
    assert outcomes(snapshot_of([unrelated]))["event-dentist"] is ParseOutcome.NOT_A_FLIGHT


def test_non_authoritative_snapshot_never_yields_flights() -> None:
    failed = Snapshot(
        authority=SnapshotAuthority.NON_AUTHORITATIVE,
        observed_at=datetime(2026, 9, 21, 6, 0, tzinfo=UTC),
        reason="calendar_request_failed:TimeoutError",
    )

    cycle = parse_cycle(failed)

    assert cycle.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert cycle.flights == ()
    assert cycle.interpretations == ()
    assert cycle.reason == "calendar_request_failed:TimeoutError"


def test_flight_order_is_deterministic_regardless_of_event_order() -> None:
    later = friend_event(
        "event-later",
        summary=FRIEND_SUMMARY.replace(FRIEND_DESIGNATOR, f"BA{NBSP}5"),
    )
    later["start"] = {"dateTime": "2026-10-24T12:40:00+01:00", "timeZone": "Europe/London"}
    earlier = friend_event("event-earlier")

    forward = parse_cycle(snapshot_of([earlier, later])).flights
    reverse = parse_cycle(snapshot_of([later, earlier])).flights

    assert [flight.key for flight in forward] == [flight.key for flight in reverse]
    assert [flight.designator for flight in forward] == ["VY8721", "BA5"]
