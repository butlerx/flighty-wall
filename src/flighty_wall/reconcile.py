"""Ownership-safe reconciliation between wanted flights and the wall's tracked list.

Three facts from the capture shape every rule here (``docs/flightwall-api-discovery.md``):
the wall has no ownership signal, so the journal is the only record of what the daemon
added; writes are whole-document last-writer-wins, so a plan is a complete desired list,
not a diff; and the server enforces no cap, so the daemon holds the line at five itself.

``plan`` is pure. ``apply_plan`` is the only writer and journals its intent first.
``recover_pending_write`` runs at startup to settle an intent the last run never resolved.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import UTC
from typing import TYPE_CHECKING, Literal, Protocol

from .flightwall import MAX_TRACKED_FLIGHTS, TrackedFlight, WriteOutcome, WriteResult
from .models import SnapshotAuthority
from .state import PendingWrite

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence
    from datetime import datetime

    from .flightwall import WallSnapshot
    from .state import OwnedFlight, StateStore

RecoveryOutcome = Literal["applied", "not_applied", "deferred"]


@dataclass(frozen=True, slots=True)
class Wanted:
    """One flight the calendar wants on the wall, and the key that asked for it."""

    flight_number: str
    source_key: str


@dataclass(frozen=True, slots=True)
class Plan:
    """The complete desired ``tracked_flights`` list, and how it differs from the wall."""

    desired: tuple[str, ...]
    """What the wall should hold after the write, in wall order with additions appended."""
    additions: tuple[str, ...]
    """Wanted numbers not on the wall that fit under the cap."""
    removals: tuple[str, ...]
    """Owned numbers on the wall that nothing wants any more."""
    manual: tuple[str, ...]
    """Numbers on the wall the daemon does not own. Always kept."""
    suppressed: tuple[str, ...]
    """Wanted numbers that are already manual entries. Not added, not adopted."""
    unplaceable: tuple[str, ...]
    """Wanted numbers that did not fit. Reported, never forced."""
    released: tuple[str, ...]
    """Owned numbers already gone from the wall (the server drops landed flights)."""
    wanted_keys: Mapping[str, str]
    """``flight_number`` to calendar key for every wanted flight, for the journal."""

    @property
    def changed(self) -> bool:
        """Whether a write is needed at all."""
        return bool(self.additions or self.removals)


class Writer(Protocol):
    """The one client method reconciliation needs."""

    def replace_tracked_flights(
        self, snapshot: WallSnapshot, flights: Sequence[TrackedFlight]
    ) -> WriteResult:
        """Replace the wall's list with ``flights`` and return the outcome plus a re-read."""
        ...


def plan(
    wanted: Sequence[Wanted],
    snapshot: WallSnapshot,
    owned: Mapping[str, OwnedFlight],
    *,
    cap: int = MAX_TRACKED_FLIGHTS,
) -> Plan:
    """Compute the desired list. Pure; raises rather than planning against bad input."""
    if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        raise ValueError("refusing to plan against a non-authoritative wall snapshot")
    wanted_numbers = [item.flight_number for item in wanted]
    if len(set(wanted_numbers)) != len(wanted_numbers):
        raise ValueError("duplicate flight numbers in the wanted list")

    wanted_keys = {item.flight_number: item.source_key for item in wanted}
    on_wall = snapshot.flight_numbers
    on_wall_set = set(on_wall)

    desired: list[str] = []
    manual: list[str] = []
    removals: list[str] = []
    for number in on_wall:
        if number not in owned:
            manual.append(number)
            desired.append(number)
        elif number in wanted_keys:
            desired.append(number)
        else:
            removals.append(number)

    suppressed = tuple(number for number in wanted_numbers if number in on_wall_set and number not in owned)
    released = tuple(number for number in owned if number not in on_wall_set)

    additions: list[str] = []
    unplaceable: list[str] = []
    for number in wanted_numbers:
        if number in on_wall_set:
            continue
        if len(desired) < cap:
            desired.append(number)
            additions.append(number)
        else:
            unplaceable.append(number)

    return Plan(
        desired=tuple(desired),
        additions=tuple(additions),
        removals=tuple(removals),
        manual=tuple(manual),
        suppressed=suppressed,
        unplaceable=tuple(unplaceable),
        released=released,
        wanted_keys=wanted_keys,
    )


def apply_plan(
    client: Writer,
    store: StateStore,
    snapshot: WallSnapshot,
    the_plan: Plan,
    *,
    now: datetime,
) -> WriteResult | None:
    """Write the plan if it changes anything; journal intent first, ownership after.

    Returns ``None`` when no write was needed. Ownership of entries that have already
    vanished from the wall is released either way.
    """
    if not the_plan.changed:
        if the_plan.released:
            with store.transaction() as transaction:
                for number in the_plan.released:
                    transaction.release_owned(number)
        return None

    started_at = _rfc3339(now)
    before = snapshot.flight_numbers
    with store.transaction() as transaction:
        transaction.begin_pending_write(desired=the_plan.desired, before=before, started_at=started_at)

    flights = _flights_for(the_plan, snapshot, now)
    result = client.replace_tracked_flights(snapshot, flights)

    if result.outcome is WriteOutcome.APPLIED:
        _settle(store, the_plan.desired, the_plan.wanted_keys, before=before, applied=True, now=started_at)
    elif result.outcome is WriteOutcome.REJECTED:
        _settle(store, the_plan.desired, the_plan.wanted_keys, before=before, applied=False, now=started_at)
    else:
        # UNKNOWN: a full body that reached the server applies. Only the re-read can say.
        recover_pending_write(store, result.snapshot, wanted_keys=the_plan.wanted_keys)
    return result


def recover_pending_write(
    store: StateStore,
    snapshot: WallSnapshot,
    *,
    wanted_keys: Mapping[str, str] | None = None,
) -> RecoveryOutcome | None:
    """Settle an intent the last run journaled but never resolved.

    If the wall now holds exactly the desired list, the write applied and ownership is
    recorded. If it does not, the write did not apply (or the owner has since intervened)
    and only the intent is cleared; the next cycle re-plans from scratch. A non-authoritative
    read defers the decision.
    """
    pending = store.pending_write()
    if pending is None:
        return None
    if snapshot.authority is not SnapshotAuthority.AUTHORITATIVE:
        return "deferred"
    applied = snapshot.flight_numbers == pending.desired
    _settle(
        store,
        pending.desired,
        wanted_keys or {},
        before=pending.before,
        applied=applied,
        now=pending.started_at,
    )
    return "applied" if applied else "not_applied"


def _settle(
    store: StateStore,
    desired: Sequence[str],
    wanted_keys: Mapping[str, str],
    *,
    before: Sequence[str],
    applied: bool,
    now: str,
) -> None:
    """Clear the intent; if the write applied, own what was added and release what was dropped."""
    with store.transaction() as transaction:
        transaction.resolve_pending_write()
        if not applied:
            return
        desired_set = set(desired)
        before_set = set(before)
        for number in desired:
            if number not in before_set:
                transaction.record_owned(
                    number,
                    first_added_at=now,
                    source_key=wanted_keys.get(number, ""),
                )
        for number in store.owned_flights():
            if number not in desired_set:
                transaction.release_owned(number)


def _flights_for(the_plan: Plan, snapshot: WallSnapshot, now: datetime) -> tuple[TrackedFlight, ...]:
    """Existing entries go back exactly as read; new ones are stamped the way the app does."""
    existing = {flight.flight_number: flight for flight in snapshot.tracked_flights}
    return tuple(
        existing[number] if number in existing else TrackedFlight.new(number, created_at=now)
        for number in the_plan.desired
    )


def _rfc3339(value: datetime) -> str:
    return value.astimezone(UTC).isoformat().replace("+00:00", "Z")


__all__ = [
    "PendingWrite",
    "Plan",
    "RecoveryOutcome",
    "Wanted",
    "Writer",
    "apply_plan",
    "plan",
    "recover_pending_write",
]
