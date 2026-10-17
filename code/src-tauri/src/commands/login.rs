//! Signing in to Douyin, and everything that keeps that a user decision.
//!
//! What lives here is deliberately small: open a window the user signs in with,
//! ask the browser what its profile is holding, and clear the profile when the
//! user asks for that. Nothing here types a credential, answers a challenge, or
//! reads a credential out — the sign-in happens in an ordinary browser window
//! on Douyin's own page, and the result stays in Chromium's profile.

use serde::Serialize;
use tauri::State;

use crate::app_state::AppState;
use crate::errors::{to_command_error, AppError, CommandResult};
use crate::network::chromium::LoginStatus;

/// What the Profile Downloader needs to draw its sign-in section.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginReport {
    /// Whether this build can sign in at all — false when no supported browser
    /// was found at startup, in which case there is nothing to open.
    pub available: bool,
    /// What the browser's profile holds, or `None` when it could not be asked.
    ///
    /// `None` is a third answer, not a failure dressed up as one: "the browser
    /// is not running and could not be started" is different from "the profile
    /// has no session", and the screen says which.
    pub status: Option<LoginStatus>,
    /// Where the profile lives, so the user can see that it is the
    /// application's own and not their everyday browser's.
    ///
    /// A path and nothing else. The profile holds Chromium's session data; none
    /// of it is read here, and none of it crosses this boundary.
    pub profile_directory: String,
}

/// The profile directory, or an empty string when there is none to name.
fn profile_directory(state: &AppState) -> String {
    state
        .core()
        .paths()
        .auth_profile_dir
        .to_string_lossy()
        .into_owned()
}

/// A report built without asking the browser anything.
fn unavailable(state: &AppState) -> LoginReport {
    LoginReport {
        available: false,
        status: None,
        profile_directory: profile_directory(state),
    }
}

/// Open the window the user signs in with.
///
/// Returns as soon as the window is open — the sign-in itself is the user's,
/// takes as long as it takes, and is never waited on. The report that comes back
/// says what the profile held at that moment, which immediately after opening is
/// "signed out"; the screen shows "signing in" until the user checks.
#[tauri::command]
pub async fn begin_login(state: State<'_, AppState>) -> CommandResult<LoginReport> {
    let core = state.core();
    let Some(browser) = core.browser() else {
        return Err(to_command_error(
            AppError::browser_unavailable("no browser is available to sign in with").with_user_message(
                "This application could not find a browser to sign in with. Install Chrome or \
                 Edge, then restart the application.",
            ),
        ));
    };

    browser.open_login_window().await.map_err(to_command_error)?;

    Ok(LoginReport {
        available: true,
        status: browser.login_status().await.ok(),
        profile_directory: profile_directory(&state),
    })
}

/// Ask the saved profile what it is holding.
///
/// Answered from the browser's own cookie store every time it is asked. There
/// is no remembered "signed in" anywhere in this application to go stale, which
/// is what makes the answer worth showing.
#[tauri::command]
pub async fn get_login_status(state: State<'_, AppState>) -> CommandResult<LoginReport> {
    let core = state.core();
    let Some(browser) = core.browser() else {
        return Ok(unavailable(&state));
    };

    Ok(LoginReport {
        available: true,
        status: browser.login_status().await.ok(),
        profile_directory: profile_directory(&state),
    })
}

/// Close the sign-in browser and delete the saved profile.
///
/// The order is the whole of it: the browser that has the profile open is
/// closed *first*, and only then is anything removed. Deleting a profile a
/// running Chromium still has mapped is how a "cleared" session comes back, or
/// how a browser is left holding files that no longer exist.
#[tauri::command]
pub async fn clear_login_session(state: State<'_, AppState>) -> CommandResult<LoginReport> {
    let core = state.core();
    let Some(browser) = core.browser() else {
        return Ok(unavailable(&state));
    };

    // Closing releases the profile lock as it goes: the lock lives in the
    // session, so there is no way to close the browser and keep the claim.
    browser.close_login_session().await.map_err(to_command_error)?;

    let dir = core.paths().auth_profile_dir.clone();
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|err| {
            to_command_error(
                AppError::storage("the saved sign-in could not be removed")
                    .with_user_message(
                        "The saved sign-in could not be cleared. Close any window of this \
                         application that is still open and try again.",
                    )
                    .with_technical_detail(format!("{}: {err}", dir.display())),
            )
        })?;
    }

    tracing::info!(target: "browser", "the saved sign-in profile was cleared");

    Ok(LoginReport {
        available: true,
        // Known without asking: the profile directory is gone, so there is
        // nothing left for a session to be in.
        status: Some(LoginStatus::SignedOut),
        profile_directory: profile_directory(&state),
    })
}
