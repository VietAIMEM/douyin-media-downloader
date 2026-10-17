//! Dashboard commands: what is inside the download folder.
//!
//! Read-only, and narrow on purpose. The window is allowed to *load* media from
//! the configured download folder and nowhere else — the scope is granted here,
//! for exactly that directory, rather than by opening the filesystem to the
//! frontend and trusting it to stay inside.

use std::path::{Path, PathBuf};

use tauri::Manager;

use crate::app_state::AppState;
use crate::errors::{to_command_error, AppError, CommandResult};
use crate::services::dashboard::{FolderContents, FolderListing};

/// Let the window load files from the configured download folder, and nowhere
/// else.
///
/// Called before anything is listed, so the permission always matches the
/// directory the listing came from. Scope grants are not persisted across
/// restarts, which is why this is done on the way in rather than once at
/// startup — and a user who changes their download folder gets the new one
/// without having to know that anything had to be re-granted.
///
/// A failure here is logged and not fatal: the listing is still true, and the
/// worst that follows is a picture that will not render, which the window
/// reports as a failed load rather than as an absence.
pub(crate) fn allow_the_download_folder(app: &tauri::AppHandle) -> String {
    let root = app
        .state::<AppState>()
        .core()
        .settings()
        .current()
        .download_directory;
    if root.trim().is_empty() {
        return root;
    }
    if let Err(err) = app.asset_protocol_scope().allow_directory(&root, true) {
        tracing::warn!(
            target: "dashboard",
            error = %err,
            "the download folder could not be shared with the window"
        );
    }
    // And the thumbnails this application generated, which live in its own data
    // directory rather than among the user's files. Not recursive: every entry
    // is a flat, generated JPEG, and the window has no reason to walk anywhere
    // from here.
    let cache = app.state::<AppState>().core().thumbnails().cache_dir().to_path_buf();
    if let Err(err) = app.asset_protocol_scope().allow_directory(&cache, false) {
        tracing::warn!(
            target: "thumbnails",
            error = %err,
            "the thumbnail cache could not be shared with the window"
        );
    }
    root
}

/// The folders inside the download folder.
#[tauri::command]
pub async fn browse_media_folders(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<FolderListing> {
    allow_the_download_folder(&app);
    state
        .core()
        .browse_media_folders()
        .map_err(to_command_error)
}

/// What is inside one folder: the folders below it, and the media in it.
///
/// The path arrives from the frontend and is checked in the service against the
/// configured root before anything is opened — a path this application did not
/// hand out is refused there, not trusted here.
#[tauri::command]
pub async fn browse_media_folder(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> CommandResult<FolderContents> {
    allow_the_download_folder(&app);
    state
        .core()
        .browse_media_folder(&path)
        .map_err(to_command_error)
}

/// Open one file with whatever the desktop uses for it.
///
/// The path is checked against the configured download folder before anything is
/// opened, so the window cannot ask the desktop to launch something outside it —
/// resolved on both sides, so a link out of the folder is refused rather than
/// followed. The file travels to the opener as a **path**, never assembled into
/// a command line, so a name containing shell characters is a name and nothing
/// else.
#[tauri::command]
pub async fn open_media_file(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> CommandResult<()> {
    let root = PathBuf::from(
        state
            .core()
            .settings()
            .current()
            .download_directory,
    );
    let candidate = Path::new(&path);

    let inside = candidate.is_file()
        && std::fs::canonicalize(candidate)
            .ok()
            .zip(std::fs::canonicalize(&root).ok())
            .map(|(real, real_root)| crate::storage::filenames::is_within(&real_root, &real))
            .unwrap_or(false);
    if !inside {
        return Err(to_command_error(
            AppError::validation("path")
                .with_user_message("That file is not inside the download folder.")
                .with_technical_detail("outside-root"),
        ));
    }

    tauri_plugin_opener::OpenerExt::opener(&app)
        .open_path(candidate.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|err| {
            to_command_error(
                AppError::storage("could not open the file")
                    .with_user_message(
                        "The file could not be opened with its usual application. \
                         You can still open it from the download folder.",
                    )
                    .with_technical_detail(err.to_string()),
            )
        })
}


/// A small preview picture for one file in the download folder.
///
/// A thin adapter: the containment check, the choice of what can be previewed,
/// and the bounded extraction all live in [`AppCore::thumbnail_for`], which is
/// where they can be tested without a window. All this adds is the asset-scope
/// grant, so the picture it returns can actually be loaded.
#[tauri::command]
pub async fn media_thumbnail(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> CommandResult<Option<String>> {
    allow_the_download_folder(&app);
    state
        .core()
        .thumbnail_for(&path)
        .await
        .map_err(to_command_error)
}
