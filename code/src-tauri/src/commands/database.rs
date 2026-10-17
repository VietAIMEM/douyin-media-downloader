//! Database diagnostic commands.

use crate::app_state::AppState;
use crate::database::DatabaseStatus;
use crate::errors::{to_command_error, CommandResult};

/// Health snapshot of the local SQLite database.
///
/// Reports where the database lives, its schema version, which migrations were
/// applied, and row counts per table. Intended for the diagnostics panel and for
/// verifying persistence during development.
#[tauri::command]
pub fn get_database_status(state: tauri::State<'_, AppState>) -> CommandResult<DatabaseStatus> {
    state.core().database().status().map_err(to_command_error)
}
