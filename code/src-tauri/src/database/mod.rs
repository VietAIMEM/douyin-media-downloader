//! SQLite database layer.
//!
//! The [`Database`] type owns a connection pool and nothing else. It exposes a
//! narrow, storage-agnostic surface (`with_conn`, `status`, `schema_version`)
//! that repositories and services build on. No other module constructs a
//! `rusqlite::Connection` directly, which keeps database assumptions in one
//! place and makes the store swappable.

pub mod migrations;
pub mod repositories;
pub mod row;

use std::path::{Path, PathBuf};

use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::TransactionBehavior;
use serde::Serialize;

use crate::errors::{AppError, AppResult};

/// Connection pool type alias.
pub type DbPool = Pool<SqliteConnectionManager>;
/// A checked-out pooled connection.
pub type DbConnection = PooledConnection<SqliteConnectionManager>;

/// Tables the status command reports row counts for.
pub const TRACKED_TABLES: &[&str] = &[
    "creators",
    "posts",
    "media_items",
    "media_variants",
    "downloads",
    "download_history",
    "settings",
];

/// Where the database lives. Kept explicit so `status` can tell a user whether
/// their data is persistent.
#[derive(Debug, Clone)]
pub enum DatabaseLocation {
    File(PathBuf),
    Memory,
}

impl DatabaseLocation {
    pub fn display(&self) -> String {
        match self {
            Self::File(path) => path.to_string_lossy().into_owned(),
            Self::Memory => ":memory:".to_string(),
        }
    }

    pub fn is_persistent(&self) -> bool {
        matches!(self, Self::File(_))
    }
}

/// Handle to the application database.
#[derive(Clone)]
pub struct Database {
    pool: DbPool,
    location: DatabaseLocation,
}

/// How many times a write that lost a race for the database's write lock is
/// retried before it is reported.
///
/// Bounded on purpose. Contention between a handful of connections finishing at
/// once is transient and a retry fixes it; contention that survives four waits
/// is not a race, and pretending otherwise would turn a real problem into a
/// request that never returns.
const LOCK_RETRIES: u32 = 4;

/// How long to wait before the `attempt`-th retry.
///
/// Growing rather than fixed, so a batch of connections that collided does not
/// collide again in step. Short enough that four of them together are still well
/// inside a second.
fn lock_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(25 * u64::from(attempt).pow(2))
}

/// Whether a failure was the database refusing to be written to *at this
/// instant*.
///
/// SQLite says this two ways — `SQLITE_BUSY` when another connection holds the
/// lock, `SQLITE_LOCKED` when the conflict is within one connection — and both
/// say something about *when* the write was attempted rather than about the
/// write. Every other failure is about the write itself and retrying it would
/// only repeat the same mistake.
fn is_lock_contention(err: &AppError) -> bool {
    use rusqlite::ErrorCode;

    std::error::Error::source(err)
        .and_then(|source| source.downcast_ref::<rusqlite::Error>())
        .map(|sqlite| match sqlite {
            rusqlite::Error::SqliteFailure(inner, _) => {
                matches!(inner.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
            }
            _ => false,
        })
        .unwrap_or(false)
}

impl Database {
    /// Open (creating if needed) a file-backed database and bring it up to the
    /// latest schema version.
    pub fn open(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    AppError::storage("could not create the database directory")
                        .with_technical_detail(parent.to_string_lossy().into_owned())
                        .with_source(err)
                })?;
            }
        }

        let manager = SqliteConnectionManager::file(&path).with_init(|conn| {
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA busy_timeout = 5000;",
            )
        });
        Self::from_manager(manager, DatabaseLocation::File(path), 8)
    }

    /// Open a private in-memory database. Used by tests.
    ///
    /// The pool is capped at one connection because SQLite in-memory databases
    /// are per-connection: a second connection would see an empty schema.
    pub fn open_in_memory() -> AppResult<Self> {
        let manager = SqliteConnectionManager::memory().with_init(|conn| {
            conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")
        });
        Self::from_manager(manager, DatabaseLocation::Memory, 1)
    }

    fn from_manager(
        manager: SqliteConnectionManager,
        location: DatabaseLocation,
        max_size: u32,
    ) -> AppResult<Self> {
        let pool = Pool::builder()
            .max_size(max_size)
            .min_idle(Some(1))
            .build(manager)?;

        let db = Self { pool, location };
        // Bring the schema up to date before the handle is handed out, so the
        // rest of the application can assume the tables exist.
        {
            let mut conn = db.get()?;
            let report = migrations::run(&mut conn)?;
            if !report.is_up_to_date() {
                tracing::info!(
                    target: "database",
                    applied = ?report.applied,
                    version = report.current_version,
                    "applied database migrations"
                );
            }
        }
        Ok(db)
    }

    /// Check out a connection from the pool.
    pub fn get(&self) -> AppResult<DbConnection> {
        Ok(self.pool.get()?)
    }

    /// Run `f` with a pooled connection.
    ///
    /// The closure returns [`AppResult`], so `?` works for both domain errors
    /// and `rusqlite` errors (which convert automatically).
    pub fn with_conn<T>(&self, f: impl FnOnce(&DbConnection) -> AppResult<T>) -> AppResult<T> {
        let conn = self.get()?;
        f(&conn)
    }

    /// Run `f` inside a transaction, committing on success and rolling back on
    /// error.
    ///
    /// Needed wherever more than one statement has to be true together. The pool
    /// hands out up to eight connections, so two related writes issued as
    /// separate autocommitting statements can be interleaved by another
    /// connection — and "the task says completed but the history row was never
    /// written" is exactly the kind of half-state that leaves a database that
    /// disagrees with itself.
    ///
    /// Rolling back on error is automatic: `Transaction` rolls back on drop
    /// unless it was committed.
    ///
    /// # Why the lock is taken up front
    ///
    /// The transaction begins **immediately** rather than deferring the write
    /// lock to the first write. A deferred transaction in WAL mode reads from a
    /// snapshot and, if another connection writes before it does, cannot be
    /// upgraded: SQLite refuses with `SQLITE_BUSY` *without consulting the busy
    /// handler*, so no timeout can help and the write fails at once. Eight
    /// connections finishing carousel downloads at the same moment is exactly
    /// that pattern. Taking the lock at `BEGIN` turns the problem back into
    /// ordinary waiting, which `busy_timeout` already handles.
    ///
    /// `f` may be run more than once, so it must be a function of the database's
    /// contents rather than of anything it changes on the way. Every caller here
    /// re-reads the row it is about to write inside the closure, so a second run
    /// sees the same thing and reaches the same conclusion.
    pub fn with_transaction<T>(
        &self,
        f: impl Fn(&rusqlite::Connection) -> AppResult<T>,
    ) -> AppResult<T> {
        let mut attempt = 0u32;
        loop {
            let mut conn = self.get()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

            let value = match f(&tx) {
                Ok(value) => value,
                // The transaction is dropped here, which rolls it back, so the
                // next attempt starts from the database rather than from a
                // half-finished one.
                Err(err) if is_lock_contention(&err) && attempt < LOCK_RETRIES => {
                    drop(tx);
                    attempt += 1;
                    std::thread::sleep(lock_backoff(attempt));
                    tracing::warn!(
                        target: "database",
                        attempt,
                        "the database was busy; retrying the write"
                    );
                    continue;
                }
                Err(err) => return Err(err),
            };

            tx.commit()?;
            return Ok(value);
        }
    }

    pub fn location(&self) -> &DatabaseLocation {
        &self.location
    }

    /// Current schema version stored in the database.
    pub fn schema_version(&self) -> AppResult<i64> {
        self.with_conn(|conn| migrations::current_version(conn))
    }

    /// The newest schema version this build knows about.
    pub fn latest_schema_version(&self) -> i64 {
        migrations::MIGRATIONS
            .last()
            .map(|m| m.version)
            .unwrap_or(0)
    }

    /// Snapshot of database health for the `get_database_status` command.
    pub fn status(&self) -> AppResult<DatabaseStatus> {
        self.with_conn(|conn| {
            let schema_version = migrations::current_version(conn)?;
            let applied = migrations::applied_versions(conn)?
                .into_iter()
                .map(|(version, description, applied_at)| AppliedMigration {
                    version,
                    description,
                    applied_at,
                })
                .collect();

            let journal_mode: String = conn
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap_or_else(|_| "unknown".to_string());
            let foreign_keys: bool = conn
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .map(|v| v != 0)
                .unwrap_or(false);

            let mut table_counts = Vec::with_capacity(TRACKED_TABLES.len());
            for table in TRACKED_TABLES {
                // SAFETY: `table` comes from the compile-time TRACKED_TABLES
                // list, never from user input, so interpolation is safe here
                // (SQLite cannot bind identifiers as parameters).
                let count: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                    .unwrap_or(0);
                table_counts.push(TableCount {
                    table: (*table).to_string(),
                    rows: count,
                });
            }

            Ok(DatabaseStatus {
                healthy: true,
                location: self.location.display(),
                is_persistent: self.location.is_persistent(),
                schema_version,
                latest_schema_version: migrations::MIGRATIONS
                    .last()
                    .map(|m| m.version)
                    .unwrap_or(0),
                applied_migrations: applied,
                table_counts,
                journal_mode,
                foreign_keys,
            })
        })
    }
}

/// Health/diagnostic snapshot of the database, returned to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStatus {
    pub healthy: bool,
    /// Path on disk, or `:memory:` for a temporary database.
    pub location: String,
    pub is_persistent: bool,
    pub schema_version: i64,
    pub latest_schema_version: i64,
    pub applied_migrations: Vec<AppliedMigration>,
    pub table_counts: Vec<TableCount>,
    pub journal_mode: String,
    pub foreign_keys: bool,
}

impl DatabaseStatus {
    /// True when the stored schema matches the newest known migration.
    pub fn is_current(&self) -> bool {
        self.schema_version >= self.latest_schema_version
    }
}

/// One recorded migration.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedMigration {
    pub version: i64,
    pub description: String,
    pub applied_at: String,
}

/// Row count for a single table.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableCount {
    pub table: String,
    pub rows: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_database_is_migrated_on_open() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), db.latest_schema_version());
        assert!(!db.location().is_persistent());
    }

    #[test]
    fn status_reports_healthy_and_empty() {
        let db = Database::open_in_memory().unwrap();
        let status = db.status().unwrap();
        assert!(status.healthy);
        assert!(status.is_current());
        assert!(status.foreign_keys);
        assert_eq!(status.table_counts.len(), TRACKED_TABLES.len());
        for count in &status.table_counts {
            assert_eq!(count.rows, 0, "table {} should start empty", count.table);
        }
    }

    #[test]
    fn all_tracked_tables_exist() {
        let db = Database::open_in_memory().unwrap();
        db.with_conn(|conn| {
            for table in TRACKED_TABLES {
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )?;
                assert_eq!(exists, 1, "missing table {table}");
            }
            Ok(())
        })
        .unwrap();
    }

    // --- lock contention -----------------------------------------------------

    /// The error SQLite raises when another connection holds the write lock.
    fn busy() -> AppError {
        AppError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        ))
    }

    #[test]
    fn a_write_that_lost_a_race_is_tried_again_rather_than_reported() {
        // The shape of the failure this exists for: several downloads finishing
        // at once, one of them reaching the write lock while another holds it.
        // Losing that race is not an answer about the download, and reporting it
        // as one is how a finished file came to be recorded as a failure.
        let db = Database::open_in_memory().unwrap();
        let attempts = std::cell::Cell::new(0);

        let outcome = db.with_transaction(|conn| {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 3 {
                return Err(busy());
            }
            conn.execute_batch("CREATE TABLE IF NOT EXISTS retried (id INTEGER)")?;
            Ok(attempts.get())
        });

        assert_eq!(outcome.unwrap(), 3, "the third attempt is the one that ran");
        assert_eq!(attempts.get(), 3);
        // And the retries left nothing behind: the transaction that failed was
        // rolled back, so the schema change is from the attempt that succeeded.
        db.with_conn(|conn| {
            let exists: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='retried'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(exists, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn contention_that_does_not_clear_is_reported_after_a_bounded_number_of_tries() {
        // A retry is for a race, not for a database that is genuinely stuck. The
        // bound is what keeps this from becoming a request that never returns.
        let db = Database::open_in_memory().unwrap();
        let attempts = std::cell::Cell::new(0);

        let err = db
            .with_transaction(|_| {
                attempts.set(attempts.get() + 1);
                Err::<(), _>(busy())
            })
            .expect_err("persistent contention is a failure");

        assert_eq!(err.kind(), crate::errors::AppErrorKind::Database);
        assert_eq!(
            attempts.get(),
            LOCK_RETRIES + 1,
            "the first attempt plus the bounded retries, and no more"
        );
    }

    #[test]
    fn a_failure_that_is_not_contention_is_not_retried() {
        // The other half of the rule. Retrying a malformed statement would only
        // repeat it, four times, with a growing delay before each.
        let db = Database::open_in_memory().unwrap();
        let attempts = std::cell::Cell::new(0);

        let err = db
            .with_transaction(|conn| {
                attempts.set(attempts.get() + 1);
                conn.execute_batch("THIS IS NOT SQL")?;
                Ok(())
            })
            .expect_err("a syntax error is not a race");

        assert_eq!(err.kind(), crate::errors::AppErrorKind::Database);
        assert_eq!(attempts.get(), 1, "it was tried once and reported");
    }

    #[test]
    fn a_write_waits_out_a_lock_another_connection_holds() {
        // The real path, with a real lock: another connection holds the write
        // lock for a moment and the transaction waits rather than failing. That
        // is what beginning immediately buys — with a deferred transaction the
        // upgrade would have been refused outright.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("contended.db");
        let db = Database::open(&path).unwrap();

        let holder = rusqlite::Connection::open(&path).unwrap();
        holder
            .execute_batch("PRAGMA busy_timeout = 5000; BEGIN IMMEDIATE;")
            .unwrap();

        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            holder.execute_batch("COMMIT;").unwrap();
        });

        let outcome = db.with_transaction(|conn| {
            conn.execute_batch("CREATE TABLE IF NOT EXISTS waited (id INTEGER)")?;
            Ok("written")
        });

        release.join().unwrap();
        assert_eq!(outcome.unwrap(), "written");
    }
}
