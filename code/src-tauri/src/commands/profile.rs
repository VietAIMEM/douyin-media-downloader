//! Profile workflow commands.
//!
//! Thin adapters over [`crate::services::profile`]. Each one unwraps managed
//! state, calls exactly one core operation, and converts the result to the wire
//! format — no network, parsing, database, or filesystem logic lives here.
//!
//! The three commands map one-to-one onto the screen's three actions: identify
//! the creator behind a link, read a page of their posts, and submit a selection
//! to the download pipeline.

use crate::app_state::AppState;
use crate::errors::{to_command_error, CommandResult};
use crate::services::profile::{
    ProfileDownloadResult, ProfileIdentity, ProfilePostsPage, SubmitProfileDownloadsInput,
};

/// Resolve a profile URL into creator information.
///
/// A link that is not a profile is a validation error naming what it actually
/// was, so the screen can send the user to the right place instead of failing
/// silently.
#[tauri::command]
pub async fn resolve_profile(
    state: tauri::State<'_, AppState>,
    url: String,
) -> CommandResult<ProfileIdentity> {
    state
        .core()
        .resolve_profile(&url)
        .await
        .map_err(to_command_error)
}

/// Read one page of a creator's posts.
///
/// `cursor` is the opaque token the previous page returned; omit it for the
/// first page. The platform's paging scheme never reaches the frontend.
#[tauri::command]
pub async fn list_profile_posts(
    state: tauri::State<'_, AppState>,
    url: String,
    cursor: Option<String>,
    limit: Option<u32>,
) -> CommandResult<ProfilePostsPage> {
    state
        .core()
        .list_profile_posts(&url, cursor, limit)
        .await
        .map_err(to_command_error)
}

/// Submit stored posts for download.
///
/// Carries ids only. Media is resolved and the task is created by the
/// application service through the same path the Single URL screen uses, so a
/// compromised renderer cannot point a transfer anywhere and the profile screen
/// cannot become a second downloader.
#[tauri::command]
pub async fn submit_profile_downloads(
    state: tauri::State<'_, AppState>,
    input: SubmitProfileDownloadsInput,
) -> CommandResult<ProfileDownloadResult> {
    state
        .core()
        .submit_profile_downloads(input)
        .await
        .map_err(to_command_error)
}
