"""The synchronization engine: one cycle, the loop that repeats it, and the lock that guards it.

One engine sits behind every entry point so the dry run, the one-shot apply, and the daemon
cannot drift. A cycle is read → parse → read wall → plan → (apply). Any non-authoritative
input ends the cycle before the plan is computed; nothing is written from a partial view.
"""

from __future__ import annotations

import fcntl
import logging
import os
import signal
import time
from contextlib import contextmanager
from dataclasses import dataclass
from enum import StrEnum
from typing import TYPE_CHECKING, Protocol

from .flightwall import WriteOutcome
from .models import SnapshotAuthority
from .parser import parse_cycle
from .reconcile import Wanted, apply_plan, plan, recover_pending_write

if TYPE_CHECKING:
    import threading
    from collections.abc import Callable, Generator
    from datetime import datetime
    from pathlib import Path

    from .calendar import CalendarReader
    from .flightwall import FlightWallClient, WallSnapshot, WriteResult
    from .models import Snapshot
    from .parser import ParsedCycle
    from .reconcile import Plan
    from .state import StateStore

log = logging.getLogger(__name__)


class CycleStatus(StrEnum):
    """How far one cycle got, and therefore what its report means."""

    CALENDAR_NOT_AUTHORITATIVE = "calendar_not_authoritative"
    PARSE_NOT_AUTHORITATIVE = "parse_not_authoritative"
    WALL_NOT_AUTHORITATIVE = "wall_not_authoritative"
    """Calendar and parse were fine; the report carries provisional local intent only."""
    NO_CHANGE = "no_change"
    DRY_RUN = "dry_run"
    """A write was planned and not sent."""
    APPLIED = "applied"
    REJECTED = "rejected"
    UNKNOWN = "unknown"
    """The write did not complete; the journal was settled from a re-read where possible."""


@dataclass(frozen=True, slots=True)
class CycleReport:
    """Everything one cycle observed and decided. Safe to log: no document, no descriptions."""

    status: CycleStatus
    started_at: datetime
    calendar: Snapshot
    parsed: ParsedCycle | None = None
    wall: WallSnapshot | None = None
    plan: Plan | None = None
    write: WriteResult | None = None
    recovery: str | None = None

    @property
    def wanted(self) -> tuple[str, ...]:
        """Flight numbers the calendar wants, in parse order."""
        if self.parsed is None:
            return ()
        return tuple(flight.designator for flight in self.parsed.flights)

    def summary(self) -> str:
        """One journal-safe line: counts, designators, and the reason for any stop."""
        parts = [f"status={self.status.value}"]
        if self.recovery is not None:
            parts.append(f"recovery={self.recovery}")
        parts.extend(self._source_parts())
        parts.extend(self._plan_parts())
        if self.write is not None and self.write.reason:
            parts.append(f"write_reason={self.write.reason}")
        return " ".join(parts)

    def _source_parts(self) -> list[str]:
        parts = [f"calendar_events={len(self.calendar.events)}"]
        if self.calendar.reason:
            parts.append(f"calendar_reason={self.calendar.reason}")
        if self.parsed is not None:
            parts.append(f"wanted={list(self.wanted)}")
            if self.parsed.reason:
                parts.append(f"parse_reason={self.parsed.reason}")
        if self.wall is not None:
            parts.append(f"wall={list(self.wall.flight_numbers)}")
            if self.wall.reason:
                parts.append(f"wall_reason={self.wall.reason}")
        return parts

    def _plan_parts(self) -> list[str]:
        if self.plan is None:
            return []
        parts = [f"add={list(self.plan.additions)} remove={list(self.plan.removals)}"]
        for label in ("suppressed", "unplaceable", "released"):
            values: tuple[str, ...] = getattr(self.plan, label)
            if values:
                parts.append(f"{label}={list(values)}")
        return parts


def run_cycle(
    *,
    calendar: CalendarReader,
    wall: FlightWallClient,
    store: StateStore,
    now: datetime,
    apply: bool,
) -> CycleReport:
    """Run one full cycle. ``apply=False`` plans but never writes."""
    calendar_snapshot = calendar.read_snapshot(now)
    if calendar_snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        return CycleReport(CycleStatus.CALENDAR_NOT_AUTHORITATIVE, now, calendar_snapshot)

    parsed = parse_cycle(calendar_snapshot)
    if parsed.authority is not SnapshotAuthority.AUTHORITATIVE:
        return CycleReport(CycleStatus.PARSE_NOT_AUTHORITATIVE, now, calendar_snapshot, parsed)

    wall_snapshot = wall.read()
    recovery = recover_pending_write(store, wall_snapshot)
    if recovery is not None and wall_snapshot.authority is SnapshotAuthority.AUTHORITATIVE:
        # The journal changed; read the wall again so the plan sees the settled state.
        wall_snapshot = wall.read()
    if wall_snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        return CycleReport(
            CycleStatus.WALL_NOT_AUTHORITATIVE,
            now,
            calendar_snapshot,
            parsed,
            wall_snapshot,
            recovery=recovery,
        )

    wanted = tuple(Wanted(flight.designator, flight.key) for flight in parsed.flights)
    the_plan = plan(wanted, wall_snapshot, store.owned_flights())

    if not apply:
        status = CycleStatus.DRY_RUN if the_plan.changed else CycleStatus.NO_CHANGE
        return CycleReport(status, now, calendar_snapshot, parsed, wall_snapshot, the_plan, recovery=recovery)

    write = apply_plan(wall, store, wall_snapshot, the_plan, now=now)
    if write is None:
        status = CycleStatus.NO_CHANGE
    elif write.outcome is WriteOutcome.APPLIED:
        status = CycleStatus.APPLIED
    elif write.outcome is WriteOutcome.REJECTED:
        status = CycleStatus.REJECTED
    else:
        status = CycleStatus.UNKNOWN
    return CycleReport(status, now, calendar_snapshot, parsed, wall_snapshot, the_plan, write, recovery)


class Cycle(Protocol):
    """One cycle, with the clock already bound."""

    def __call__(self) -> CycleReport:
        """Run and report."""
        ...


def run_forever(
    cycle: Cycle,
    *,
    interval_seconds: float,
    stop: threading.Event,
    monotonic: Callable[[], float] = time.monotonic,
) -> int:
    """Run cycles until ``stop`` is set. Never overlaps two cycles; never sleeps past a stop."""
    cycles = 0
    while not stop.is_set():
        started = monotonic()
        try:
            report = cycle()
        except Exception:
            log.exception("cycle failed")
        else:
            log.info("cycle %s", report.summary())
        cycles += 1
        elapsed = monotonic() - started
        remaining = interval_seconds - elapsed
        if remaining <= 0:
            log.warning(
                "cycle took %.1fs, longer than the %.0fs interval; starting the next at once",
                elapsed,
                interval_seconds,
            )
            continue
        stop.wait(remaining)
    return cycles


def install_stop_signals(stop: threading.Event) -> None:
    """Set ``stop`` on SIGTERM or SIGINT so the loop exits at the next safe point."""

    def _request_stop(signum: int, _frame: object) -> None:
        log.info("received %s, stopping after the current cycle", signal.Signals(signum).name)
        stop.set()

    signal.signal(signal.SIGTERM, _request_stop)
    signal.signal(signal.SIGINT, _request_stop)


class LockBusyError(RuntimeError):
    """Another mutating process holds the host lock."""


@contextmanager
def host_lock(path: Path) -> Generator[None, None, None]:
    """Hold an exclusive advisory lock for the duration. Raise ``LockBusyError`` instead of waiting."""
    path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    descriptor = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise LockBusyError(f"another flighty-wall process holds {path}") from error
        yield
    finally:
        os.close(descriptor)


def lock_path_for(state_path: Path) -> Path:
    """The lock sits beside the state database, in the same private directory."""
    return state_path.with_name(f"{state_path.name}.lock")


def configure_logging(level: str) -> None:
    """Plain stderr lines for the journal: systemd adds the timestamp and unit."""
    logging.basicConfig(
        level=level.upper(),
        format="%(levelname)s %(name)s: %(message)s",
        stream=None,
        force=True,
    )
