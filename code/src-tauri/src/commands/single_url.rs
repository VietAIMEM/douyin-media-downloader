//! Single URL workflow commands.
//!
//! Thin adapters over [`crate::services::single_url`]. Each one unwraps managed
//! state, calls exactly one core operation, and converts the result to the wire
//! format — no network, parsing, database, or filesystem logic lives here.

use std::path::Path;

use crate::app_state::AppState;
use crate::errors::{to_command_error, AppError, AppResult, CommandResult};
use crate::models::DownloadTask;
use crate::services::single_url::{AnalyzedUrl, CreateDownloadInput};
use crate::storage::filenames::is_within;

/// Analyze a pasted link: validate, expand, classify, resolve, persist.
///
/// A profile link is a *successful* analysis (`intent = "profile"`) rather than
/// an error, because the link was understood correctly — it simply is not a
/// downloadable post. Genuine failures keep their typed kind, so the UI can tell
/// a bad URL from an unreachable source from unavailable content.
#[tauri::command]
pub async fn analyze_url(
    state: tauri::State<'_, AppState>,
    url: String,
) -> CommandResult<AnalyzedUrl> {
    state
        .core()
        .analyze_url(&url)
        .await
        .map_err(to_command_error)
}

/// Create a download task for one media variant of an analyzed post.
///
/// The request carries ids only; the media URL is read from the database by the
/// application service, so the frontend cannot choose what gets fetched.
#[tauri::command]
pub async fn create_download_task(
    state: tauri::State<'_, AppState>,
    input: CreateDownloadInput,
) -> CommandResult<DownloadTask> {
    state
        .core()
        .create_download(input)
        .await
        .map_err(to_command_error)
}

/// Cancel a queued, running, or paused download.
///
/// The transfer itself is signalled to stop, not merely hidden from the list.
#[tauri::command]
pub async fn cancel_download(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> CommandResult<DownloadTask> {
    state
        .core()
        .downloads()
        .cancel(&task_id)
        .await
        .map_err(to_command_error)
}

/// Re-queue a failed or cancelled download.
#[tauri::command]
pub async fn retry_download(
    state: tauri::State<'_, AppState>,
    task_id: String,
) -> CommandResult<DownloadTask> {
    let core = state.core();
    // A retry of a task whose media URLs were refused has to be a *different*
    // request, or it fails the same way it did before — which is what made
    // "Retry" useless here and left re-analysing the link as the only way
    // forward. Refreshing first is best effort: a source that is simply down is
    // then retried exactly as it stood.
    if let Err(err) = core.refresh_task_media(&task_id).await {
        tracing::warn!(
            target: "single_url",
            task_id = %task_id,
            error = %err.message(),
            "could not refresh the media URLs before retrying; retrying as it stands"
        );
    }
    core.downloads()
        .retry(&task_id)
        .await
        .map_err(to_command_error)
}

/// Open a completed download, or the folder containing it.
///
/// `kind` is `file` or `folder`. The path is read from the database — never
/// supplied by the caller — and is re-checked to be inside the configured
/// download directory before anything is handed to the operating system.
#[tauri::command]
pub async fn open_download_path(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
    task_id: String,
    kind: String,
) -> CommandResult<()> {
    let core = state.core();
    let task = core
        .downloads()
        .get(&task_id)
        .await
        .map_err(to_command_error)?
        .ok_or_else(|| {
            to_command_error(
                AppError::not_found("download task", &task_id)
                    .with_user_message("That download is no longer in the list."),
            )
        })?;

    let path = resolve_openable_path(core, &task).map_err(to_command_error)?;
    let path_string = path.to_string_lossy().into_owned();

    let opened = match kind.as_str() {
        "folder" => tauri_plugin_opener::OpenerExt::opener(&app).reveal_item_in_dir(&path),
        _ => tauri_plugin_opener::OpenerExt::opener(&app)
            .open_path(path_string.clone(), None::<&str>),
    };

    opened.map_err(|err| {
        to_command_error(
            AppError::storage("could not open the downloaded file")
                .with_user_message("The file could not be opened.")
                .with_technical_detail(err.to_string()),
        )
    })
}

/// Validate that a task's destination is a real file inside the download root.
fn resolve_openable_path(core: &crate::services::AppCore, task: &DownloadTask) -> AppResult<std::path::PathBuf> {
    let Some(raw) = task.destination_path.as_deref() else {
        return Err(AppError::not_found("download file", &task.id)
            .with_user_message("This download has not produced a file yet."));
    };

    let root = core.settings().current().download_directory;
    let root = Path::new(&root);
    let path = Path::new(raw);

    if !is_within(root, path) {
        return Err(AppError::storage("refusing to open a path outside the download folder")
            .with_user_message("That file is outside the download folder.")
            .with_technical_detail(raw.to_string()));
    }
    if !path.exists() {
        return Err(AppError::not_found("download file", raw)
            .with_user_message("The downloaded file is no longer on disk."));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppPaths;
    use crate::models::{DownloadState, MediaType};

    fn core_with_download_dir(dir: &Path) -> (tempfile::TempDir, crate::services::AppCore) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_data_dir(temp.path().to_path_buf());
        let core = crate::services::AppCore::bootstrap(paths).unwrap().core;
        core.settings()
            .update(|s| {
                s.download_directory = dir.to_string_lossy().into_owned();
                Ok(())
            })
            .unwrap();
        (temp, core)
    }

    fn task_with_path(path: Option<&str>) -> DownloadTask {
        let mut task = DownloadTask::new(
            &crate::models::DownloadRequest::new("https://cdn/v.mp4", MediaType::Video),
        );
        task.state = DownloadState::Completed;
        task.destination_path = path.map(str::to_string);
        task
    }

    #[test]
    fn a_task_without_a_file_is_not_openable() {
        let dir = tempfile::tempdir().unwrap();
        let (_temp, core) = core_with_download_dir(dir.path());
        let err = resolve_openable_path(&core, &task_with_path(None)).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::NotFound);
    }

    #[test]
    fn a_path_outside_the_download_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("outside.mp4");
        std::fs::write(&outside, b"x").unwrap();

        let (_temp, core) = core_with_download_dir(dir.path());
        let err = resolve_openable_path(&core, &task_with_path(Some(&outside.to_string_lossy())))
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
    }

    #[test]
    fn a_traversal_attempt_is_refused_even_when_the_root_prefix_matches() {
        let dir = tempfile::tempdir().unwrap();
        let escaped = dir.path().join("..").join("escaped.mp4");
        let (_temp, core) = core_with_download_dir(dir.path());
        let err = resolve_openable_path(&core, &task_with_path(Some(&escaped.to_string_lossy())))
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
    }

    #[test]
    fn a_real_file_inside_the_download_folder_is_openable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("clip.mp4");
        std::fs::write(&file, b"data").unwrap();

        let (_temp, core) = core_with_download_dir(dir.path());
        let resolved =
            resolve_openable_path(&core, &task_with_path(Some(&file.to_string_lossy()))).unwrap();
        assert_eq!(resolved, file);
    }

    #[test]
    fn a_missing_file_inside_the_folder_is_reported_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("deleted.mp4");
        let (_temp, core) = core_with_download_dir(dir.path());
        let err = resolve_openable_path(&core, &task_with_path(Some(&missing.to_string_lossy())))
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::NotFound);
    }
}
