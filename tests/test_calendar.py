"""Tests for bounded, authoritative Google Calendar snapshots."""

from __future__ import annotations

from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any

from flighty_wall.calendar import (
    CalendarLimits,
    CalendarReader,
    sanitize_event_payload,
)
from flighty_wall.models import SnapshotAuthority

if TYPE_CHECKING:
    from collections.abc import Mapping


class FakeGateway:
    def __init__(
        self,
        pages: Mapping[str | None, Mapping[str, Any] | Exception],
    ) -> None:
        self.pages = pages
        self.calls: list[dict[str, object]] = []

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, Any]:
        self.calls.append(
            {
                "calendar_id": calendar_id,
                "time_min": time_min,
                "time_max": time_max,
                "page_token": page_token,
            }
        )
        page = self.pages[page_token]
        if isinstance(page, Exception):
            raise page
        return page


def timed_event(
    event_id: str,
    *,
    summary: str = "AA123 · Friend",
    start: str = "2026-09-22T08:00:00-04:00",
    end: str = "2026-09-22T10:30:00-05:00",
) -> dict[str, Any]:
    return {
        "id": event_id,
        "status": "confirmed",
        "summary": summary,
        "description": "Flight AA123 from JFK to ORD",
        "start": {"dateTime": start, "timeZone": "America/New_York"},
        "end": {"dateTime": end, "timeZone": "America/Chicago"},
        "updated": "2026-09-21T11:00:00Z",
    }


def reader(gateway: FakeGateway, **limit_overrides: int) -> CalendarReader:
    limits = CalendarLimits(
        max_pages=limit_overrides.get("max_pages", 10),
        max_events=limit_overrides.get("max_events", 500),
        max_field_chars=limit_overrides.get("max_field_chars", 8_192),
        max_snapshot_bytes=limit_overrides.get("max_snapshot_bytes", 1_048_576),
    )
    return CalendarReader(
        gateway=gateway,
        calendar_id="friends@example.invalid",
        lookahead_days=limit_overrides.get("lookahead_days", 7),
        lookback_days=limit_overrides.get("lookback_days", 0),
        limits=limits,
    )


def test_reader_window_defaults_to_now_forward() -> None:
    gateway = FakeGateway({None: {"items": []}})
    now = datetime(2026, 9, 21, 12, 0, tzinfo=UTC)

    reader(gateway).read_snapshot(now)

    assert gateway.calls[0]["time_min"] == now
    assert gateway.calls[0]["time_max"] == datetime(2026, 9, 28, 12, 0, tzinfo=UTC)


def test_reader_lookback_extends_window_into_the_past() -> None:
    gateway = FakeGateway({None: {"items": []}})
    now = datetime(2026, 9, 21, 12, 0, tzinfo=UTC)

    reader(gateway, lookahead_days=60, lookback_days=3).read_snapshot(now)

    assert gateway.calls[0]["time_min"] == datetime(2026, 9, 18, 12, 0, tzinfo=UTC)
    assert gateway.calls[0]["time_max"] == datetime(2026, 11, 20, 12, 0, tzinfo=UTC)


def test_reader_consumes_every_page_before_marking_snapshot_authoritative() -> None:
    gateway = FakeGateway(
        {
            None: {"items": [timed_event("event-1")], "nextPageToken": "page-2"},
            "page-2": {"items": [timed_event("event-2")]},
        }
    )
    now = datetime(2026, 9, 21, 12, 0, tzinfo=UTC)

    snapshot = reader(gateway).read_snapshot(now)

    assert snapshot.authority is SnapshotAuthority.AUTHORITATIVE
    assert [event.event_id for event in snapshot.events] == ["event-1", "event-2"]
    assert snapshot.events[0].starts_at == datetime(2026, 9, 22, 12, 0, tzinfo=UTC)
    assert snapshot.events[0].ends_at == datetime(2026, 9, 22, 15, 30, tzinfo=UTC)
    assert snapshot.events[0].updated_at == datetime(2026, 9, 21, 11, 0, tzinfo=UTC)
    assert [call["page_token"] for call in gateway.calls] == [None, "page-2"]


def test_second_page_failure_discards_partial_events() -> None:
    gateway = FakeGateway(
        {
            None: {"items": [timed_event("event-1")], "nextPageToken": "page-2"},
            "page-2": TimeoutError("network timeout with private details"),
        }
    )

    snapshot = reader(gateway).read_snapshot(datetime(2026, 9, 21, 12, 0, tzinfo=UTC))

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.events == ()
    assert snapshot.reason == "calendar_request_failed:TimeoutError"
    assert "private details" not in snapshot.reason


def test_reader_rejects_more_pages_than_configured() -> None:
    gateway = FakeGateway(
        {
            None: {"items": [], "nextPageToken": "page-2"},
            "page-2": {"items": [], "nextPageToken": "page-3"},
        }
    )

    snapshot = reader(gateway, max_pages=1).read_snapshot(datetime(2026, 9, 21, 12, 0, tzinfo=UTC))

    assert snapshot.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert snapshot.reason == "calendar_limit_exceeded:max_pages"


def test_reader_rejects_event_and_field_limits() -> None:
    events_gateway = FakeGateway({None: {"items": [timed_event("1"), timed_event("2")]}})
    fields_gateway = FakeGateway({None: {"items": [timed_event("1", summary="A" * 40)]}})
    now = datetime(2026, 9, 21, 12, 0, tzinfo=UTC)

    too_many = reader(events_gateway, max_events=1).read_snapshot(now)
    oversized = reader(fields_gateway, max_field_chars=20).read_snapshot(now)

    assert too_many.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert too_many.reason == "calendar_limit_exceeded:max_events"
    assert oversized.authority is SnapshotAuthority.NON_AUTHORITATIVE
    assert oversized.reason == "calendar_limit_exceeded:max_field_chars"


def test_reader_retains_all_day_event_for_fail_closed_parser() -> None:
    gateway = FakeGateway(
        {
            None: {
                "items": [
                    {
                        "id": "all-day",
                        "status": "confirmed",
                        "summary": "AA123",
                        "start": {"date": "2026-09-22"},
                        "end": {"date": "2026-09-23"},
                    }
                ]
            }
        }
    )

    snapshot = reader(gateway).read_snapshot(datetime(2026, 9, 21, 12, 0, tzinfo=UTC))

    assert snapshot.authority is SnapshotAuthority.AUTHORITATIVE
    assert snapshot.events[0].starts_at is None
    assert snapshot.events[0].ends_at is None


def test_sanitizer_preserves_structure_and_flight_number_but_removes_pii() -> None:
    event = timed_event("private-google-id", summary="Alice Smith · AA123")
    event.update(
        {
            "description": (
                "Confirmation: ABC123\nSeat: 12A\nalice@example.com\nhttps://calendar.example/private-link"
            ),
            "creator": {"email": "alice@example.com", "displayName": "Alice Smith"},
            "organizer": {"email": "owner@example.com"},
            "attendees": [{"email": "friend@example.com"}],
        }
    )

    sanitized = sanitize_event_payload(event, sensitive_terms=("Alice Smith",))
    rendered = repr(sanitized)

    sanitized_id = sanitized["id"]
    assert isinstance(sanitized_id, str)
    assert sanitized_id.startswith("event-")
    assert "AA123" in str(sanitized["summary"])
    assert "Alice Smith" not in rendered
    assert "ABC123" not in rendered
    assert "12A" not in rendered
    assert "@example.com" not in rendered
    assert "private-link" not in rendered
    assert "attendees" not in sanitized


def test_sanitizer_redacts_flighty_deeplinks_and_calendar_uids() -> None:
    event = timed_event("private-google-id")
    event.update(
        {
            "description": (
                "Ryanair 8721\nDublin to Barcelona\n"
                "View in Flighty flighty://flight/651cbaa3-2a8b-4580-b88c-c7d2a0164f9e"
            ),
            "iCalUID": "9BDF5975-05CA-4689-B3BD-48501DC930D4",
            "etag": '"3579963474324702"',
        }
    )

    sanitized = sanitize_event_payload(event)
    rendered = repr(sanitized)

    assert "651cbaa3" not in rendered
    assert "flighty://" not in rendered
    assert "9BDF5975" not in rendered
    assert "3579963474324702" not in rendered
    assert "Ryanair 8721" in str(sanitized["description"])
    assert "Dublin to Barcelona" in str(sanitized["description"])
