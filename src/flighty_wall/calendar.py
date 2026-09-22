"""Bounded Google Calendar reads and privacy-safe fixture sanitization."""

from __future__ import annotations

import hashlib
import json
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from typing import Protocol, cast

from .models import Snapshot, SnapshotAuthority, SourceEvent


class CalendarServiceRequest(Protocol):
    def execute(self) -> Mapping[str, object]: ...


class CalendarEventsResource(Protocol):
    def list(self, **parameters: object) -> CalendarServiceRequest: ...


class CalendarService(Protocol):
    def events(self) -> CalendarEventsResource: ...


class CalendarGateway(Protocol):
    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, object]: ...


@dataclass(frozen=True, slots=True)
class CalendarLimits:
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
        if now.tzinfo is None:
            raise ValueError("snapshot time must include a timezone")

        observed_at = now.astimezone(UTC)
        time_min = observed_at - timedelta(days=self._lookback_days)
        time_max = observed_at + timedelta(days=self._lookahead_days)
        page_token: str | None = None
        page_count = 0
        byte_count = 0
        events_by_id: dict[str, SourceEvent] = {}
        raw_by_id: dict[str, Mapping[str, object]] = {}

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
            except Exception as error:
                return _failed(
                    observed_at,
                    f"calendar_request_failed:{type(error).__name__}",
                )

            try:
                byte_count += len(
                    json.dumps(page, default=str, separators=(",", ":")).encode("utf-8")
                )
                if byte_count > self._limits.max_snapshot_bytes:
                    raise CalendarDataError("calendar_limit_exceeded:max_snapshot_bytes")
                if _longest_string(page) > self._limits.max_field_chars:
                    raise CalendarDataError("calendar_limit_exceeded:max_field_chars")

                raw_items_value = page.get("items", [])
                if not isinstance(raw_items_value, list):
                    raise CalendarDataError("calendar_response_invalid:items")
                raw_items = cast(list[object], raw_items_value)

                for raw_event_value in raw_items:
                    raw_event = _string_mapping(raw_event_value)
                    if raw_event is None:
                        raise CalendarDataError("calendar_response_invalid:event")
                    event = _source_event(raw_event, observed_at)
                    prior = raw_by_id.get(event.event_id)
                    if prior is not None and prior != raw_event:
                        raise CalendarDataError("calendar_response_invalid:duplicate_event")
                    raw_by_id[event.event_id] = raw_event
                    events_by_id[event.event_id] = event
                    if len(events_by_id) > self._limits.max_events:
                        raise CalendarDataError("calendar_limit_exceeded:max_events")

                raw_next_token = page.get("nextPageToken")
                if raw_next_token is None:
                    page_token = None
                elif isinstance(raw_next_token, str):
                    page_token = raw_next_token
                else:
                    raise CalendarDataError("calendar_response_invalid:next_page_token")
            except CalendarDataError as error:
                return _failed(observed_at, str(error))

            if page_token is None:
                return Snapshot(
                    authority=SnapshotAuthority.AUTHORITATIVE,
                    observed_at=observed_at,
                    events=tuple(events_by_id.values()),
                )


def sanitize_event_payload(
    event: Mapping[str, object], *, sensitive_terms: Sequence[str] = ()
) -> dict[str, object]:
    """Return a structurally useful fixture with direct identifiers and PII removed."""

    sanitized = _sanitize_mapping(event, sensitive_terms)
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
    boundary = _string_mapping(value)
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
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
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
    mapping = _string_mapping(value)
    if mapping is not None:
        lengths = [max(len(key), _longest_string(item)) for key, item in mapping.items()]
        return max(lengths, default=0)
    if isinstance(value, list):
        items = cast(list[object], value)
        return max((_longest_string(item) for item in items), default=0)
    return 0


def _string_mapping(value: object) -> Mapping[str, object] | None:
    if not isinstance(value, Mapping):
        return None
    return cast(Mapping[str, object], value)


_DROPPED_FIXTURE_KEYS = {
    "attachments",
    "attendees",
    "conferenceData",
    "etag",
    "hangoutLink",
    "htmlLink",
    "iCalUID",
}
_EMAIL = re.compile(r"[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}", re.IGNORECASE)
_URL = re.compile(r"[A-Z][A-Z0-9+.-]*://\S+", re.IGNORECASE)
_UUID = re.compile(
    r"\b[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}\b",
    re.IGNORECASE,
)
_BOOKING = re.compile(
    r"(?im)\b(confirmation|reservation|booking)(?:\s+(?:code|number))?\s*[:#-]?\s*[A-Z0-9-]+"
)
_SEAT = re.compile(r"(?im)\bseat\s*[:#-]?\s*[A-Z0-9-]+")


def _sanitize_mapping(
    value: Mapping[str, object], sensitive_terms: Sequence[str]
) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, item in value.items():
        if key in _DROPPED_FIXTURE_KEYS:
            continue
        result[str(key)] = _sanitize_value(item, sensitive_terms)
    return result


def _sanitize_value(value: object, sensitive_terms: Sequence[str]) -> object:
    if isinstance(value, str):
        redacted = _EMAIL.sub("<redacted-email>", value)
        redacted = _URL.sub("<redacted-url>", redacted)
        redacted = _UUID.sub("<redacted-uuid>", redacted)
        redacted = _BOOKING.sub(r"\1: <redacted>", redacted)
        redacted = _SEAT.sub("Seat: <redacted>", redacted)
        for term in sensitive_terms:
            if term:
                redacted = re.sub(re.escape(term), "<redacted-name>", redacted, flags=re.I)
        return redacted
    mapping = _string_mapping(value)
    if mapping is not None:
        return _sanitize_mapping(mapping, sensitive_terms)
    if isinstance(value, list):
        items = cast(list[object], value)
        return [_sanitize_value(item, sensitive_terms) for item in items]
    return value
