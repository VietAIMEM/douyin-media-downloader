//! Download history commands.
//!
//! History is a read-only record of finished downloads. Phase 01 wires the
//! query path so the screen is real; rows appear once Phase 04 performs
//! transfers.

use crate::app_state::AppState;
use crate::database::repositories::history as history_repo;
use crate::errors::{to_command_error, CommandResult};
use crate::models::DownloadHistoryEntry;

/// Newest entries first. `limit` is clamped so a frontend bug cannot ask for
/// an unbounded result set.
#[tauri::command]
pub fn list_history(
    state: tauri::State<'_, AppState>,
    limit: Option<i64>,
) -> CommandResult<Vec<DownloadHistoryEntry>> {
    let limit = limit.unwrap_or(200).clamp(1, 1000);
    state
        .core()
        .database()
        .with_conn(|conn| history_repo::list(conn, limit, 0))
        .map_err(to_command_error)
}

/// Total number of recorded downloads.
#[tauri::command]
pub fn get_history_count(state: tauri::State<'_, AppState>) -> CommandResult<i64> {
    state
        .core()
        .database()
        .with_conn(|conn| history_repo::count(conn))
        .map_err(to_command_error)
}
