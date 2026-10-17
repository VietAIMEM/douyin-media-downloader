//! Schema migrations.
//!
//! Migrations are an ordered list of immutable, forward-only steps. Each step
//! is applied inside a transaction and recorded in `schema_migrations`, so an
//! interrupted run can simply be retried.
//!
//! To evolve the schema, append a new [`Migration`] with the next version
//! number and a new SQL file. Never edit an already-released migration: some
//! users will already have it recorded as applied.

use rusqlite::Connection;

use crate::errors::{AppError, AppResult};

/// One forward-only schema step.
pub struct Migration {
    /// Monotonic version. Must strictly increase across the list.
    pub version: i64,
    /// Short human-readable description, stored for diagnostics.
    pub description: &'static str,
    /// SQL executed as a single batch inside a transaction.
    pub sql: &'static str,
}

/// The complete, ordered migration list.
///
/// The SQL lives in `migrations/*.sql` at the repository root so it can be
/// reviewed as plain SQL; `include_str!` embeds it into the binary so the
/// shipped application has no runtime file dependency.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema",
        sql: include_str!("../../../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        description: "download extension column",
        sql: include_str!("../../../migrations/0002_download_extension.sql"),
    },
    Migration {
        version: 3,
        description: "one history row per task",
        sql: include_str!("../../../migrations/0003_history_unique_task.sql"),
    },
    Migration {
        version: 4,
        description: "download audio source column",
        sql: include_str!("../../../migrations/0004_download_audio_source.sql"),
    },
    Migration {
        version: 5,
        description: "library read indexes",
        sql: include_str!("../../../migrations/0005_library_indexes.sql"),
    },
];

/// Result of running migrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Versions applied during this run (empty when already up to date).
    pub applied: Vec<i64>,
    /// Schema version after the run.
    pub current_version: i64,
}

impl MigrationReport {
    pub fn is_up_to_date(&self) -> bool {
        self.applied.is_empty()
    }
}

/// Create the migration bookkeeping table if it does not exist yet.
fn ensure_migration_table(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version     INTEGER PRIMARY KEY,
             description TEXT NOT NULL,
             applied_at  TEXT NOT NULL
         );",
    )?;
    Ok(())
}

/// Highest applied migration version, or `0` when the database is empty.
pub fn current_version(conn: &Connection) -> AppResult<i64> {
    ensure_migration_table(conn)?;
    let version: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    Ok(version.unwrap_or(0))
}

/// Apply every migration newer than the recorded version.
pub fn run(conn: &mut Connection) -> AppResult<MigrationReport> {
    ensure_migration_table(conn)?;
    let start_version = current_version(conn)?;
    let mut applied = Vec::new();

    for migration in MIGRATIONS {
        if migration.version <= start_version {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql).map_err(|err| {
            AppError::database(format!("migration {} failed", migration.version))
                .with_technical_detail(format!("{}: {err}", migration.description))
                .with_source(err)
        })?;
        tx.execute(
            "INSERT INTO schema_migrations (version, description, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                migration.version,
                migration.description,
                crate::models::common::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
        applied.push(migration.version);
    }

    Ok(MigrationReport {
        current_version: current_version(conn)?,
        applied,
    })
}

/// Versions recorded as applied, oldest first.
pub fn applied_versions(conn: &Connection) -> AppResult<Vec<(i64, String, String)>> {
    ensure_migration_table(conn)?;
    let mut stmt = conn
        .prepare("SELECT version, description, applied_at FROM schema_migrations ORDER BY version")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_ordered_and_unique() {
        let mut last = 0;
        for m in MIGRATIONS {
            assert!(
                m.version > last,
                "migration versions must strictly increase: {} after {}",
                m.version,
                last
            );
            last = m.version;
            assert!(!m.description.is_empty());
            assert!(!m.sql.trim().is_empty());
        }
    }

    #[test]
    fn run_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        let first = run(&mut conn).unwrap();
        assert_eq!(first.current_version, MIGRATIONS.len() as i64);
        assert!(!first.is_up_to_date());

        let second = run(&mut conn).unwrap();
        assert!(second.is_up_to_date());
        assert_eq!(second.current_version, first.current_version);
    }
}
