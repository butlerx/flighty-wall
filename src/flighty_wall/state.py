"""Private, crash-safe SQLite storage without FlightWall contract assumptions."""

from __future__ import annotations

import os
import sqlite3
import stat
from collections.abc import Generator
from contextlib import contextmanager
from pathlib import Path
from types import TracebackType
from typing import Self

_SCHEMA_VERSION = 1


class StateError(RuntimeError):
    """Raised when state storage cannot be opened safely."""


class StateTransaction:
    """Operations available inside one explicit SQLite transaction."""

    def __init__(self, connection: sqlite3.Connection) -> None:
        self._connection = connection

    def set_metadata(self, key: str, value: str) -> None:
        self._connection.execute(
            """
            INSERT INTO metadata(key, value)
            VALUES (?, ?)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            """,
            (key, value),
        )


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
        row = self._connection.execute(
            "SELECT version FROM schema_info WHERE singleton = 1"
        ).fetchone()
        if row is None:
            raise StateError("state database has no schema version")
        try:
            return int(row[0])
        except (TypeError, ValueError) as error:
            raise StateError("state database has an invalid schema version") from error

    def get_metadata(self, key: str) -> str | None:
        row = self._connection.execute(
            "SELECT value FROM metadata WHERE key = ?", (key,)
        ).fetchone()
        return None if row is None else str(row[0])

    def set_metadata(self, key: str, value: str) -> None:
        with self.transaction() as transaction:
            transaction.set_metadata(key, value)

    @contextmanager
    def transaction(self) -> Generator[StateTransaction, None, None]:
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
        self._connection.close()
        self._secure_sqlite_files()

    def __enter__(self) -> Self:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
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
                "INSERT OR IGNORE INTO schema_info(singleton, version) VALUES (1, ?)",
                (_SCHEMA_VERSION,),
            )

        if self.schema_version != _SCHEMA_VERSION:
            raise StateError(
                f"unsupported state schema version {self.schema_version}; "
                f"expected {_SCHEMA_VERSION}"
            )

    def _secure_sqlite_files(self) -> None:
        for path in (self.path, Path(f"{self.path}-wal"), Path(f"{self.path}-shm")):
            if path.exists():
                path.chmod(0o600)
