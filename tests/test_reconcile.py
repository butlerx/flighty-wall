"""Tests for the reconciliation planner and the journal it drives.

The planner is pure: (wanted, wall snapshot, journal) -> plan. ``apply_plan`` is the only
function that writes, and it does so through the FlightWall client with a journaled intent.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import TYPE_CHECKING

import pytest

from flighty_wall.flightwall import (
    Fingerprint,
    TrackedFlight,
    WallSnapshot,
    WriteOutcome,
    WriteResult,
)
from flighty_wall.models import SnapshotAuthority
from flighty_wall.reconcile import (
    Plan,
    Wanted,
    apply_plan,
    plan,
    recover_pending_write,
)
from flighty_wall.state import OwnedFlight, StateStore

if TYPE_CHECKING:
    from collections.abc import Sequence
    from pathlib import Path

NOW = datetime(2026, 9, 22, 12, 0, tzinfo=UTC)
FINGERPRINT = Fingerprint("mini-v1", frozenset({"display_config", "request_config", "version"}), frozenset())


def wall(*numbers: str, authority: SnapshotAuthority = SnapshotAuthority.AUTHORITATIVE) -> WallSnapshot:
    return WallSnapshot(
        authority=authority,
        observed_at=NOW,
        tracked_flights=tuple(TrackedFlight(n, "2026-09-01T00:00:00.000Z") for n in numbers),
        fingerprint=FINGERPRINT,
        document={
            "display_config": {"model": "mini-v1"},
            "request_config": {"tracked_flights": []},
            "version": 2,
        },
    )


def owned(*numbers: str) -> dict[str, OwnedFlight]:
    return {n: OwnedFlight(n, "2026-09-01T00:00:00Z", f"{n}:XXX:2026-10-01") for n in numbers}


def wanted(*numbers: str) -> tuple[Wanted, ...]:
    return tuple(Wanted(n, f"{n}:XXX:2026-10-01") for n in numbers)


# --- plan(): pure -----------------------------------------------------------------------------


def test_unchanged_inputs_plan_no_write() -> None:
    result = plan(wanted("BA5"), wall("BA5"), owned("BA5"))

    assert result.desired == ("BA5",)
    assert not result.changed
    assert result.additions == ()
    assert result.removals == ()


def test_first_add_on_an_empty_wall() -> None:
    result = plan(wanted("VY8721"), wall(), {})

    assert result.desired == ("VY8721",)
    assert result.additions == ("VY8721",)
    assert result.changed


def test_manual_entries_are_kept_and_never_removed() -> None:
    # EI61 is on the wall and not in the journal: the owner put it there.
    result = plan(wanted("BA5"), wall("EI61"), {})

    assert result.desired == ("EI61", "BA5")
    assert result.removals == ()
    assert result.manual == ("EI61",)


def test_owned_entry_no_longer_wanted_is_removed() -> None:
    result = plan(wanted(), wall("EI61", "BA5"), owned("BA5"))

    assert result.desired == ("EI61",)
    assert result.removals == ("BA5",)


def test_a_friend_flying_a_manual_number_is_suppressed_not_adopted() -> None:
    result = plan(wanted("EI61"), wall("EI61"), {})

    assert result.desired == ("EI61",)
    assert result.additions == ()
    assert result.suppressed == ("EI61",)
    assert not result.changed


def test_owned_entry_that_vanished_is_released_without_a_write() -> None:
    # The server removes landed flights on its own. Nothing to write; the journal is stale.
    result = plan(wanted(), wall(), owned("EI61"))

    assert not result.changed
    assert result.released == ("EI61",)


def test_owned_entry_that_vanished_but_is_still_wanted_is_re_added() -> None:
    result = plan(wanted("BA5"), wall(), owned("BA5"))

    assert result.additions == ("BA5",)


def test_capacity_is_respected_and_overflow_is_reported() -> None:
    result = plan(wanted("A1", "A2", "A3"), wall("M1", "M2", "M3"), {})

    assert len(result.desired) == 5
    assert result.desired[:3] == ("M1", "M2", "M3")
    assert result.additions == ("A1", "A2")
    assert result.unplaceable == ("A3",)


def test_a_stale_owned_entry_frees_a_slot_in_the_same_write() -> None:
    # Five on the wall, one of them owned and no longer wanted, one new flight wanted.
    result = plan(wanted("NEW"), wall("M1", "M2", "M3", "M4", "OLD"), owned("OLD"))

    assert result.removals == ("OLD",)
    assert result.additions == ("NEW",)
    assert result.desired == ("M1", "M2", "M3", "M4", "NEW")


def test_manual_entries_are_never_evicted_even_when_the_wall_is_full_of_them() -> None:
    result = plan(wanted("NEW"), wall("M1", "M2", "M3", "M4", "M5"), {})

    assert result.desired == ("M1", "M2", "M3", "M4", "M5")
    assert result.unplaceable == ("NEW",)
    assert not result.changed


def test_wall_order_is_preserved_and_new_flights_append() -> None:
    result = plan(wanted("B", "A"), wall("M"), {})

    assert result.desired == ("M", "B", "A")


def test_non_authoritative_wall_yields_no_plan() -> None:
    with pytest.raises(ValueError, match="authoritative"):
        plan(wanted("BA5"), wall(authority=SnapshotAuthority.NON_AUTHORITATIVE), {})


def test_duplicate_wanted_numbers_are_rejected() -> None:
    with pytest.raises(ValueError, match="duplicate"):
        plan(wanted("BA5", "BA5"), wall(), {})


# --- apply_plan(): the one writer -----------------------------------------------------------


class ScriptedClient:
    """A FlightWallClient stand-in that records the flights it was asked to write."""

    def __init__(self, result: WriteResult) -> None:
        self.result = result
        self.writes: list[Sequence[TrackedFlight]] = []

    def replace_tracked_flights(
        self, snapshot: WallSnapshot, flights: Sequence[TrackedFlight]
    ) -> WriteResult:
        del snapshot
        self.writes.append(tuple(flights))
        return self.result


def store(tmp_path: Path) -> StateStore:
    return StateStore(tmp_path / "state" / "state.sqlite3")


def test_apply_with_no_change_does_not_write(tmp_path: Path) -> None:
    journal = store(tmp_path)
    client = ScriptedClient(WriteResult(WriteOutcome.APPLIED, wall("BA5")))
    unchanged = plan(wanted("BA5"), wall("BA5"), owned("BA5"))

    result = apply_plan(client, journal, wall("BA5"), unchanged, now=NOW)

    assert result is None
    assert client.writes == []
    assert journal.pending_write() is None
    journal.close()


def test_apply_journals_intent_then_writes_then_records_ownership(tmp_path: Path) -> None:
    journal = store(tmp_path)
    before = wall("EI61")
    client = ScriptedClient(WriteResult(WriteOutcome.APPLIED, wall("EI61", "BA5")))
    the_plan = plan(wanted("BA5"), before, {})

    result = apply_plan(client, journal, before, the_plan, now=NOW)

    assert result is not None
    assert result.outcome is WriteOutcome.APPLIED
    assert [f.flight_number for f in client.writes[0]] == ["EI61", "BA5"]
    # The manual entry is sent back exactly as read; the new one is stamped now.
    assert client.writes[0][0] == before.tracked_flights[0]
    assert client.writes[0][1].created_at == "2026-09-22T12:00:00.000Z"
    assert set(journal.owned_flights()) == {"BA5"}
    assert journal.pending_write() is None
    journal.close()


def test_apply_releases_ownership_of_removed_and_vanished_entries(tmp_path: Path) -> None:
    journal = store(tmp_path)
    with journal.transaction() as transaction:
        transaction.record_owned("OLD", first_added_at="t", source_key="k")
        transaction.record_owned("GONE", first_added_at="t", source_key="k")
    before = wall("EI61", "OLD")
    client = ScriptedClient(WriteResult(WriteOutcome.APPLIED, wall("EI61")))
    the_plan = plan(wanted(), before, journal.owned_flights())

    apply_plan(client, journal, before, the_plan, now=NOW)

    assert journal.owned_flights() == {}
    journal.close()


def test_apply_rejected_write_leaves_journal_unchanged(tmp_path: Path) -> None:
    journal = store(tmp_path)
    before = wall("EI61")
    client = ScriptedClient(
        WriteResult(WriteOutcome.REJECTED, before, "flightwall_credentials_rejected:1102")
    )
    the_plan = plan(wanted("BA5"), before, {})

    result = apply_plan(client, journal, before, the_plan, now=NOW)

    assert result is not None
    assert result.outcome is WriteOutcome.REJECTED
    assert journal.owned_flights() == {}
    assert journal.pending_write() is None
    journal.close()


def test_apply_unknown_outcome_that_applied_is_journaled_from_the_re_read(tmp_path: Path) -> None:
    journal = store(tmp_path)
    before = wall("EI61")
    client = ScriptedClient(
        WriteResult(WriteOutcome.UNKNOWN, wall("EI61", "BA5"), "flightwall_request_failed:X")
    )
    the_plan = plan(wanted("BA5"), before, {})

    apply_plan(client, journal, before, the_plan, now=NOW)

    assert set(journal.owned_flights()) == {"BA5"}
    assert journal.pending_write() is None
    journal.close()


def test_apply_unknown_outcome_that_did_not_apply_leaves_the_journal_alone(tmp_path: Path) -> None:
    journal = store(tmp_path)
    before = wall("EI61")
    client = ScriptedClient(WriteResult(WriteOutcome.UNKNOWN, wall("EI61"), "flightwall_request_failed:X"))
    the_plan = plan(wanted("BA5"), before, {})

    apply_plan(client, journal, before, the_plan, now=NOW)

    assert journal.owned_flights() == {}
    assert journal.pending_write() is None
    journal.close()


def test_apply_unknown_outcome_with_a_non_authoritative_re_read_keeps_the_intent(tmp_path: Path) -> None:
    journal = store(tmp_path)
    before = wall("EI61")
    unreadable = WallSnapshot.non_authoritative(NOW, "flightwall_server_error:503")
    client = ScriptedClient(WriteResult(WriteOutcome.UNKNOWN, unreadable, "flightwall_request_failed:X"))
    the_plan = plan(wanted("BA5"), before, {})

    apply_plan(client, journal, before, the_plan, now=NOW)

    pending = journal.pending_write()
    assert pending is not None
    assert pending.desired == ("EI61", "BA5")
    journal.close()


# --- recover_pending_write(): startup ----------------------------------------------------


def test_recovery_with_no_pending_write_is_a_no_op(tmp_path: Path) -> None:
    journal = store(tmp_path)

    assert recover_pending_write(journal, wall("EI61")) is None
    journal.close()


def test_recovery_when_the_wall_matches_the_intent_records_ownership(tmp_path: Path) -> None:
    journal = store(tmp_path)
    with journal.transaction() as transaction:
        transaction.begin_pending_write(desired=("EI61", "BA5"), started_at="t")

    outcome = recover_pending_write(journal, wall("EI61", "BA5"), before=("EI61",))

    assert outcome == "applied"
    assert set(journal.owned_flights()) == {"BA5"}
    assert journal.pending_write() is None
    journal.close()


def test_recovery_when_the_wall_does_not_match_clears_the_intent_only(tmp_path: Path) -> None:
    journal = store(tmp_path)
    with journal.transaction() as transaction:
        transaction.begin_pending_write(desired=("EI61", "BA5"), started_at="t")

    outcome = recover_pending_write(journal, wall("EI61"), before=("EI61",))

    assert outcome == "not_applied"
    assert journal.owned_flights() == {}
    assert journal.pending_write() is None
    journal.close()


def test_recovery_against_a_non_authoritative_wall_keeps_the_intent(tmp_path: Path) -> None:
    journal = store(tmp_path)
    with journal.transaction() as transaction:
        transaction.begin_pending_write(desired=("EI61", "BA5"), started_at="t")

    outcome = recover_pending_write(journal, WallSnapshot.non_authoritative(NOW, "x"))

    assert outcome == "deferred"
    assert journal.pending_write() is not None
    journal.close()


def test_plan_is_a_frozen_value() -> None:
    result = plan(wanted("BA5"), wall(), {})
    assert isinstance(result, Plan)
    with pytest.raises(AttributeError):
        result.desired = ()  # type: ignore[misc]
