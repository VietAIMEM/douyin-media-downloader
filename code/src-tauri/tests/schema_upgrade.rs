//! Upgrading a database that a previous version wrote.
//!
//! Migrations are forward-only and are recorded per version, so the two things
//! that have to be true are: a database written by an older build ends up on
//! the current schema with its rows intact, and a run that was interrupted part
//! way leaves nothing recorded that it did not actually do. Both are checked
//! here against real files, opened through the same `Database::open` the
//! application uses — not against a connection built by the test.

use std::path::Path;

use douyin_downloader_lib::database::migrations::{self, MIGRATIONS};
use douyin_downloader_lib::database::Database;
use rusqlite::{params, Connection};

/// Write a database as the build that ended at `through` would have left it.
///
/// Applying a prefix of the real migration list is what an older release did;
/// the SQL and the bookkeeping rows are the same ones, so the file this
/// produces is indistinguishable from one that release wrote.
fn write_old_database(path: &Path, through: i64) {
    let conn = Connection::open(path).expect("the database file must open");
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS schema_migrations (
             version     INTEGER PRIMARY KEY,
             description TEXT NOT NULL,
             applied_at  TEXT NOT NULL
         );",
    )
    .expect("the bookkeeping table must be created");

    for migration in MIGRATIONS.iter().filter(|m| m.version <= through) {
        conn.execute_batch(migration.sql)
            .unwrap_or_else(|err| panic!("migration {} must apply: {err}", migration.version));
        conn.execute(
            "INSERT INTO schema_migrations (version, description, applied_at) VALUES (?1, ?2, ?3)",
            params![
                migration.version,
                migration.description,
                "2026-01-01T00:00:00Z"
            ],
        )
        .expect("the migration must be recorded");
    }
}

fn count(db: &Database, table: &str) -> i64 {
    db.with_conn(|conn| {
        Ok(
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })?,
        )
    })
    .unwrap_or_else(|err| panic!("counting {table} must work: {err}"))
}

fn columns(db: &Database, table: &str) -> Vec<String> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
    .unwrap_or_else(|err| panic!("reading the columns of {table} must work: {err}"))
}

fn index_exists(db: &Database, name: &str) -> bool {
    db.with_conn(|conn| {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )? > 0)
    })
    .unwrap_or_else(|err| panic!("reading sqlite_master must work: {err}"))
}

fn seed_content(path: &Path) {
    let conn = Connection::open(path).expect("the database file must open");
    conn.execute_batch(
        "INSERT INTO creators (id, platform, platform_id, display_name, profile_url, created_at, updated_at)
           VALUES ('creator-1', 'douyin', 'sec-1', 'A Creator', 'https://example.invalid/creator', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
         INSERT INTO posts (id, platform, platform_post_id, creator_id, url, media_type, status, created_at, updated_at)
           VALUES ('post-1', 'douyin', 'p-1', 'creator-1', 'https://example.invalid/p/1', 'video', 'available', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
         INSERT INTO media_items (id, post_id, media_type, source_url, created_at)
           VALUES ('item-1', 'post-1', 'video', 'https://cdn.invalid/1.mp4', '2026-01-01T00:00:00Z');
         INSERT INTO media_variants (id, media_item_id, kind, label, source_url)
           VALUES ('variant-1', 'item-1', 'video', '720p', 'https://cdn.invalid/1-720.mp4');
         INSERT INTO downloads (id, post_id, media_item_id, variant_id, source_url, media_type, state, created_at, updated_at)
           VALUES ('download-1', 'post-1', 'item-1', 'variant-1', 'https://cdn.invalid/1-720.mp4', 'video', 'completed', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
         INSERT INTO download_history (id, task_id, post_id, media_type, source_url, local_path, status, downloaded_at)
           VALUES ('history-1', 'download-1', 'post-1', 'video', 'https://cdn.invalid/1-720.mp4', 'D:\\Media\\1.mp4', 'completed', '2026-01-01T00:00:00Z');
         INSERT INTO settings (key, value, updated_at) VALUES ('theme', 'dark', '2026-01-01T00:00:00Z');
         INSERT INTO settings (key, value, updated_at) VALUES ('maxConcurrentDownloads', '7', '2026-01-01T00:00:00Z');",
    )
    .expect("the fixture rows must be written");
}

#[test]
fn a_database_from_an_earlier_schema_upgrades_with_its_rows_intact() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("douyin-downloader.db");

    write_old_database(&path, 1);
    seed_content(&path);

    // What the application does on launch.
    let db = Database::open(&path).expect("the database must open and migrate");

    assert_eq!(
        db.schema_version().expect("a schema version"),
        db.latest_schema_version(),
        "the database must end up on the newest schema this build knows"
    );
    assert_eq!(db.latest_schema_version(), MIGRATIONS.len() as i64);

    for (table, expected) in [
        ("creators", 1),
        ("posts", 1),
        ("media_items", 1),
        ("media_variants", 1),
        ("downloads", 1),
        ("download_history", 1),
        ("settings", 2),
    ] {
        assert_eq!(
            count(&db, table),
            expected,
            "{table} lost rows in the upgrade"
        );
    }

    // The columns the later migrations add are present, and the rows that
    // predate them carry nulls rather than having been dropped and recreated.
    let download_columns = columns(&db, "downloads");
    assert!(
        download_columns.contains(&"extension".to_string()),
        "downloads.extension is missing after the upgrade: {download_columns:?}"
    );
    assert!(
        download_columns.contains(&"audio_source_url".to_string()),
        "downloads.audio_source_url is missing after the upgrade: {download_columns:?}"
    );

    let value: String = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT value FROM settings WHERE key = 'maxConcurrentDownloads'",
                [],
                |row| row.get::<_, String>(0),
            )?)
        })
        .expect("the setting must still be readable");
    assert_eq!(value, "7", "a setting written by the old build changed");

    for index in [
        "idx_history_task_id",
        "idx_downloads_post_created",
        "idx_history_post_id_local_path",
    ] {
        assert!(
            index_exists(&db, index),
            "{index} is missing after the upgrade"
        );
    }
}

#[test]
fn the_upgrade_deduplicates_history_and_keeps_the_earliest_row() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("douyin-downloader.db");

    // Schema 2 is the last one before history rows were made one-per-task.
    write_old_database(&path, 2);
    {
        let conn = Connection::open(&path).expect("the database file must open");
        conn.execute_batch(
            "INSERT INTO download_history (id, task_id, media_type, source_url, status, downloaded_at)
               VALUES ('first',  'task-1', 'video', 'https://cdn.invalid/a.mp4', 'completed', '2026-01-01T00:00:00Z');
             INSERT INTO download_history (id, task_id, media_type, source_url, status, downloaded_at)
               VALUES ('second', 'task-1', 'video', 'https://cdn.invalid/a.mp4', 'failed',    '2026-01-02T00:00:00Z');
             INSERT INTO download_history (id, task_id, media_type, source_url, status, downloaded_at)
               VALUES ('third',  'task-1', 'video', 'https://cdn.invalid/a.mp4', 'completed', '2026-01-03T00:00:00Z');
             INSERT INTO download_history (id, task_id, media_type, source_url, status, downloaded_at)
               VALUES ('loose',  NULL,     'video', 'https://cdn.invalid/b.mp4', 'completed', '2026-01-04T00:00:00Z');",
        )
        .expect("the duplicate rows must be written");
    }

    let db = Database::open(&path).expect("the database must open and migrate");

    assert_eq!(
        count(&db, "download_history"),
        2,
        "the upgrade must leave one row per task, plus the row that has no task"
    );

    let kept: String = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT id FROM download_history WHERE task_id = 'task-1'",
                [],
                |row| row.get::<_, String>(0),
            )?)
        })
        .expect("the surviving row must be readable");
    assert_eq!(
        kept, "first",
        "the upgrade must keep the earliest row for a task, not an arbitrary one"
    );

    assert!(
        index_exists(&db, "idx_history_task_id"),
        "the unique index the migration exists for was not created"
    );

    // And it really is unique: a second row for the same task is refused.
    let rejected = db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO download_history (id, task_id, media_type, source_url, status, downloaded_at)
             VALUES ('later', 'task-1', 'video', 'https://cdn.invalid/a.mp4', 'completed', '2026-01-05T00:00:00Z')",
            [],
        )
        .map_err(|err| err.into())
    });
    assert!(
        rejected.is_err(),
        "history accepted a second row for a task after the upgrade"
    );
}

#[test]
fn a_migration_that_was_interrupted_is_not_recorded_and_is_applied_next_time() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("douyin-downloader.db");

    let last = MIGRATIONS.last().expect("at least one migration");
    write_old_database(&path, last.version - 1);
    seed_content(&path);

    // A run that reached the final migration and then died: its statements are
    // rolled back and nothing about it is recorded, which is what the
    // transaction in `migrations::run` is for.
    {
        let mut conn = Connection::open(&path).expect("the database file must open");
        let tx = conn.transaction().expect("a transaction");
        tx.execute_batch(last.sql)
            .expect("the last migration must apply");
        drop(tx);

        let recorded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
                [last.version],
                |row| row.get(0),
            )
            .expect("the bookkeeping rows must be readable");
        assert_eq!(recorded, 0, "an uncommitted migration must not be recorded");
    }

    let db = Database::open(&path).expect("the database must open and migrate");
    assert_eq!(
        db.schema_version().expect("a schema version"),
        last.version,
        "the interrupted migration was not applied on the next run"
    );
    assert_eq!(
        count(&db, "creators"),
        1,
        "the rows written before the interruption did not survive"
    );
    assert!(
        index_exists(&db, "idx_downloads_post_created"),
        "the migration's own work is missing after the retry"
    );
}

#[test]
fn every_recorded_migration_is_one_this_build_knows() {
    // The failure this rules out is a database that claims a schema version the
    // running build has no migration for — an older build opening a newer
    // database. It would then read columns whose meaning it does not know.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("douyin-downloader.db");

    let db = Database::open(&path).expect("a fresh database must open");
    let applied = db
        .with_conn(|conn| migrations::applied_versions(conn))
        .expect("the applied migrations must be readable");

    let known: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
    let recorded: Vec<i64> = applied.iter().map(|(version, ..)| *version).collect();
    assert_eq!(
        recorded, known,
        "a fresh database must record exactly the migrations this build has"
    );

    // Reopening changes nothing: no migration runs twice.
    drop(db);
    let reopened = Database::open(&path).expect("the database must reopen");
    assert_eq!(
        reopened.schema_version().expect("a schema version"),
        known.last().copied().unwrap_or(0)
    );
    assert_eq!(count(&reopened, "settings"), 0);
}

/// The migration list itself, so a failure points at the list rather than at
/// one of the tests above.
#[test]
fn the_migrations_are_a_contiguous_run_from_one() {
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let due = index as i64 + 1;
        assert_eq!(
            migration.version, due,
            "migrations must run 1..=n with no gaps; found {} where {due} was due",
            migration.version
        );
    }
}
