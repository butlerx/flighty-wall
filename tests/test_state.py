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

    assert reopened.schema_version == 1
    assert reopened.get_metadata("last_authoritative_sync") == "2026-09-21T12:00:00Z"
    reopened.close()


def test_transaction_rolls_back_on_failure(tmp_path: Path) -> None:
    store = StateStore(tmp_path / "state" / "state.sqlite3")
    store.set_metadata("status", "before")

    with (
        pytest.raises(RuntimeError, match="interrupt"),
        store.transaction() as transaction,
    ):
        transaction.set_metadata("status", "after")
        raise RuntimeError("interrupt")

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


def test_bootstrap_does_not_guess_flightwall_schema(tmp_path: Path) -> None:
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

    assert tables == {"metadata", "schema_info"}
