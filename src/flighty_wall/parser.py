"""Deterministic interpretation of Flighty calendar exports into physical flights.

The parser never guesses. Every event gets an explicit outcome, and any event that
looks like a Flighty export but does not match a known shape makes the whole cycle
non-authoritative so that no downstream component mutates the wall from a partial
understanding of the calendar.

Failure reasons carry the offending Google event id but never event text, so an
operator can find the event in their own calendar without names, routes, or
reservation details reaching the logs.
"""

from __future__ import annotations

import re
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import UTC
from enum import StrEnum
from typing import TYPE_CHECKING, cast

from .models import SnapshotAuthority

if TYPE_CHECKING:
    from collections.abc import Iterable, Sequence
    from datetime import datetime

    from .models import Snapshot, SourceEvent


class ParseOutcome(StrEnum):
    """What the parser concluded about a single calendar event."""

    FLIGHT = "flight"
    CANCELLED = "cancelled"
    ALL_DAY = "all_day"
    MISSING_DEPARTURE = "missing_departure"
    NOT_A_FLIGHT = "not_a_flight"
    UNRECOGNIZED = "unrecognized"


@dataclass(frozen=True, slots=True)
class FlightIdentity:
    """The normalized identity of one physical flight leg."""

    carrier: str
    number: str
    origin: str
    destination: str
    scheduled_departure: datetime

    @property
    def designator(self) -> str:
        """Return the carrier code and flight number with no separator."""
        return f"{self.carrier}{self.number}"

    @property
    def route(self) -> str:
        """Return the IATA route as plain ASCII, safe for logs and wall labels."""
        return f"{self.origin}-{self.destination}"

    @property
    def key(self) -> str:
        """Return a stable key: the same leg keeps its key across delays on the day."""
        departure_day = self.scheduled_departure.astimezone(UTC).date().isoformat()
        return f"{self.designator}:{self.origin}:{departure_day}"


@dataclass(frozen=True, slots=True)
class DesiredFlight:
    """One physical flight the wall should track, with every event that asked for it."""

    key: str
    carrier: str
    number: str
    origin: str
    destination: str
    scheduled_departure: datetime
    source_event_ids: tuple[str, ...]

    @property
    def designator(self) -> str:
        """Return the carrier code and flight number with no separator."""
        return f"{self.carrier}{self.number}"

    @property
    def route(self) -> str:
        """Return the IATA route as plain ASCII, safe for logs and wall labels."""
        return f"{self.origin}-{self.destination}"


@dataclass(frozen=True, slots=True)
class EventInterpretation:
    """The explicit parse result for one source event."""

    event_id: str
    outcome: ParseOutcome
    identity: FlightIdentity | None = None
    updated_at: datetime | None = None


@dataclass(frozen=True, slots=True)
class ParsedCycle:
    """Desired flights for one poll, or an explicit record of why parsing failed."""

    authority: SnapshotAuthority
    flights: tuple[DesiredFlight, ...] = ()
    interpretations: tuple[EventInterpretation, ...] = ()
    reason: str | None = None


@dataclass(frozen=True, slots=True)
class _Contribution:
    """One event's claim on a flight key, kept with the freshness used to break ties."""

    event_id: str
    identity: FlightIdentity
    updated_at: datetime | None = None


def parse_cycle(snapshot: Snapshot) -> ParsedCycle:
    """Turn one authoritative snapshot into the set of flights the wall should track."""
    if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        return ParsedCycle(authority=snapshot.authority, reason=snapshot.reason)

    interpretations = tuple(_interpret(event) for event in snapshot.events)

    unrecognized = next(
        (item for item in interpretations if item.outcome is ParseOutcome.UNRECOGNIZED),
        None,
    )
    if unrecognized is not None:
        return _failed(
            interpretations,
            f"parse_flighty_event_unrecognized:{unrecognized.event_id}",
        )

    flights = _desired_flights(interpretations)
    codeshare = _codeshare_conflict(flights)
    if codeshare is not None:
        return _failed(interpretations, f"parse_codeshare_ambiguous:{codeshare}")

    return ParsedCycle(
        authority=SnapshotAuthority.AUTHORITATIVE,
        flights=flights,
        interpretations=interpretations,
    )


def _interpret(event: SourceEvent) -> EventInterpretation:
    if event.status.casefold() == "cancelled":
        return EventInterpretation(event.event_id, ParseOutcome.CANCELLED)

    summary = _normalize(event.summary)
    if not _is_flighty(event, summary):
        return EventInterpretation(event.event_id, ParseOutcome.NOT_A_FLIGHT)

    if _CANCELLATION.search(summary):
        return EventInterpretation(event.event_id, ParseOutcome.CANCELLED)

    match = _FLIGHT_SUMMARY.match(summary)
    if match is None:
        return EventInterpretation(event.event_id, ParseOutcome.UNRECOGNIZED)

    number = match["number"].lstrip("0")
    if not number:
        return EventInterpretation(event.event_id, ParseOutcome.UNRECOGNIZED)

    if event.starts_at is None:
        missing = ParseOutcome.ALL_DAY if _is_all_day(event) else ParseOutcome.MISSING_DEPARTURE
        return EventInterpretation(event.event_id, missing)

    return EventInterpretation(
        event_id=event.event_id,
        outcome=ParseOutcome.FLIGHT,
        identity=FlightIdentity(
            carrier=match["carrier"].upper(),
            number=number,
            origin=match["origin"].upper(),
            destination=match["destination"].upper(),
            scheduled_departure=event.starts_at.astimezone(UTC),
        ),
        updated_at=event.updated_at,
    )


def _desired_flights(interpretations: Iterable[EventInterpretation]) -> tuple[DesiredFlight, ...]:
    grouped: dict[str, list[_Contribution]] = {}
    for item in interpretations:
        if item.outcome is not ParseOutcome.FLIGHT or item.identity is None:
            continue
        contribution = _Contribution(
            event_id=item.event_id,
            identity=item.identity,
            updated_at=item.updated_at,
        )
        grouped.setdefault(item.identity.key, []).append(contribution)

    flights = [_merge(key, contributions) for key, contributions in grouped.items()]
    return tuple(sorted(flights, key=lambda flight: (flight.scheduled_departure, flight.key)))


def _merge(key: str, contributions: Sequence[_Contribution]) -> DesiredFlight:
    """Collapse every event that named one flight, preferring the freshest departure."""
    freshest = max(contributions, key=_freshness)
    identity = freshest.identity
    return DesiredFlight(
        key=key,
        carrier=identity.carrier,
        number=identity.number,
        origin=identity.origin,
        destination=identity.destination,
        scheduled_departure=identity.scheduled_departure,
        source_event_ids=tuple(sorted(item.event_id for item in contributions)),
    )


def _freshness(contribution: _Contribution) -> tuple[float, str]:
    updated_at = contribution.updated_at
    stamp = updated_at.timestamp() if updated_at is not None else float("-inf")
    return (stamp, contribution.event_id)


def _codeshare_conflict(flights: Sequence[DesiredFlight]) -> str | None:
    """Return the conflicting designators when one leg is claimed by two flight numbers."""
    by_leg: dict[tuple[str, str, datetime], set[str]] = {}
    for flight in flights:
        leg = (flight.origin, flight.destination, flight.scheduled_departure)
        by_leg.setdefault(leg, set()).add(flight.designator)
    for leg in sorted(by_leg):
        designators = by_leg[leg]
        if len(designators) > 1:
            return ",".join(sorted(designators))
    return None


def _failed(interpretations: tuple[EventInterpretation, ...], reason: str) -> ParsedCycle:
    return ParsedCycle(
        authority=SnapshotAuthority.NON_AUTHORITATIVE,
        interpretations=interpretations,
        reason=reason,
    )


def _is_flighty(event: SourceEvent, summary: str) -> bool:
    """Decide whether an event must parse as a flight or may be ignored.

    The plane glyph stays a marker on purpose: if Flighty ever drops its description
    footer, a Friends' flight still fails the cycle loudly instead of vanishing from
    the wall without a trace.
    """
    description = event.fields.get("description")
    haystack = summary
    if isinstance(description, str):
        haystack = f"{haystack}\n{_normalize(description)}"
    folded = haystack.casefold()
    return any(marker in folded for marker in _FLIGHTY_MARKERS)


def _is_all_day(event: SourceEvent) -> bool:
    start = event.fields.get("start")
    if not isinstance(start, Mapping):
        return False
    boundary = cast("Mapping[str, object]", start)
    return isinstance(boundary.get("date"), str)


def _normalize(text: str) -> str:
    """Fold the export's non-breaking and zero-width characters into plain spacing."""
    return _WHITESPACE.sub(" ", text.translate(_TRANSLATION)).strip()


_FLIGHTY_MARKERS = ("synced by flighty", "flighty.app", "flighty://", "✈")

_TRANSLATION: dict[int, str | None] = {
    0x00A0: " ",  # no-break space, used between carrier code and flight number
    0x2002: " ",
    0x2003: " ",
    0x2007: " ",
    0x2009: " ",
    0x202F: " ",
    0x200B: None,  # zero-width space, used either side of the route arrow
    0x200C: None,
    0x200D: None,
    0xFEFF: None,
}

_WHITESPACE = re.compile(r"\s+")
_CANCELLATION = re.compile(r"\bcancell?ed\b", re.IGNORECASE)
_ARROW = r"(?:→|➡|➙|->)"
_FLIGHT_SUMMARY = re.compile(
    rf"""
    ^
    (?:[^:]{{1,120}}:\s*)?        # optional traveller label, deliberately discarded
    (?:✈\s*)?                # optional plane glyph
    (?P<origin>[A-Z]{{3}})
    \s*{_ARROW}\s*
    (?P<destination>[A-Z]{{3}})
    \s*[•·|]\s*         # bullet between route and designator
    (?P<carrier>[A-Z0-9]{{2,3}})
    \s+                           # a separator is required; without one the event is ambiguous
    (?P<number>\d{{1,4}})
    \s*$
    """,
    re.VERBOSE,
)
