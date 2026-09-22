"""Tests for the synchronization engine: one cycle, the loop, and the host lock."""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import TYPE_CHECKING, Any

import pytest

from flighty_wall.calendar import CalendarLimits, CalendarReader
from flighty_wall.flightwall import Fingerprint, TrackedFlight, WallSnapshot, WriteOutcome, WriteResult
from flighty_wall.models import SnapshotAuthority
from flighty_wall.service import (
    CycleReport,
    CycleStatus,
    LockBusyError,
    host_lock,
    lock_path_for,
    run_cycle,
    run_forever,
)
from flighty_wall.state import StateStore

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence
    from pathlib import Path

NOW = datetime(2026, 9, 22, 12, 0, tzinfo=UTC)
FINGERPRINT = Fingerprint("mini-v1", frozenset({"display_config", "request_config", "version"}), frozenset())


# --- fakes -----------------------------------------------------------------------------------


def flighty_event(event_id: str, friend: str, carrier: str, number: str, route: str) -> dict[str, Any]:
    """A calendar event in the shape Flighty exports (see the google_calendar fixture README)."""
    origin, destination = route.split("-")
    return {
        "id": event_id,
        "status": "confirmed",
        "summary": f"{friend}: \u2708 {origin}\u200b\u2192\u200b{destination} \u2022 {carrier}\u00a0{number}",
        "description": f"{carrier} {number}\n{origin} to {destination}\n\u2197 10:00 IST\n\u2198 13:00 CET",
        "location": origin,
        "start": {"dateTime": "2026-10-24T10:00:00+01:00", "timeZone": "Europe/Dublin"},
        "end": {"dateTime": "2026-10-24T13:00:00+02:00", "timeZone": "Europe/Madrid"},
        "updated": "2026-09-20T09:00:00Z",
    }


class FixtureGateway:
    def __init__(self, page: Mapping[str, Any] | Exception) -> None:
        self.page = page

    def list_events_page(
        self,
        *,
        calendar_id: str,
        time_min: datetime,
        time_max: datetime,
        page_token: str | None,
    ) -> Mapping[str, Any]:
        del calendar_id, time_min, time_max, page_token
        if isinstance(self.page, Exception):
            raise self.page
        return self.page


def calendar(*events: dict[str, Any]) -> CalendarReader:
    return CalendarReader(
        gateway=FixtureGateway({"items": list(events)}),
        calendar_id="friends@example.invalid",
        lookahead_days=60,
        limits=CalendarLimits(max_pages=10, max_events=500, max_field_chars=8192, max_snapshot_bytes=1 << 20),
    )


def sample_calendar(friend: str = "Alice") -> CalendarReader:
    return calendar(flighty_event("e1", friend, "VY", "8721", "DUB-BCN"))


def broken_calendar() -> CalendarReader:
    return CalendarReader(
        gateway=FixtureGateway(TimeoutError("google")),
        calendar_id="friends@example.invalid",
        lookahead_days=60,
        limits=CalendarLimits(max_pages=10, max_events=500, max_field_chars=8192, max_snapshot_bytes=1 << 20),
    )


def wall_snapshot(*numbers: str) -> WallSnapshot:
    return WallSnapshot(
        authority=SnapshotAuthority.AUTHORITATIVE,
        observed_at=NOW,
        tracked_flights=tuple(TrackedFlight(n, "2026-09-01T00:00:00.000Z") for n in numbers),
        fingerprint=FINGERPRINT,
        document={
            "display_config": {"model": "mini-v1"},
            "request_config": {"tracked_flights": []},
            "version": 2,
        },
    )


@dataclass
class FakeWall:
    """Stands in for FlightWallClient: scripted reads, recorded writes."""

    reads: list[WallSnapshot]
    write_outcome: WriteOutcome = WriteOutcome.APPLIED
    writes: list[tuple[str, ...]] = field(default_factory=list[tuple[str, ...]])

    def read(self) -> WallSnapshot:
        return self.reads.pop(0) if len(self.reads) > 1 else self.reads[0]

    def replace_tracked_flights(
        self, snapshot: WallSnapshot, flights: Sequence[TrackedFlight]
    ) -> WriteResult:
        del snapshot
        numbers = tuple(f.flight_number for f in flights)
        self.writes.append(numbers)
        after = wall_snapshot(*numbers) if self.write_outcome is not WriteOutcome.REJECTED else self.reads[0]
        return WriteResult(
            self.write_outcome, after, None if self.write_outcome is WriteOutcome.APPLIED else "x"
        )


def store(tmp_path: Path) -> StateStore:
    return StateStore(tmp_path / "state" / "state.sqlite3")


def cycle(
    tmp_path: Path,
    *,
    cal: CalendarReader,
    wall: FakeWall,
    apply: bool,
) -> tuple[CycleReport, StateStore]:
    journal = store(tmp_path)
    report = run_cycle(calendar=cal, wall=wall, store=journal, now=NOW, apply=apply)  # type: ignore[arg-type]
    return report, journal


# --- run_cycle -------------------------------------------------------------------------------


def test_dry_run_plans_the_add_and_writes_nothing(tmp_path: Path) -> None:
    wall = FakeWall([wall_snapshot("EI61")])
    report, journal = cycle(tmp_path, cal=sample_calendar(), wall=wall, apply=False)

    assert report.status is CycleStatus.DRY_RUN
    assert report.wanted == ("VY8721",)
    assert report.plan is not None
    assert report.plan.additions == ("VY8721",)
    assert wall.writes == []
    assert journal.owned_flights() == {}
    assert "add=['VY8721']" in report.summary()
    journal.close()


def test_apply_writes_once_and_records_ownership(tmp_path: Path) -> None:
    wall = FakeWall([wall_snapshot("EI61")])
    report, journal = cycle(tmp_path, cal=sample_calendar(), wall=wall, apply=True)

    assert report.status is CycleStatus.APPLIED
    assert wall.writes == [("EI61", "VY8721")]
    assert set(journal.owned_flights()) == {"VY8721"}
    journal.close()


def test_unchanged_cycle_makes_no_write(tmp_path: Path) -> None:
    journal = store(tmp_path)
    with journal.transaction() as transaction:
        transaction.record_owned("VY8721", first_added_at="t", source_key="k")
    wall = FakeWall([wall_snapshot("EI61", "VY8721")])

    for _ in range(10):
        report = run_cycle(
            calendar=sample_calendar(),
            wall=wall,  # type: ignore[arg-type]
            store=journal,
            now=NOW,
            apply=True,
        )
        assert report.status is CycleStatus.NO_CHANGE

    assert wall.writes == []
    journal.close()


def test_calendar_failure_stops_before_the_wall_is_read(tmp_path: Path) -> None:
    wall = FakeWall([wall_snapshot("EI61")])
    report, journal = cycle(tmp_path, cal=broken_calendar(), wall=wall, apply=True)

    assert report.status is CycleStatus.CALENDAR_NOT_AUTHORITATIVE
    assert report.plan is None
    assert wall.writes == []
    assert "calendar_reason=" in report.summary()
    journal.close()


def test_ambiguous_flighty_event_stops_before_the_wall_is_read(tmp_path: Path) -> None:
    odd = flighty_event("e1", "Alice", "VY", "8721", "DUB-BCN")
    odd["summary"] = "Alice: \u2708 something new Flighty invented"
    wall = FakeWall([wall_snapshot("EI61")])
    report, journal = cycle(tmp_path, cal=calendar(odd), wall=wall, apply=True)

    assert report.status is CycleStatus.PARSE_NOT_AUTHORITATIVE
    assert wall.writes == []
    journal.close()


def test_wall_failure_yields_provisional_intent_and_no_write(tmp_path: Path) -> None:
    wall = FakeWall([WallSnapshot.non_authoritative(NOW, "flightwall_server_error:503")])
    report, journal = cycle(tmp_path, cal=sample_calendar(), wall=wall, apply=True)

    assert report.status is CycleStatus.WALL_NOT_AUTHORITATIVE
    assert report.wanted == ("VY8721",)  # local intent is still visible
    assert report.plan is None  # but nothing remote-dependent is claimed
    assert wall.writes == []
    journal.close()


def test_rejected_write_is_reported_and_owns_nothing(tmp_path: Path) -> None:
    wall = FakeWall([wall_snapshot("EI61")], write_outcome=WriteOutcome.REJECTED)
    report, journal = cycle(tmp_path, cal=sample_calendar(), wall=wall, apply=True)

    assert report.status is CycleStatus.REJECTED
    assert journal.owned_flights() == {}
    journal.close()


def test_startup_recovery_settles_a_pending_write_before_planning(tmp_path: Path) -> None:
    journal = store(tmp_path)
    with journal.transaction() as transaction:
        transaction.begin_pending_write(desired=("EI61", "VY8721"), before=("EI61",), started_at="t")
    # The wall shows the pending write applied; the second read is what the plan uses.
    wall = FakeWall([wall_snapshot("EI61", "VY8721"), wall_snapshot("EI61", "VY8721")])

    report = run_cycle(
        calendar=sample_calendar(),
        wall=wall,  # type: ignore[arg-type]
        store=journal,
        now=NOW,
        apply=True,
    )

    assert report.recovery == "applied"
    assert report.status is CycleStatus.NO_CHANGE
    assert set(journal.owned_flights()) == {"VY8721"}
    assert journal.pending_write() is None
    journal.close()


def test_summary_never_contains_the_friend_name_or_description(tmp_path: Path) -> None:
    wall = FakeWall([wall_snapshot()])
    report, journal = cycle(tmp_path, cal=sample_calendar("Alice Smith"), wall=wall, apply=False)

    text = report.summary()
    assert "Alice" not in text
    assert "IST" not in text
    assert "VY8721" in text
    journal.close()


def test_same_flight_number_twice_in_the_window_is_wanted_once(tmp_path: Path) -> None:
    # Flighty exports every leg. AA577 DFW->DEN on two different dates is two calendar
    # events, two parser keys, but one wall entry: the wall has no date field.
    first = flighty_event("e1", "Alice", "AA", "577", "DFW-DEN")
    second = flighty_event("e2", "Bob", "AA", "577", "DFW-DEN")
    second["start"] = {"dateTime": "2026-11-05T10:00:00-06:00", "timeZone": "America/Chicago"}
    second["end"] = {"dateTime": "2026-11-05T12:00:00-07:00", "timeZone": "America/Denver"}
    wall = FakeWall([wall_snapshot()])

    report, journal = cycle(tmp_path, cal=calendar(first, second), wall=wall, apply=True)

    assert report.status is CycleStatus.APPLIED
    assert report.wanted == ("AA577", "AA577")  # both legs parsed
    assert wall.writes == [("AA577",)]  # one entry written
    assert set(journal.owned_flights()) == {"AA577"}
    journal.close()


# --- run_forever -----------------------------------------------------------------------------


def test_loop_runs_until_stopped_and_waits_the_remaining_interval() -> None:
    stop = threading.Event()
    clock = iter([0.0, 2.0, 120.0, 122.0, 240.0, 241.0])
    waits: list[float] = []
    original_wait = stop.wait

    def wait(timeout: float | None = None) -> bool:
        waits.append(timeout or 0.0)
        if len(waits) == 2:
            stop.set()
        return original_wait(0)

    stop.wait = wait  # type: ignore[method-assign]
    reports: list[int] = []

    def one() -> CycleReport:
        reports.append(1)
        return CycleReport(CycleStatus.NO_CHANGE, NOW, calendar_snapshot_stub())

    cycles = run_forever(one, interval_seconds=120, stop=stop, monotonic=lambda: next(clock))

    assert cycles == 2
    assert waits == [118.0, 118.0]


def test_loop_does_not_overlap_a_slow_cycle_and_starts_the_next_at_once() -> None:
    stop = threading.Event()
    clock = iter([0.0, 200.0, 200.0, 201.0])
    waits: list[float] = []

    def wait(timeout: float | None = None) -> bool:
        waits.append(timeout or 0.0)
        stop.set()
        return True

    stop.wait = wait  # type: ignore[method-assign]

    def one() -> CycleReport:
        return CycleReport(CycleStatus.NO_CHANGE, NOW, calendar_snapshot_stub())

    cycles = run_forever(one, interval_seconds=120, stop=stop, monotonic=lambda: next(clock))

    # First cycle overran: no wait, straight into the second. Second waited, then stop.
    assert cycles == 2
    assert waits == [119.0]


def test_loop_survives_a_cycle_that_raises() -> None:
    stop = threading.Event()
    calls = 0

    def flaky() -> CycleReport:
        nonlocal calls
        calls += 1
        if calls == 1:
            raise RuntimeError("boom")
        stop.set()
        return CycleReport(CycleStatus.NO_CHANGE, NOW, calendar_snapshot_stub())

    cycles = run_forever(flaky, interval_seconds=0.01, stop=stop)

    assert cycles == 2


def calendar_snapshot_stub() -> Any:
    from flighty_wall.models import Snapshot  # noqa: PLC0415

    return Snapshot(SnapshotAuthority.AUTHORITATIVE, NOW)


# --- host lock -------------------------------------------------------------------------------


def test_host_lock_is_exclusive_and_released(tmp_path: Path) -> None:
    lock = lock_path_for(tmp_path / "state" / "state.sqlite3")

    with host_lock(lock), pytest.raises(LockBusyError), host_lock(lock):
        pass

    with host_lock(lock):
        pass  # released cleanly


def test_lock_file_is_private(tmp_path: Path) -> None:
    import stat  # noqa: PLC0415

    lock = lock_path_for(tmp_path / "state" / "state.sqlite3")
    with host_lock(lock):
        assert stat.S_IMODE(lock.stat().st_mode) == 0o600
        assert stat.S_IMODE(lock.parent.stat().st_mode) == 0o700
