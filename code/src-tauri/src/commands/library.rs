//! Library commands.
//!
//! Thin, like every command: unwrap managed state, call one core operation,
//! convert the result. The interesting logic — status reconciliation, derived
//! filters, export serialization — lives in
//! [`services::library`](crate::services::library), where it can be tested
//! without a Tauri runtime.

use serde::Serialize;

use crate::app_state::AppState;
use crate::errors::{to_command_error, AppError, CommandResult};
use crate::models::LibraryPage;
use crate::services::{ExportFormat, ExportResult, ExportScope, LibraryQuery};

/// One creator in the Library's filter menu.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCreator {
    pub id: String,
    pub name: String,
}

/// Run a Library query: one view, with search, filters, sorting and a page.
///
/// The query arrives as a raw value and is deserialized here rather than by
/// Tauri's own argument binding. That is deliberate: a bad date filter is a
/// *validation* failure the user can act on, and Tauri's binder would turn it
/// into an untyped string the screen could only display as-is.
#[tauri::command]
pub async fn query_library(
    state: tauri::State<'_, AppState>,
    query: serde_json::Value,
) -> CommandResult<LibraryPage> {
    let query = parse_query(query)?;
    state.core().query_library(query).map_err(to_command_error)
}

/// Deserialize a library query, reporting a shape failure as a typed error.
fn parse_query(value: serde_json::Value) -> CommandResult<LibraryQuery> {
    serde_json::from_value(value).map_err(|err| {
        let detail = err.to_string();
        // The date parser and the JSON parser both fail here, and they mean
        // different things to a user, so they get different messages.
        let error = if detail.contains("invalid date filter") {
            AppError::validation("library query")
                .with_user_message("That date filter is not a valid date.")
        } else {
            AppError::validation("library query")
                .with_user_message("The library filters could not be understood.")
        };
        to_command_error(error.with_technical_detail(detail))
    })
}

/// The creators the Library holds, for the filter menu.
#[tauri::command]
pub async fn list_library_creators(
    state: tauri::State<'_, AppState>,
) -> CommandResult<Vec<LibraryCreator>> {
    state
        .core()
        .library_creators()
        .map(|creators| {
            creators
                .into_iter()
                .map(|(id, name)| LibraryCreator { id, name })
                .collect()
        })
        .map_err(to_command_error)
}

/// Show an exported file in the system file manager.
///
/// The path arrives from the frontend, so it is checked rather than trusted: it
/// must resolve to an existing file inside the configured download folder, which
/// is the only place this application writes. Without that check a compromised
/// or buggy frontend could hand the shell any path on the machine.
#[tauri::command]
pub async fn reveal_export(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> CommandResult<()> {
    let root = state
        .core()
        .settings()
        .download_directory_checked()
        .map_err(to_command_error)?;

    let candidate = std::path::Path::new(&path);
    // Both sides canonicalized: the plain check is lexical and would not see a
    // link that points out of the folder.
    let inside = candidate.is_file()
        && std::fs::canonicalize(candidate)
            .ok()
            .zip(std::fs::canonicalize(&root).ok())
            .map(|(real, real_root)| crate::storage::filenames::is_within(&real_root, &real))
            .unwrap_or(false);
    if !inside {
        return Err(to_command_error(
            AppError::validation("export path")
                .with_user_message("That export is not inside the download folder."),
        ));
    }

    tauri_plugin_opener::OpenerExt::opener(&app)
        .reveal_item_in_dir(candidate)
        .map_err(|err| {
            to_command_error(
                AppError::storage("could not reveal the export")
                    .with_user_message("The export folder could not be opened.")
                    .with_technical_detail(err.to_string()),
            )
        })
}

/// Write a post's text to the download folder as a UTF-8 `.txt` file.
///
/// Not a download: no task, no queue, no history row. A text post has nothing to
/// transfer, and its words are written straight from what is already held.
#[tauri::command]
pub async fn save_post_text(
    state: tauri::State<'_, AppState>,
    post_id: String,
) -> CommandResult<crate::services::text::SavedText> {
    crate::services::text::save_post_text(state.core(), &post_id).map_err(to_command_error)
}

/// Write the matching records to JSON or CSV.
///
/// Returns where the file went, because the destination is chosen by the
/// application (inside the download folder) rather than by a file dialog the
/// frontend cannot open.
#[tauri::command]
pub async fn export_library(
    state: tauri::State<'_, AppState>,
    query: serde_json::Value,
    format: ExportFormat,
    scope: ExportScope,
) -> CommandResult<ExportResult> {
    let query = parse_query(query)?;
    state
        .core()
        .export_library(query, format, scope)
        .map_err(to_command_error)
}
