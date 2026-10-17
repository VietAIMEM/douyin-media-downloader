//! CRUD for the `download_history` table.

use rusqlite::{params, Connection, Row};

use crate::database::row;
use crate::errors::AppResult;
use crate::models::{DownloadHistoryEntry, DownloadState};

const COLUMNS: &str = "id, task_id, post_id, post_url, creator_id, creator_name, media_type, \
     source_url, local_path, file_size, status, error, downloaded_at";

fn map(row: &Row<'_>) -> AppResult<DownloadHistoryEntry> {
    Ok(DownloadHistoryEntry {
        id: row::text(row, 0)?,
        task_id: row::opt_text(row, 1)?,
        post_id: row::opt_text(row, 2)?,
        post_url: row::opt_text(row, 3)?,
        creator_id: row::opt_text(row, 4)?,
        creator_name: row::opt_text(row, 5)?,
        media_type: row::parse_enum(&row::text(row, 6)?, "media_type")?,
        source_url: row::text(row, 7)?,
        local_path: row::opt_text(row, 8)?,
        file_size: row::opt_u64(row, 9)?,
        status: row::parse_enum(&row::text(row, 10)?, "download_state")?,
        error: row::opt_text(row, 11)?,
        downloaded_at: row::timestamp(row, 12, "download_history.downloaded_at")?,
    })
}

/// Append a history entry.
///
/// Idempotent per task: the unique index on `task_id` turns a second write for
/// the same task into a no-op, so two writers that both reach the end of a
/// download cannot produce two rows. (`ON CONFLICT` targets that index
/// specifically, so no other constraint failure is silently swallowed.)
pub fn insert(conn: &Connection, entry: &DownloadHistoryEntry) -> AppResult<()> {
    // One row per task, and the row follows the task's *current* outcome.
    //
    // The uniqueness is what makes two writers racing to finish the same task
    // safe — the database decides, not a check-then-insert that both could pass.
    // What the conflict resolution must not do is keep the first answer for
    // ever. A task that fails and is then retried to success is the *same* task
    // row: retrying resets its state rather than creating a new one. Under
    // `DO NOTHING` the history kept the failure, and the History screen went on
    // reporting a download as failed while the Downloads and Library screens
    // showed the file that had just been fetched. Measured live.
    //
    // Rewriting the row is safe for the race this exists for: two writers
    // finishing one attempt are recording the same outcome, so whichever wins
    // writes the same values.
    conn.execute(
        "INSERT INTO download_history (id, task_id, post_id, post_url, creator_id, creator_name, \
         media_type, source_url, local_path, file_size, status, error, downloaded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
         ON CONFLICT (task_id) DO UPDATE SET \
         post_id = excluded.post_id, post_url = excluded.post_url, \
         creator_id = excluded.creator_id, creator_name = excluded.creator_name, \
         media_type = excluded.media_type, source_url = excluded.source_url, \
         local_path = excluded.local_path, file_size = excluded.file_size, \
         status = excluded.status, error = excluded.error, \
         downloaded_at = excluded.downloaded_at",
        params![
            entry.id,
            entry.task_id,
            entry.post_id,
            entry.post_url,
            entry.creator_id,
            entry.creator_name,
            entry.media_type.as_str(),
            entry.source_url,
            entry.local_path,
            entry.file_size.map(|v| v as i64),
            entry.status.as_str(),
            entry.error,
            row::storing_timestamp(entry.downloaded_at),
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, id: &str) -> AppResult<Option<DownloadHistoryEntry>> {
    let sql = format!("SELECT {COLUMNS} FROM download_history WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

/// Most recent entries first.
pub fn list(conn: &Connection, limit: i64, offset: i64) -> AppResult<Vec<DownloadHistoryEntry>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM download_history ORDER BY downloaded_at DESC LIMIT ?1 OFFSET ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit, offset], |row| Ok(map(row)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

pub fn list_by_status(conn: &Connection, status: DownloadState) -> AppResult<Vec<DownloadHistoryEntry>> {
    let sql = format!("SELECT {COLUMNS} FROM download_history WHERE status = ?1 ORDER BY downloaded_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([status.as_str()], |row| Ok(map(row)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// True when this source URL has already been downloaded successfully.
///
/// Backs duplicate detection for later phases.
pub fn has_successful_download(conn: &Connection, source_url: &str) -> AppResult<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM download_history WHERE source_url = ?1 AND status = 'completed'",
        [source_url],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn count(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM download_history", [], |row| row.get(0))?)
}

/// True when this task already has a history row.
///
/// History is written exactly once per task, but more than one code path can
/// reach the terminal state (the transfer worker, or the manager when a task is
/// cancelled before it starts), so the check is what makes the write idempotent
/// rather than duplicated.
pub fn exists_for_task(conn: &Connection, task_id: &str) -> AppResult<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM download_history WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn delete(conn: &Connection, id: &str) -> AppResult<bool> {
    let affected = conn.execute("DELETE FROM download_history WHERE id = ?1", [id])?;
    Ok(affected > 0)
}

/// Remove every history entry, returning how many were removed.
pub fn clear(conn: &Connection) -> AppResult<usize> {
    Ok(conn.execute("DELETE FROM download_history", [])?)
}
