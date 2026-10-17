//! Download queue commands.
//!
//! The queue is read and controlled from here; every one of these delegates to
//! the central [`DownloadManager`](crate::downloads::DownloadManager), so the
//! screen and the download engine cannot disagree about what a task is doing.
//! Nothing here transfers anything, and nothing here reaches around the manager
//! to the database.

use serde::Serialize;

use crate::app_state::AppState;
use crate::downloads::DownloadSummary;
use crate::errors::{to_command_error, CommandResult};
use crate::models::DownloadTask;

/// Every download task, newest first.
#[tauri::command]
pub async fn list_downloads(state: tauri::State<'_, AppState>) -> CommandResult<Vec<DownloadTask>> {
    state
        .core()
        .downloads()
        .list()
        .await
        .map_err(to_command_error)
}

/// Counts per download state, for the Downloads screen header.
#[tauri::command]
pub async fn get_download_summary(
    state: tauri::State<'_, AppState>,
) -> CommandResult<DownloadSummary> {
    state
        .core()
        .downloads()
        .summary()
        .await
        .map_err(to_command_error)
}

/// The whole queue in one answer: the tasks and their aggregate.
///
/// One command rather than two, because two calls can straddle a task finishing
/// and report a list and a summary that disagree with each other.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadQueue {
    pub tasks: Vec<DownloadTask>,
    pub summary: DownloadSummary,
}

#[tauri::command]
pub async fn get_download_queue(state: tauri::State<'_, AppState>) -> CommandResult<DownloadQueue> {
    let downloads = state.core().downloads();
    let tasks = downloads.list().await.map_err(to_command_error)?;
    let summary = DownloadSummary::from_tasks(&tasks);
    Ok(DownloadQueue { tasks, summary })
}

/// One task, or `None` when it is no longer in the queue.
#[tauri::command]
pub async fn get_download(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> CommandResult<Option<DownloadTask>> {
    state
        .core()
        .downloads()
        .get(&task_id)
        .await
        .map_err(to_command_error)
}

/// Stop a running transfer and keep what it has written, so it can be resumed.
#[tauri::command]
pub async fn pause_download(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> CommandResult<DownloadTask> {
    state
        .core()
        .downloads()
        .pause(&task_id)
        .await
        .map_err(to_command_error)
}

/// Continue a paused task.
#[tauri::command]
pub async fn resume_download(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> CommandResult<DownloadTask> {
    state
        .core()
        .downloads()
        .resume(&task_id)
        .await
        .map_err(to_command_error)
}

/// Remove a task from the visible queue.
///
/// The queue entry goes; the file it downloaded and the history row it wrote do
/// not. Removing something from a list is not deleting it.
#[tauri::command]
pub async fn remove_download(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> CommandResult<bool> {
    state
        .core()
        .downloads()
        .remove(&task_id)
        .await
        .map_err(to_command_error)
}

/// Clear the completed entries from the visible queue, returning how many went.
/// Ask the filesystem about every finished download.
///
/// Read-only by construction: it opens no file for writing and removes nothing.
/// A screen calls it when it is opened and when the user asks for a rescan, not
/// on every render.
#[tauri::command]
pub async fn scan_downloaded_files(
    state: tauri::State<'_, AppState>,
) -> CommandResult<crate::services::files::FileScan> {
    state.core().scan_downloaded_files().map_err(to_command_error)
}

#[tauri::command]
pub async fn clear_completed_downloads(
    state: tauri::State<'_, AppState>,
) -> CommandResult<usize> {
    state
        .core()
        .downloads()
        .clear_completed()
        .await
        .map_err(to_command_error)
}

/// Re-queue every failed task, returning the ones that were actually re-queued.
#[tauri::command]
pub async fn retry_failed_downloads(
    state: tauri::State<'_, AppState>,
) -> CommandResult<Vec<DownloadTask>> {
    let core = state.core();
    // Every failed task gets the same treatment a single retry does, for the
    // same reason: re-queueing one unchanged asks the source for a resource it
    // has already refused. Best effort, and skipped entirely when the queue
    // cannot be read — the retry itself is not conditional on any of this.
    let failed = core
        .downloads()
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|task| matches!(task.state, crate::models::DownloadState::Failed))
        .collect::<Vec<_>>();
    for task in &failed {
        if let Err(err) = core.refresh_task_media(&task.id).await {
            tracing::warn!(
                target: "single_url",
                task_id = %task.id,
                error = %err.message(),
                "could not refresh the media URLs before retrying; retrying as it stands"
            );
        }
    }

    core.downloads()
        .retry_failed()
        .await
        .map_err(to_command_error)
}
