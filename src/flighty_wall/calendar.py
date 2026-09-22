"""Bounded Google Calendar reads and privacy-safe fixture sanitization."""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import TYPE_CHECKING, Protocol, cast

import orjson

from .models import Snapshot, SnapshotAuthority, SourceEvent
from .redaction import as_mapping, scrub_mapping

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence


class CalendarServiceRequest(Protocol):
    """A prepared Google API request that has not been sent yet."""

    def execute(self) -> Mapping[str, object]:
        """Send the request and return the decoded response body."""
        ...


class CalendarEventsResource(Protocol):
    """The `events` collection of the Calendar v3 service."""

    def list(self, **parameters: object) -> CalendarServiceRequest:
        """Build an `events.list` request from the given query parameters."""
        ...


class CalendarService(Protocol):
    """The subset of the discovery-built Calendar service this package uses."""

    def events(self) -> CalendarEventsResource:
        """Return the `events` collection."""
        ...


class CalendarGateway(Protocol):
    """One page of calendar reads, expressed without any Google types."""

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, object]:
        """Read one page of events in `[time_min, time_max]`."""
        ...


@dataclass(frozen=True, slots=True)
class CalendarLimits:
    """Hard caps that make an oversized calendar response non-authoritative."""

    max_pages: int
    max_events: int
    max_field_chars: int
    max_snapshot_bytes: int


class CalendarDataError(ValueError):
    """Raised when a Calendar response cannot be treated as authoritative."""


class GoogleCalendarGateway:
    """Small typed boundary around the dynamic Google discovery client."""

    def __init__(self, service: CalendarService) -> None:
        self.service = service

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, object]:
        """Read one page of single, time-ordered events including cancellations."""
        request = self.service.events().list(
            calendarId=calendar_id,
            timeMin=_rfc3339(time_min),
            timeMax=_rfc3339(time_max),
            singleEvents=True,
            orderBy="startTime",
            showDeleted=True,
            pageToken=page_token,
        )
        return request.execute()


class CalendarReader:
    """Read a complete bounded window or return a non-authoritative snapshot."""

    def __init__(
        self,
        *,
        gateway: CalendarGateway,
        calendar_id: str,
        lookahead_days: int,
        limits: CalendarLimits,
        lookback_days: int = 0,
    ) -> None:
        if lookback_days < 0:
            raise ValueError("lookback_days must not be negative")
        self._gateway = gateway
        self._calendar_id = calendar_id
        self._lookahead_days = lookahead_days
        self._lookback_days = lookback_days
        self._limits = limits

    def read_snapshot(self, now: datetime) -> Snapshot:
        """Return an authoritative snapshot, or a failed one if the window is incomplete."""
        if now.tzinfo is None:
            raise ValueError("snapshot time must include a timezone")

        observed_at = now.astimezone(UTC)
        time_min = observed_at - timedelta(days=self._lookback_days)
        time_max = observed_at + timedelta(days=self._lookahead_days)
        accumulator = _PageAccumulator(limits=self._limits, observed_at=observed_at)
        page_token: str | None = None
        page_count = 0

        while True:
            page_count += 1
            if page_count > self._limits.max_pages:
                return _failed(observed_at, "calendar_limit_exceeded:max_pages")

            try:
                page = self._gateway.list_events_page(
                    calendar_id=self._calendar_id,
                    time_min=time_min,
                    time_max=time_max,
                    page_token=page_token,
                )
            except Exception as error:  # noqa: BLE001 - any gateway failure must fail closed
                return _failed(
                    observed_at,
                    f"calendar_request_failed:{type(error).__name__}",
                )

            try:
                page_token = accumulator.absorb(page)
            except CalendarDataError as error:
                return _failed(observed_at, str(error))

            if page_token is None:
                return Snapshot(
                    authority=SnapshotAuthority.AUTHORITATIVE,
                    observed_at=observed_at,
                    events=accumulator.events(),
                )


class _PageAccumulator:
    """Collect events across pages, rejecting the whole cycle on any bound breach."""

    def __init__(self, *, limits: CalendarLimits, observed_at: datetime) -> None:
        self._limits = limits
        self._observed_at = observed_at
        self._byte_count = 0
        self._events: dict[str, SourceEvent] = {}
        self._raw: dict[str, Mapping[str, object]] = {}

    def absorb(self, page: Mapping[str, object]) -> str | None:
        """Add one response page and return the next page token, if any."""
        self._charge_bounds(page)
        for raw_event_value in self._items(page):
            self._add_event(raw_event_value)
        return self._next_token(page)

    def events(self) -> tuple[SourceEvent, ...]:
        """Return every accepted event in first-seen order."""
        return tuple(self._events.values())

    def _charge_bounds(self, page: Mapping[str, object]) -> None:
        self._byte_count += len(orjson.dumps(page, default=str))
        if self._byte_count > self._limits.max_snapshot_bytes:
            raise CalendarDataError("calendar_limit_exceeded:max_snapshot_bytes")
        if _longest_string(page) > self._limits.max_field_chars:
            raise CalendarDataError("calendar_limit_exceeded:max_field_chars")

    def _items(self, page: Mapping[str, object]) -> list[object]:
        raw_items_value = page.get("items", [])
        if not isinstance(raw_items_value, list):
            raise CalendarDataError("calendar_response_invalid:items")
        return cast("list[object]", raw_items_value)

    def _add_event(self, raw_event_value: object) -> None:
        raw_event = as_mapping(raw_event_value)
        if raw_event is None:
            raise CalendarDataError("calendar_response_invalid:event")
        event = _source_event(raw_event, self._observed_at)
        prior = self._raw.get(event.event_id)
        if prior is not None and prior != raw_event:
            raise CalendarDataError("calendar_response_invalid:duplicate_event")
        self._raw[event.event_id] = raw_event
        self._events[event.event_id] = event
        if len(self._events) > self._limits.max_events:
            raise CalendarDataError("calendar_limit_exceeded:max_events")

    def _next_token(self, page: Mapping[str, object]) -> str | None:
        raw_next_token = page.get("nextPageToken")
        if raw_next_token is None:
            return None
        if isinstance(raw_next_token, str):
            return raw_next_token
        raise CalendarDataError("calendar_response_invalid:next_page_token")


def sanitize_event_payload(
    event: Mapping[str, object], *, sensitive_terms: Sequence[str] = ()
) -> dict[str, object]:
    """Return a structurally useful fixture with direct identifiers and PII removed."""
    sanitized = scrub_mapping(event, sensitive_terms, dropped_keys=DROPPED_FIXTURE_KEYS)
    raw_id = event.get("id")
    if isinstance(raw_id, str):
        digest = hashlib.sha256(raw_id.encode("utf-8")).hexdigest()[:12]
        sanitized["id"] = f"event-{digest}"
    return sanitized


def _source_event(raw_event: Mapping[str, object], observed_at: datetime) -> SourceEvent:
    event_id = raw_event.get("id")
    if not isinstance(event_id, str) or not event_id:
        raise CalendarDataError("calendar_response_invalid:event_id")

    summary = raw_event.get("summary", "")
    status = raw_event.get("status", "confirmed")
    if not isinstance(summary, str) or not isinstance(status, str):
        raise CalendarDataError(f"calendar_response_invalid:event_fields:{event_id}")

    return SourceEvent(
        event_id=event_id,
        summary=summary,
        starts_at=_event_boundary(raw_event.get("start"), event_id, "start"),
        ends_at=_event_boundary(raw_event.get("end"), event_id, "end"),
        status=status,
        updated_at=_optional_datetime(raw_event.get("updated"), event_id, "updated"),
        observed_at=observed_at,
        fields=dict(raw_event),
    )


def _event_boundary(value: object, event_id: str, field: str) -> datetime | None:
    if value is None:
        return None
    boundary = as_mapping(value)
    if boundary is None:
        raise CalendarDataError(f"calendar_response_invalid:{field}:{event_id}")
    date_time = boundary.get("dateTime")
    if date_time is None and isinstance(boundary.get("date"), str):
        return None
    return _optional_datetime(date_time, event_id, field)


def _optional_datetime(value: object, event_id: str, field: str) -> datetime | None:
    if value is None:
        return None
    if not isinstance(value, str):
        raise CalendarDataError(f"calendar_response_invalid:{field}:{event_id}")
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError as error:
        raise CalendarDataError(f"calendar_response_invalid:{field}:{event_id}") from error
    if parsed.tzinfo is None:
        raise CalendarDataError(f"calendar_response_invalid:{field}_timezone:{event_id}")
    return parsed.astimezone(UTC)


def _failed(observed_at: datetime, reason: str) -> Snapshot:
    return Snapshot(
        authority=SnapshotAuthority.NON_AUTHORITATIVE,
        observed_at=observed_at,
        reason=reason,
    )


def _rfc3339(value: datetime) -> str:
    return value.astimezone(UTC).isoformat().replace("+00:00", "Z")


def _longest_string(value: object) -> int:
    if isinstance(value, str):
        return len(value)
    mapping = as_mapping(value)
    if mapping is not None:
        lengths = [max(len(key), _longest_string(item)) for key, item in mapping.items()]
        return max(lengths, default=0)
    if isinstance(value, list):
        items = cast("list[object]", value)
        return max((_longest_string(item) for item in items), default=0)
    return 0


DROPPED_FIXTURE_KEYS = frozenset(
    {
        "attachments",
        "attendees",
        "conferenceData",
        "etag",
        "hangoutLink",
        "htmlLink",
        "iCalUID",
    }
)
"""Calendar keys that carry identity or private links and never belong in a fixture."""
