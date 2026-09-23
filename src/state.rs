//! Private, crash-safe `SQLite` storage: the ownership journal and one pending write.
//!
//! The wall carries no ownership signal (see `docs/flightwall-api.md` §4), so this
//! journal is the only record of which `flight_number` entries the daemon added. A
//! `flight_number` absent from `owned_flights` is the owner's and is never removed.
//!
//! Every query is checked against the schema at compile time (`sqlx::query!`), using the
//! metadata in `.sqlx/`. After changing a query or a migration, run `mise run sqlx:prepare`
//! to regenerate that directory and commit it.

use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions},
    {Sqlite, Transaction},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::runtime::Runtime;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Raised when state storage cannot be opened safely or an invariant would break.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state parent is not a directory: {0}")]
    ParentNotDirectory(PathBuf),
    #[error("state directory must be mode 0700: {0}")]
    ParentTooOpen(PathBuf),
    #[error("state path is not a regular file: {0}")]
    NotRegularFile(PathBuf),
    #[error("state database must be mode 0600: {0}")]
    FileTooOpen(PathBuf),
    #[error("state filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("state database error at {path}: {source}")]
    Sqlx {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },
    #[error("state database migration failed at {path}: {source}")]
    Migrate {
        path: PathBuf,
        #[source]
        source: sqlx::migrate::MigrateError,
    },
    #[error("a pending write is already journaled; resolve it before starting another")]
    PendingWriteExists,
    #[error("pending write journal is corrupt")]
    CorruptJournal,
}

/// One `flight_number` the daemon added, and the calendar key that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFlight {
    pub flight_number: String,
    pub first_added_at: String,
    pub source_key: String,
}

/// The desired list the daemon was about to POST, and what the wall held just before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    pub desired: Vec<String>,
    pub before: Vec<String>,
    pub started_at: String,
}

/// Own the generic state database and its privacy invariants.
///
/// The store is synchronous to its callers. `sqlx` is async, so a single-threaded tokio
/// runtime lives inside and every call is `block_on`. `SQLite` itself runs on `sqlx`'s own
/// worker thread, so nothing here needs a multi-threaded executor.
#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    runtime: Runtime,
    pool: SqlitePool,
}

impl StateStore {
    /// Open (creating if needed) the private state database at `path` and bring its
    /// schema up to date.
    ///
    /// # Errors
    ///
    /// [`StateError`] when the directory or file exists with loose permissions, cannot be
    /// created, or a migration fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref().to_owned();
        prepare_directory(&path)?;
        prepare_database_file(&path)?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|source| StateError::Io {
                path: path.clone(),
                source,
            })?;
        let pool = runtime
            .block_on(
                SqlitePoolOptions::new()
                    .max_connections(1)
                    .connect_with(connect_options(&path)),
            )
            .map_err(|source| StateError::Sqlx {
                path: path.clone(),
                source,
            })?;
        runtime
            .block_on(MIGRATOR.run(&pool))
            .map_err(|source| StateError::Migrate {
                path: path.clone(),
                source,
            })?;

        let store = Self {
            path,
            runtime,
            pool,
        };
        store.secure_sqlite_files()?;
        Ok(store)
    }

    /// The path this store was opened at.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The newest migration applied to this database.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn schema_version(&self) -> Result<i64, StateError> {
        self.sql(sqlx::query_scalar!(
            r#"SELECT version AS "version!: i64" FROM _sqlx_migrations ORDER BY version DESC LIMIT 1"#
        )
        .fetch_one(&self.pool))
    }

    /// One metadata value, or `None` when the key is absent.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
        self.sql(
            sqlx::query_scalar!("SELECT value FROM metadata WHERE key = ?1", key)
                .fetch_optional(&self.pool),
        )
    }

    /// Upsert one metadata key in its own transaction.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        self.transaction(|tx| tx.set_metadata(key, value))
    }

    /// Every flight the daemon owns, keyed by `flight_number`.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn owned_flights(&self) -> Result<BTreeMap<String, OwnedFlight>, StateError> {
        let rows = self.sql(
            sqlx::query_as!(
                OwnedFlight,
                r#"SELECT flight_number AS "flight_number!", first_added_at, source_key
                   FROM owned_flights
                   ORDER BY first_added_at, flight_number"#
            )
            .fetch_all(&self.pool),
        )?;
        Ok(rows
            .into_iter()
            .map(|flight| (flight.flight_number.clone(), flight))
            .collect())
    }

    /// The journaled write from the last run, if it was never resolved.
    ///
    /// # Errors
    ///
    /// [`StateError::CorruptJournal`] when the stored lists are not JSON string arrays.
    pub fn pending_write(&self) -> Result<Option<PendingWrite>, StateError> {
        let row = self.sql(
            sqlx::query!(
                r#"SELECT desired, "before" AS before, started_at FROM pending_writes WHERE singleton = 1"#
            )
            .fetch_optional(&self.pool),
        )?;
        row.map(|row| {
            Ok(PendingWrite {
                desired: string_list(&row.desired)?,
                before: string_list(&row.before)?,
                started_at: row.started_at,
            })
        })
        .transpose()
    }

    /// Run `body` inside one transaction; commit on `Ok`, roll back on `Err`.
    ///
    /// # Errors
    ///
    /// Whatever `body` returns, or [`StateError::Sqlx`] if the transaction itself fails.
    pub fn transaction<T>(
        &self,
        body: impl FnOnce(&Tx<'_>) -> Result<T, StateError>,
    ) -> Result<T, StateError> {
        let inner = self.sql(self.pool.begin())?;
        let tx = Tx {
            inner: RefCell::new(inner),
            runtime: &self.runtime,
            path: &self.path,
        };
        // Both arms consume the transaction inside `block_on`: the pooled connection is
        // handed back on drop via a spawned task, which needs a runtime to be current.
        let result = match body(&tx) {
            Ok(value) => self.sql(tx.inner.into_inner().commit()).map(|()| value),
            Err(error) => {
                self.sql(tx.inner.into_inner().rollback())?;
                Err(error)
            }
        };
        // Re-assert private modes whether or not the body succeeded: SQLite may have
        // created the WAL and SHM siblings on first write.
        self.secure_sqlite_files()?;
        result
    }

    fn secure_sqlite_files(&self) -> Result<(), StateError> {
        secure_sqlite_files(&self.path)
    }

    /// Drive one query to completion and attach the path to any failure.
    fn sql<T>(&self, future: impl Future<Output = sqlx::Result<T>>) -> Result<T, StateError> {
        self.runtime
            .block_on(future)
            .map_err(|source| StateError::Sqlx {
                path: self.path.clone(),
                source,
            })
    }
}

/// Operations available inside one explicit `SQLite` transaction.
#[derive(Debug)]
pub struct Tx<'a> {
    inner: RefCell<Transaction<'static, Sqlite>>,
    runtime: &'a Runtime,
    path: &'a Path,
}

impl Tx<'_> {
    /// Upsert one metadata key inside the caller's transaction.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        let mut tx = self.inner.borrow_mut();
        self.sql(
            sqlx::query!(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                key,
                value
            )
            .execute(&mut **tx),
        )
        .map(drop)
    }

    /// Record that the daemon added `flight_number`; a repeat keeps the first timestamp.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn record_owned(
        &self,
        flight_number: &str,
        first_added_at: &str,
        source_key: &str,
    ) -> Result<(), StateError> {
        let mut tx = self.inner.borrow_mut();
        self.sql(
            sqlx::query!(
                "INSERT INTO owned_flights(flight_number, first_added_at, source_key)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(flight_number) DO UPDATE SET source_key = excluded.source_key",
                flight_number,
                first_added_at,
                source_key
            )
            .execute(&mut **tx),
        )
        .map(drop)
    }

    /// Forget ownership of `flight_number` once it is off the wall.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn release_owned(&self, flight_number: &str) -> Result<(), StateError> {
        let mut tx = self.inner.borrow_mut();
        self.sql(
            sqlx::query!(
                "DELETE FROM owned_flights WHERE flight_number = ?1",
                flight_number
            )
            .execute(&mut **tx),
        )
        .map(drop)
    }

    /// Every owned `flight_number`, as seen from inside this transaction.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn owned_flight_numbers(&self) -> Result<Vec<String>, StateError> {
        let mut tx = self.inner.borrow_mut();
        self.sql(
            sqlx::query_scalar!(
                r#"SELECT flight_number AS "flight_number!" FROM owned_flights ORDER BY flight_number"#
            )
            .fetch_all(&mut **tx),
        )
    }

    /// Journal the list about to be `POSTed` and the list it replaces. One at a time.
    ///
    /// # Errors
    ///
    /// [`StateError::PendingWriteExists`] when one is already journaled.
    pub fn begin_pending_write(
        &self,
        desired: &[String],
        before: &[String],
        started_at: &str,
    ) -> Result<(), StateError> {
        let mut tx = self.inner.borrow_mut();
        let existing = self.sql(
            sqlx::query_scalar!(
                r#"SELECT singleton AS "singleton!: i64" FROM pending_writes WHERE singleton = 1"#
            )
            .fetch_optional(&mut **tx),
        )?;
        if existing.is_some() {
            return Err(StateError::PendingWriteExists);
        }
        let desired = json_list(desired);
        let before = json_list(before);
        self.sql(
            sqlx::query!(
                r#"INSERT INTO pending_writes(singleton, desired, "before", started_at)
                   VALUES (1, ?1, ?2, ?3)"#,
                desired,
                before,
                started_at
            )
            .execute(&mut **tx),
        )
        .map(drop)
    }

    /// Clear the journaled write after it has been compared against a fresh read.
    ///
    /// # Errors
    ///
    /// [`StateError::Sqlx`] on a database failure.
    pub fn resolve_pending_write(&self) -> Result<(), StateError> {
        let mut tx = self.inner.borrow_mut();
        self.sql(sqlx::query!("DELETE FROM pending_writes WHERE singleton = 1").execute(&mut **tx))
            .map(drop)
    }

    fn sql<T>(&self, future: impl Future<Output = sqlx::Result<T>>) -> Result<T, StateError> {
        self.runtime
            .block_on(future)
            .map_err(|source| StateError::Sqlx {
                path: self.path.to_owned(),
                source,
            })
    }
}

fn connect_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        // The file is created above with mode 0600; sqlx must not race us to it.
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
}

fn json_list(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_owned())
}

fn string_list(raw: &str) -> Result<Vec<String>, StateError> {
    serde_json::from_str::<Vec<String>>(raw).map_err(|_| StateError::CorruptJournal)
}

// ---------------------------------------------------------------------------------------
// Filesystem invariants.
// ---------------------------------------------------------------------------------------

fn prepare_directory(path: &Path) -> Result<(), StateError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    match fs::metadata(parent) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(StateError::ParentNotDirectory(parent.to_owned()));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(StateError::ParentTooOpen(parent.to_owned()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .and_then(|()| fs::set_permissions(parent, fs::Permissions::from_mode(0o700)))
            .map_err(|source| StateError::Io {
                path: parent.to_owned(),
                source,
            }),
        Err(source) => Err(StateError::Io {
            path: parent.to_owned(),
            source,
        }),
    }
}

fn prepare_database_file(path: &Path) -> Result<(), StateError> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(StateError::NotRegularFile(path.to_owned()));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(StateError::FileTooOpen(path.to_owned()));
            }
            Ok(())
        }
        // An empty file is a valid, empty SQLite database.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map(drop)
            .map_err(|source| StateError::Io {
                path: path.to_owned(),
                source,
            }),
        Err(source) => Err(StateError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn secure_sqlite_files(path: &Path) -> Result<(), StateError> {
    let name = path.to_string_lossy();
    for candidate in [
        path.to_owned(),
        PathBuf::from(format!("{name}-wal")),
        PathBuf::from(format!("{name}-shm")),
    ] {
        if candidate.exists() {
            fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600)).map_err(
                |source| StateError::Io {
                    path: candidate.clone(),
                    source,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for the private state database and its journal invariants.

    use tempfile::TempDir;

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn state_path(dir: &TempDir) -> PathBuf {
        dir.path().join("state").join("state.sqlite3")
    }

    /// Run raw SQL against the store's pool: for seeding and inspecting, never for the
    /// contract under test.
    fn raw(store: &StateStore, sql: &str) {
        store
            .runtime
            .block_on(sqlx::raw_sql(sql).execute(&store.pool))
            .unwrap();
    }

    fn tables(store: &StateStore) -> Vec<String> {
        let mut names: Vec<String> = store
            .runtime
            .block_on(
                sqlx::query_scalar(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )
                .fetch_all(&store.pool),
            )
            .unwrap();
        names.sort();
        names
    }

    #[test]
    fn metadata_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(state_path(&dir)).unwrap();
        store
            .set_metadata("last_authoritative_sync", "2026-09-21T12:00:00Z")
            .unwrap();
        drop(store);

        let reopened = StateStore::open(state_path(&dir)).unwrap();

        assert_eq!(reopened.schema_version().unwrap(), 1);
        assert_eq!(
            reopened
                .get_metadata("last_authoritative_sync")
                .unwrap()
                .as_deref(),
            Some("2026-09-21T12:00:00Z")
        );
    }

    #[test]
    fn transaction_rolls_back_on_failure() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(state_path(&dir)).unwrap();
        store.set_metadata("status", "before").unwrap();

        let result = store.transaction(|tx| {
            tx.set_metadata("status", "after")?;
            Err::<(), _>(StateError::CorruptJournal)
        });

        assert!(matches!(result, Err(StateError::CorruptJournal)));
        assert_eq!(
            store.get_metadata("status").unwrap().as_deref(),
            Some("before")
        );
    }

    #[test]
    fn state_files_and_directory_are_private() {
        let dir = TempDir::new().unwrap();
        let path = state_path(&dir);
        let store = StateStore::open(&path).unwrap();
        store.set_metadata("key", "value").unwrap();

        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        for suffix in ["-wal", "-shm"] {
            let sibling = PathBuf::from(format!("{}{suffix}", path.display()));
            if sibling.exists() {
                assert_eq!(mode(&sibling), 0o600, "{suffix}");
            }
        }
    }

    #[test]
    fn existing_insecure_state_directory_is_rejected() {
        let dir = TempDir::new().unwrap();
        let state_dir = dir.path().join("state");
        fs::create_dir(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let error = StateStore::open(state_dir.join("state.sqlite3")).unwrap_err();

        assert!(error.to_string().contains("0700"), "{error}");
    }

    #[test]
    fn existing_insecure_database_file_is_rejected() {
        let dir = TempDir::new().unwrap();
        let path = state_path(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path.parent().unwrap())
            .unwrap();
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let error = StateStore::open(&path).unwrap_err();

        assert!(error.to_string().contains("0600"), "{error}");
    }

    #[test]
    fn bootstrap_creates_only_the_contract_tables_and_the_migration_ledger() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(state_path(&dir)).unwrap();

        // No mode-lease or remote-id tables: the wall has neither. Ownership is by flight_number.
        assert_eq!(
            tables(&store),
            [
                "_sqlx_migrations",
                "metadata",
                "owned_flights",
                "pending_writes"
            ]
        );
    }

    #[test]
    fn reopen_does_not_rerun_migrations() {
        let dir = TempDir::new().unwrap();
        drop(StateStore::open(state_path(&dir)).unwrap());
        let store = StateStore::open(state_path(&dir)).unwrap();

        let applied: i64 = store
            .runtime
            .block_on(
                sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations").fetch_one(&store.pool),
            )
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(store.schema_version().unwrap(), 1);
    }

    #[test]
    fn a_database_from_the_previous_daemon_build_opens_in_place() {
        // The earlier daemon created these tables itself and tracked its schema in
        // `schema_info`. The migration must adopt that database, not fight it.
        let dir = TempDir::new().unwrap();
        let path = state_path(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path.parent().unwrap())
            .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            use sqlx::ConnectOptions as _;
            let mut conn = connect_options(&path).connect().await.unwrap();
            sqlx::raw_sql(
                "
                CREATE TABLE schema_info (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    version INTEGER NOT NULL
                );
                CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE owned_flights (
                    flight_number TEXT PRIMARY KEY,
                    first_added_at TEXT NOT NULL,
                    source_key TEXT NOT NULL
                );
                CREATE TABLE pending_writes (
                    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                    desired TEXT NOT NULL,
                    before TEXT NOT NULL DEFAULT '[]',
                    started_at TEXT NOT NULL
                );
                INSERT INTO schema_info VALUES (1, 2);
                INSERT INTO metadata VALUES ('kept', 'yes');
                INSERT INTO owned_flights VALUES ('BA5', 't', 'k');
                ",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            sqlx::Connection::close(conn).await.unwrap();
        });

        let store = StateStore::open(&path).unwrap();

        assert_eq!(store.schema_version().unwrap(), 1);
        assert_eq!(store.get_metadata("kept").unwrap().as_deref(), Some("yes"));
        assert_eq!(
            store.owned_flights().unwrap().keys().collect::<Vec<_>>(),
            ["BA5"]
        );
        assert!(tables(&store).contains(&"_sqlx_migrations".to_owned()));
    }

    #[test]
    fn owned_flights_round_trip_and_are_keyed_by_flight_number() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(state_path(&dir)).unwrap();

        store
            .transaction(|tx| {
                tx.record_owned("VY8721", "2026-09-22T10:00:00Z", "VY8721:DUB:2026-10-24")?;
                tx.record_owned("BA5", "2026-09-22T10:00:00Z", "BA5:LHR:2026-10-30")?;
                tx.record_owned("BA5", "2026-09-22T10:00:00Z", "BA5:LHR:2026-10-30")
            })
            .unwrap();

        let owned = store.owned_flights().unwrap();
        assert_eq!(owned.keys().collect::<Vec<_>>(), ["BA5", "VY8721"]);
        assert_eq!(owned["BA5"].source_key, "BA5:LHR:2026-10-30");

        store.transaction(|tx| tx.release_owned("BA5")).unwrap();

        assert_eq!(
            store.owned_flights().unwrap().keys().collect::<Vec<_>>(),
            ["VY8721"]
        );
    }

    #[test]
    fn pending_write_is_durable_until_resolved() {
        let dir = TempDir::new().unwrap();
        let path = state_path(&dir);
        let store = StateStore::open(&path).unwrap();

        store
            .transaction(|tx| {
                tx.begin_pending_write(&["EI61".into(), "BA5".into()], &[], "2026-09-22T10:00:00Z")
            })
            .unwrap();
        drop(store);

        let reopened = StateStore::open(&path).unwrap();
        let pending = reopened.pending_write().unwrap().expect("pending write");
        assert_eq!(pending.desired, ["EI61", "BA5"]);
        assert!(pending.before.is_empty());
        assert_eq!(pending.started_at, "2026-09-22T10:00:00Z");

        // `Tx::resolve_pending_write` as a bare path does not satisfy the higher-ranked
        // `for<'a> FnOnce(&Tx<'a>)` bound; the closure form does.
        #[allow(clippy::redundant_closure_for_method_calls)]
        let resolved = reopened.transaction(|tx| tx.resolve_pending_write());
        resolved.unwrap();

        assert_eq!(reopened.pending_write().unwrap(), None);
    }

    #[test]
    fn only_one_pending_write_may_exist() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(state_path(&dir)).unwrap();
        store
            .transaction(|tx| tx.begin_pending_write(&["EI61".into()], &[], "t1"))
            .unwrap();

        let second = store.transaction(|tx| tx.begin_pending_write(&["BA5".into()], &[], "t2"));

        assert!(
            matches!(second, Err(StateError::PendingWriteExists)),
            "{second:?}"
        );
        let pending = store.pending_write().unwrap().expect("first write kept");
        assert_eq!(pending.desired, ["EI61"]);
    }

    #[test]
    fn corrupt_journal_is_reported_not_trusted() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::open(state_path(&dir)).unwrap();
        raw(
            &store,
            "INSERT INTO pending_writes(singleton, desired, \"before\", started_at) VALUES (1, 'nope', '[]', 't')",
        );

        assert!(matches!(
            store.pending_write(),
            Err(StateError::CorruptJournal)
        ));
    }
}
