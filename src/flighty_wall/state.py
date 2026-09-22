"""Private, crash-safe SQLite storage: the ownership journal and one pending write.

The wall carries no ownership signal (see ``docs/flightwall-api.md`` §4), so
this journal is the only record of which ``flight_number`` entries the daemon added. A
``flight_number`` absent from ``owned_flights`` is the owner's and is never removed.
"""

from __future__ import annotations

import os
import sqlite3
import stat
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Self, cast

import orjson

if TYPE_CHECKING:
    from collections.abc import Generator, Sequence
    from types import TracebackType

_SCHEMA_VERSION = 2


class StateError(RuntimeError):
    """Raised when state storage cannot be opened safely or an invariant would break."""


@dataclass(frozen=True, slots=True)
class OwnedFlight:
    """One ``flight_number`` the daemon added, and the calendar key that asked for it."""

    flight_number: str
    first_added_at: str
    source_key: str


@dataclass(frozen=True, slots=True)
class PendingWrite:
    """The desired list the daemon was about to POST, and what the wall held just before."""

    desired: tuple[str, ...]
    before: tuple[str, ...]
    started_at: str


class StateTransaction:
    """Operations available inside one explicit SQLite transaction."""

    def __init__(self, connection: sqlite3.Connection) -> None:
        self._connection = connection

    def set_metadata(self, key: str, value: str) -> None:
        """Upsert one metadata key inside the caller's transaction."""
        self._connection.execute(
            """
            INSERT INTO metadata(key, value)
            VALUES (?, ?)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            """,
            (key, value),
        )

    def record_owned(self, flight_number: str, *, first_added_at: str, source_key: str) -> None:
        """Record that the daemon added ``flight_number``; a repeat keeps the first timestamp."""
        self._connection.execute(
            """
            INSERT INTO owned_flights(flight_number, first_added_at, source_key)
            VALUES (?, ?, ?)
            ON CONFLICT(flight_number) DO UPDATE SET source_key = excluded.source_key
            """,
            (flight_number, first_added_at, source_key),
        )

    def release_owned(self, flight_number: str) -> None:
        """Forget ownership of ``flight_number`` once it is off the wall."""
        self._connection.execute("DELETE FROM owned_flights WHERE flight_number = ?", (flight_number,))

    def begin_pending_write(
        self, *, desired: Sequence[str], before: Sequence[str] = (), started_at: str
    ) -> None:
        """Journal the list about to be POSTed and the list it replaces. One at a time."""
        existing = self._connection.execute("SELECT 1 FROM pending_writes WHERE singleton = 1").fetchone()
        if existing is not None:
            raise StateError("a pending write is already journaled; resolve it before starting another")
        self._connection.execute(
            "INSERT INTO pending_writes(singleton, desired, before, started_at) VALUES (1, ?, ?, ?)",
            (orjson.dumps(list(desired)).decode(), orjson.dumps(list(before)).decode(), started_at),
        )

    def resolve_pending_write(self) -> None:
        """Clear the journaled write after it has been compared against a fresh read."""
        self._connection.execute("DELETE FROM pending_writes WHERE singleton = 1")


class StateStore:
    """Own the generic state database and its privacy invariants."""

    def __init__(self, path: str | os.PathLike[str]) -> None:
        self.path = Path(path).expanduser()
        self._prepare_directory()
        self._prepare_database_file()

        old_umask = os.umask(0o077)
        try:
            self._connection = sqlite3.connect(self.path)
            self._connection.execute("PRAGMA foreign_keys = ON")
            self._connection.execute("PRAGMA journal_mode = WAL")
            self._bootstrap()
        except sqlite3.Error as error:
            raise StateError(f"unable to initialize state database: {self.path}") from error
        finally:
            os.umask(old_umask)
            self._secure_sqlite_files()

    @property
    def schema_version(self) -> int:
        """Return the schema version recorded in the database."""
        row = self._connection.execute("SELECT version FROM schema_info WHERE singleton = 1").fetchone()
        if row is None:
            raise StateError("state database has no schema version")
        try:
            return int(row[0])
        except (TypeError, ValueError) as error:
            raise StateError("state database has an invalid schema version") from error

    def get_metadata(self, key: str) -> str | None:
        """Return one metadata value, or None when the key is absent."""
        row = self._connection.execute("SELECT value FROM metadata WHERE key = ?", (key,)).fetchone()
        return None if row is None else str(row[0])

    def set_metadata(self, key: str, value: str) -> None:
        """Upsert one metadata key in its own transaction."""
        with self.transaction() as transaction:
            transaction.set_metadata(key, value)

    def owned_flights(self) -> dict[str, OwnedFlight]:
        """Return every flight the daemon owns, keyed by ``flight_number``."""
        rows = self._connection.execute(
            """
            SELECT flight_number, first_added_at, source_key
            FROM owned_flights
            ORDER BY first_added_at, flight_number
            """
        ).fetchall()
        return {str(row[0]): OwnedFlight(str(row[0]), str(row[1]), str(row[2])) for row in rows}

    def pending_write(self) -> PendingWrite | None:
        """Return the journaled write from the last run, if it was never resolved."""
        row = self._connection.execute(
            "SELECT desired, before, started_at FROM pending_writes WHERE singleton = 1"
        ).fetchone()
        if row is None:
            return None
        return PendingWrite(desired=_string_list(row[0]), before=_string_list(row[1]), started_at=str(row[2]))

    @contextmanager
    def transaction(self) -> Generator[StateTransaction, None, None]:
        """Run a block inside BEGIN IMMEDIATE, rolling back on any exception."""
        try:
            self._connection.execute("BEGIN IMMEDIATE")
            transaction = StateTransaction(self._connection)
            yield transaction
        except BaseException:
            self._connection.rollback()
            raise
        else:
            self._connection.commit()
        finally:
            self._secure_sqlite_files()

    def close(self) -> None:
        """Close the connection and re-assert private file modes."""
        self._connection.close()
        self._secure_sqlite_files()

    def __enter__(self) -> Self:
        """Return this store for use as a context manager."""
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        """Close the store when the context exits."""
        self.close()

    def _prepare_directory(self) -> None:
        parent = self.path.parent
        if parent.exists():
            if not parent.is_dir():
                raise StateError(f"state parent is not a directory: {parent}")
            if stat.S_IMODE(parent.stat().st_mode) & 0o077:
                raise StateError(f"state directory must be mode 0700: {parent}")
            return

        parent.mkdir(parents=True, mode=0o700)
        parent.chmod(0o700)

    def _prepare_database_file(self) -> None:
        if self.path.exists():
            if not self.path.is_file():
                raise StateError(f"state path is not a regular file: {self.path}")
            if stat.S_IMODE(self.path.stat().st_mode) & 0o077:
                raise StateError(f"state database must be mode 0600: {self.path}")
            return

        descriptor = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        os.close(descriptor)

    def _bootstrap(self) -> None:
        with self._connection:
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS schema_info (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    version INTEGER NOT NULL
                )
                """
            )
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                )
                """
            )
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS owned_flights (
                    flight_number TEXT PRIMARY KEY,
                    first_added_at TEXT NOT NULL,
                    source_key TEXT NOT NULL
                )
                """
            )
            self._connection.execute(
                """
                CREATE TABLE IF NOT EXISTS pending_writes (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    desired TEXT NOT NULL,
                    before TEXT NOT NULL DEFAULT '[]',
                    started_at TEXT NOT NULL
                )
                """
            )
            self._connection.execute(
                "INSERT OR IGNORE INTO schema_info(singleton, version) VALUES (1, ?)",
                (_SCHEMA_VERSION,),
            )
            # Schema 1 had only metadata; the two tables above are additive, so the upgrade
            # is the CREATE IF NOT EXISTS statements plus the version bump.
            self._connection.execute(
                "UPDATE schema_info SET version = ? WHERE singleton = 1 AND version = 1",
                (_SCHEMA_VERSION,),
            )

        if self.schema_version != _SCHEMA_VERSION:
            raise StateError(
                f"unsupported state schema version {self.schema_version}; expected {_SCHEMA_VERSION}"
            )

    def _secure_sqlite_files(self) -> None:
        for path in (self.path, Path(f"{self.path}-wal"), Path(f"{self.path}-shm")):
            if path.exists():
                path.chmod(0o600)


def _string_list(raw: object) -> tuple[str, ...]:
    decoded: object = orjson.loads(str(raw))
    if not isinstance(decoded, list):
        raise StateError("pending write journal is corrupt")
    items = cast("list[object]", decoded)
    if not all(isinstance(item, str) for item in items):
        raise StateError("pending write journal is corrupt")
    return tuple(cast("list[str]", items))
