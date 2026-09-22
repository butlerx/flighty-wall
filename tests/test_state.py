from __future__ import annotations

import sqlite3
import stat
from pathlib import Path

import pytest

from flighty_wall.state import StateError, StateStore


def mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


def test_metadata_survives_reopen(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    store = StateStore(state_dir / "state.sqlite3")
    store.set_metadata("last_authoritative_sync", "2026-09-21T12:00:00Z")
    store.close()

    reopened = StateStore(state_dir / "state.sqlite3")

    assert reopened.schema_version == 2
    assert reopened.get_metadata("last_authoritative_sync") == "2026-09-21T12:00:00Z"
    reopened.close()


def test_transaction_rolls_back_on_failure(tmp_path: Path) -> None:
    store = StateStore(tmp_path / "state" / "state.sqlite3")
    store.set_metadata("status", "before")

    def write_then_fail() -> None:
        with store.transaction() as transaction:
            transaction.set_metadata("status", "after")
            raise RuntimeError("interrupt")

    with pytest.raises(RuntimeError, match="interrupt"):
        write_then_fail()

    assert store.get_metadata("status") == "before"
    store.close()


def test_state_files_and_directory_are_private(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    state_path = state_dir / "state.sqlite3"
    store = StateStore(state_path)
    store.set_metadata("key", "value")

    assert mode(state_dir) == 0o700
    assert mode(state_path) == 0o600
    for suffix in ("-wal", "-shm"):
        sibling = Path(f"{state_path}{suffix}")
        if sibling.exists():
            assert mode(sibling) == 0o600
    store.close()


def test_existing_insecure_state_directory_is_rejected(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o755)

    with pytest.raises(StateError, match="0700"):
        StateStore(state_dir / "state.sqlite3")


def test_bootstrap_creates_only_the_captured_contract_tables(tmp_path: Path) -> None:
    state_path = tmp_path / "state" / "state.sqlite3"
    store = StateStore(state_path)
    store.close()

    connection = sqlite3.connect(state_path)
    tables = {
        row[0]
        for row in connection.execute(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'"
        )
    }
    connection.close()

    # No mode-lease or remote-id tables: the wall has neither. Ownership is by flight_number.
    assert tables == {"metadata", "schema_info", "owned_flights", "pending_writes"}


def test_schema_one_database_is_migrated_in_place(tmp_path: Path) -> None:
    state_dir = tmp_path / "state"
    state_dir.mkdir(mode=0o700)
    state_path = state_dir / "state.sqlite3"
    state_path.touch(mode=0o600)
    connection = sqlite3.connect(state_path)
    connection.executescript(
        """
        CREATE TABLE schema_info (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            version INTEGER NOT NULL
        );
        CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT INTO schema_info VALUES (1, 1);
        INSERT INTO metadata VALUES ('kept', 'yes');
        """
    )
    connection.commit()
    connection.close()

    store = StateStore(state_path)

    assert store.schema_version == 2
    assert store.get_metadata("kept") == "yes"
    assert store.owned_flights() == {}
    store.close()


def test_owned_flights_round_trip_and_are_keyed_by_flight_number(tmp_path: Path) -> None:
    store = StateStore(tmp_path / "state" / "state.sqlite3")

    with store.transaction() as transaction:
        transaction.record_owned(
            "VY8721", first_added_at="2026-09-22T10:00:00Z", source_key="VY8721:DUB:2026-10-24"
        )
        transaction.record_owned(
            "BA5", first_added_at="2026-09-22T10:00:00Z", source_key="BA5:LHR:2026-10-30"
        )
        transaction.record_owned(
            "BA5", first_added_at="2026-09-22T10:00:00Z", source_key="BA5:LHR:2026-10-30"
        )

    owned = store.owned_flights()
    assert set(owned) == {"VY8721", "BA5"}
    assert owned["BA5"].source_key == "BA5:LHR:2026-10-30"

    with store.transaction() as transaction:
        transaction.release_owned("BA5")

    assert set(store.owned_flights()) == {"VY8721"}
    store.close()


def test_pending_write_is_durable_until_resolved(tmp_path: Path) -> None:
    state_path = tmp_path / "state" / "state.sqlite3"
    store = StateStore(state_path)

    with store.transaction() as transaction:
        transaction.begin_pending_write(desired=("EI61", "BA5"), started_at="2026-09-22T10:00:00Z")
    store.close()

    reopened = StateStore(state_path)
    pending = reopened.pending_write()
    assert pending is not None
    assert pending.desired == ("EI61", "BA5")
    assert pending.started_at == "2026-09-22T10:00:00Z"

    with reopened.transaction() as transaction:
        transaction.resolve_pending_write()

    assert reopened.pending_write() is None
    reopened.close()


def test_only_one_pending_write_may_exist(tmp_path: Path) -> None:
    store = StateStore(tmp_path / "state" / "state.sqlite3")
    with store.transaction() as transaction:
        transaction.begin_pending_write(desired=("EI61",), started_at="t1")

    def second() -> None:
        with store.transaction() as transaction:
            transaction.begin_pending_write(desired=("BA5",), started_at="t2")

    with pytest.raises(StateError, match="pending write"):
        second()

    pending = store.pending_write()
    assert pending is not None
    assert pending.desired == ("EI61",)
    store.close()
