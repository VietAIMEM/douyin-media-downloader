//! CRUD for the `downloads` table.
//!
//! Download rows are operational records: they are independent of posts and
//! media items (no foreign keys) so that a task keeps a meaningful history
//! even if the underlying content rows are removed.

use rusqlite::{params, Connection, Row};

use crate::database::row;
use crate::errors::AppResult;
use crate::models::{DownloadState, DownloadTask};

const COLUMNS: &str = "id, post_id, media_item_id, variant_id, source_url, audio_source_url, \
     destination_path, file_name, extension, media_type, state, attempts, max_attempts, \
     downloaded_bytes, total_bytes, error, created_at, updated_at, started_at, completed_at";

fn map(row: &Row<'_>) -> AppResult<DownloadTask> {
    Ok(DownloadTask {
        id: row::text(row, 0)?,
        post_id: row::opt_text(row, 1)?,
        media_item_id: row::opt_text(row, 2)?,
        variant_id: row::opt_text(row, 3)?,
        source_url: row::text(row, 4)?,
        audio_source_url: row::opt_text(row, 5)?,
        destination_path: row::opt_text(row, 6)?,
        file_name: row::opt_text(row, 7)?,
        extension: row::opt_text(row, 8)?,
        media_type: row::parse_enum(&row::text(row, 9)?, "media_type")?,
        state: row::parse_enum(&row::text(row, 10)?, "download_state")?,
        attempts: row::uint(row, 11)?,
        max_attempts: row::uint(row, 12)?,
        progress: crate::models::DownloadProgress::new(row::u64_val(row, 13)?, row::opt_u64(row, 14)?),
        error: row::opt_text(row, 15)?,
        created_at: row::timestamp(row, 16, "downloads.created_at")?,
        updated_at: row::timestamp(row, 17, "downloads.updated_at")?,
        started_at: row::opt_timestamp(row, 18, "downloads.started_at")?,
        completed_at: row::opt_timestamp(row, 19, "downloads.completed_at")?,
    })
}

/// Insert a new download task.
pub fn insert(conn: &Connection, task: &DownloadTask) -> AppResult<()> {
    conn.execute(
        "INSERT INTO downloads (id, post_id, media_item_id, variant_id, source_url, \
         audio_source_url, destination_path, file_name, extension, media_type, state, attempts, \
         max_attempts, downloaded_bytes, total_bytes, error, created_at, updated_at, started_at, \
         completed_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            task.id,
            task.post_id,
            task.media_item_id,
            task.variant_id,
            task.source_url,
            task.audio_source_url,
            task.destination_path,
            task.file_name,
            task.extension,
            task.media_type.as_str(),
            task.state.as_str(),
            task.attempts as i64,
            task.max_attempts as i64,
            task.progress.downloaded_bytes as i64,
            task.progress.total_bytes.map(|v| v as i64),
            task.error,
            row::storing_timestamp(task.created_at),
            row::storing_timestamp(task.updated_at),
            task.started_at.map(row::timestamp_to_sql),
            task.completed_at.map(row::timestamp_to_sql),
        ],
    )?;
    Ok(())
}

/// Update a mutable download task (state, progress, attempts, timestamps).
pub fn update(conn: &Connection, task: &DownloadTask) -> AppResult<bool> {
    let affected = conn.execute(
        "UPDATE downloads SET destination_path = ?2, file_name = ?3, extension = ?4, state = ?5, \
         attempts = ?6, max_attempts = ?7, downloaded_bytes = ?8, total_bytes = ?9, error = ?10, \
         updated_at = ?11, started_at = ?12, completed_at = ?13 WHERE id = ?1",
        params![
            task.id,
            task.destination_path,
            task.file_name,
            task.extension,
            task.state.as_str(),
            task.attempts as i64,
            task.max_attempts as i64,
            task.progress.downloaded_bytes as i64,
            task.progress.total_bytes.map(|v| v as i64),
            task.error,
            row::storing_timestamp(task.updated_at),
            task.started_at.map(row::timestamp_to_sql),
            task.completed_at.map(row::timestamp_to_sql),
        ],
    )?;
    Ok(affected > 0)
}

/// Re-point a task at refreshed media URLs, and nothing else.
///
/// Deliberately narrow, in the same spirit as [`record_attempt`]. A Douyin media
/// URL is signed and short-lived, and when the signature ages out every
/// candidate for a track is refused together — so a retry has to be able to ask
/// for the *current* URL rather than the one that was refused. Before this
/// existed there was no way to change a task's resource at all: [`update`]
/// writes the state, the attempts and the destination, but never the URL, so a
/// task could only ever be retried against the resource that had already failed.
///
/// Only the columns naming the resource and the rendition it came from. The
/// task's state, its attempt count and where it is writing belong to the
/// scheduler, and a refresh must not be able to disturb them.
pub fn update_media(conn: &Connection, task: &DownloadTask) -> AppResult<bool> {
    let affected = conn.execute(
        "UPDATE downloads SET source_url = ?2, audio_source_url = ?3, media_item_id = ?4, \
         variant_id = ?5, extension = ?6, updated_at = ?7 WHERE id = ?1",
        params![
            task.id,
            task.source_url,
            task.audio_source_url,
            task.media_item_id,
            task.variant_id,
            task.extension,
            row::storing_timestamp(task.updated_at),
        ],
    )?;
    Ok(affected > 0)
}

/// Record how many attempts a task has had, and nothing else.
///
/// Deliberately not a read-modify-write. [`update`] writes whatever the caller
/// last read — *including the state* — so a worker that re-read a task and wrote
/// it back can silently undo a `pause` or a `cancel` that landed in between. The
/// transfer worker records an attempt at the start of every try, which is
/// exactly when a user is most likely to be pausing or cancelling it, so this
/// writes the single column it owns and leaves every other one alone.
pub fn record_attempt(
    conn: &Connection,
    id: &str,
    attempt: u32,
    updated_at: crate::models::common::Timestamp,
) -> AppResult<()> {
    conn.execute(
        "UPDATE downloads SET attempts = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, attempt as i64, row::timestamp_to_sql(updated_at)],
    )?;
    Ok(())
}

/// Update a task only while it is still in `expected`.
///
/// A compare-and-set in a single statement. The plain [`update`] writes
/// whatever the caller last read, so a `SELECT`-then-`UPDATE` pair lets a
/// concurrent writer — a `cancel`, a `pause`, another worker — commit in
/// between and be overwritten. That is not theoretical here: the download
/// manager and the transfer workers are separate tasks sharing a pool of eight
/// connections, and a cancelled task being silently returned to `Downloading`
/// is a user-visible bug, not a tidiness problem.
///
/// Returns whether the row was actually changed. `false` means somebody else
/// moved the task first, and the caller must not treat its own write as having
/// happened.
pub fn update_if_state(
    conn: &Connection,
    task: &DownloadTask,
    expected: DownloadState,
) -> AppResult<bool> {
    let affected = conn.execute(
        "UPDATE downloads SET destination_path = ?2, file_name = ?3, extension = ?4, state = ?5, \
         attempts = ?6, max_attempts = ?7, downloaded_bytes = ?8, total_bytes = ?9, error = ?10, \
         updated_at = ?11, started_at = ?12, completed_at = ?13 WHERE id = ?1 AND state = ?14",
        params![
            task.id,
            task.destination_path,
            task.file_name,
            task.extension,
            task.state.as_str(),
            task.attempts as i64,
            task.max_attempts as i64,
            task.progress.downloaded_bytes as i64,
            task.progress.total_bytes.map(|v| v as i64),
            task.error,
            row::storing_timestamp(task.updated_at),
            task.started_at.map(row::timestamp_to_sql),
            task.completed_at.map(row::timestamp_to_sql),
            expected.as_str(),
        ],
    )?;
    Ok(affected > 0)
}

pub fn get(conn: &Connection, id: &str) -> AppResult<Option<DownloadTask>> {    let sql = format!("SELECT {COLUMNS} FROM downloads WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

pub fn list(conn: &Connection, limit: i64, offset: i64) -> AppResult<Vec<DownloadTask>> {
    let sql = format!("SELECT {COLUMNS} FROM downloads ORDER BY created_at DESC LIMIT ?1 OFFSET ?2");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit, offset], |row| Ok(map(row)))?;
    collect(rows)
}

/// The tasks that recorded any of these paths, keyed by the path they recorded.
///
/// How a file on disk is matched to what the application knows about it: by the
/// path *this application wrote down when it saved the file*, which is a fact
/// rather than a guess. A file renamed outside the application simply has no
/// match, and that is the honest answer — matching on the file name instead
/// would attach one download's caption to another's bytes as soon as two posts
/// produced similar names.
/// The completed downloads whose destination is one of `paths`.
///
/// Keyed by [`comparison_key`](crate::storage::filenames::comparison_key) rather
/// than by the stored text, because the two sides are spelled by different
/// things: the database holds what the download recorded, and the caller holds
/// what the filesystem reports. On Windows those can differ in case and in
/// separator and still name the same file.
///
/// The normalisation has to happen on **both** sides or it achieves nothing —
/// the row must be found by the query before it can be compared to anything —
/// which is why it is done here rather than left to the caller.
pub fn by_destination_paths(
    conn: &Connection,
    paths: &[std::path::PathBuf],
) -> AppResult<std::collections::HashMap<String, DownloadTask>> {
    let mut found = std::collections::HashMap::new();
    if paths.is_empty() {
        return Ok(found);
    }
    let keys: Vec<String> = paths
        .iter()
        .map(|path| crate::storage::filenames::comparison_key(path))
        .collect();

    let placeholders = (1..=keys.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {COLUMNS} FROM downloads WHERE {} IN ({placeholders})",
        crate::storage::filenames::COMPARISON_KEY_SQL
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(keys.iter()), |row| Ok(map(row)))?;
    for task in collect(rows)? {
        for destination in task.destination_path.iter() {
            found.insert(
                crate::storage::filenames::comparison_key(std::path::Path::new(destination)),
                task.clone(),
            );
        }
    }
    Ok(found)
}

/// Every completed task that recorded a path for what it produced.
///
/// These are the rows worth asking the filesystem about, and no others. A task
/// still running has not finished writing anything, so asking whether its file
/// is there would answer "no" about a download that is doing exactly what it
/// should. A task that *failed* never produced a file either, so asking the same
/// question would flag it "file missing" — which reads as "the file you had is
/// gone", when in truth there never was one. A failed download and a download
/// whose file has vanished are different things, and only one of them is
/// answered by looking at the disk.
pub fn completed_with_a_destination(conn: &Connection) -> AppResult<Vec<DownloadTask>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM downloads \
         WHERE destination_path IS NOT NULL AND state = 'completed' \
         ORDER BY completed_at DESC, created_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| Ok(map(row)))?;
    collect(rows)
}

/// Tasks that are not in a terminal state, oldest first (queue order).
pub fn list_pending(conn: &Connection) -> AppResult<Vec<DownloadTask>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM downloads \
         WHERE state IN ('queued', 'downloading', 'paused') ORDER BY created_at"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| Ok(map(row)))?;
    collect(rows)
}

pub fn list_by_state(conn: &Connection, state: DownloadState) -> AppResult<Vec<DownloadTask>> {
    let sql = format!("SELECT {COLUMNS} FROM downloads WHERE state = ?1 ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([state.as_str()], |row| Ok(map(row)))?;
    collect(rows)
}

/// The most recent task recorded for a post, whatever state it is in.
///
/// This is what a profile listing reads to say whether a post it has just
/// discovered is new, downloading, already downloaded, or previously failed.
/// "Most recent" rather than "successful" because a later failure is the more
/// truthful description of a post whose first attempt worked and whose retry
/// did not.
pub fn latest_for_post(conn: &Connection, post_id: &str) -> AppResult<Option<DownloadTask>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM downloads WHERE post_id = ?1 \
         ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([post_id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

/// The latest task for a media item that has not finished yet, if any.
///
/// Backs duplicate detection: submitting the same media twice must not queue a
/// second transfer beside one that is still running.
pub fn active_for_media(
    conn: &Connection,
    media_item_id: &str,
) -> AppResult<Option<DownloadTask>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM downloads WHERE media_item_id = ?1 \
         AND state IN ('queued', 'downloading', 'paused') \
         ORDER BY created_at DESC, rowid DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([media_item_id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

/// The latest task for a media item that finished successfully.
pub fn completed_for_media(
    conn: &Connection,
    media_item_id: &str,
) -> AppResult<Option<DownloadTask>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM downloads WHERE media_item_id = ?1 AND state = 'completed' \
         ORDER BY completed_at DESC, rowid DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([media_item_id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

pub fn count(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM downloads", [], |row| row.get(0))?)
}

pub fn delete(conn: &Connection, id: &str) -> AppResult<bool> {
    let affected = conn.execute("DELETE FROM downloads WHERE id = ?1", [id])?;
    Ok(affected > 0)
}

/// Remove all tasks in a terminal state, returning how many were removed.
pub fn clear_terminal(conn: &Connection) -> AppResult<usize> {
    Ok(conn.execute(
        "DELETE FROM downloads WHERE state IN ('completed', 'failed', 'cancelled')",
        [],
    )?)
}

/// Remove the tasks in one state, returning how many were removed.
///
/// This is a *list* operation: the queue rows go, the files they produced and
/// the history they wrote do not. Clearing what the screen is showing is not
/// the same as deleting what was downloaded.
pub fn clear_state(conn: &Connection, state: DownloadState) -> AppResult<usize> {
    Ok(conn.execute(
        "DELETE FROM downloads WHERE state = ?1",
        [state.as_str()],
    )?)
}

/// Convenience helper so callers do not need to know a media type literal.
pub fn count_completed(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM downloads WHERE state = 'completed'",
        [],
        |row| row.get(0),
    )?)
}

fn collect(rows: impl Iterator<Item = rusqlite::Result<AppResult<DownloadTask>>>) -> AppResult<Vec<DownloadTask>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}
