//! Settings commands.

use serde::Serialize;

use crate::app_state::AppState;
use crate::errors::{to_command_error, CommandResult};
use crate::models::settings::{default_download_directory, MAX_CONCURRENT_LIMIT, TEMPLATE_PLACEHOLDERS};
use crate::models::{AppSettings, ConflictPolicy, LogLevel, Theme};

/// Read the current settings.
#[tauri::command]
pub fn get_settings(state: tauri::State<'_, AppState>) -> CommandResult<AppSettings> {
    Ok(state.core().settings().current())
}

/// Validate and persist settings, returning what was stored.
///
/// Validation runs in the settings service before anything is written, so an
/// invalid payload fails here and leaves the stored settings untouched.
#[tauri::command]
pub fn save_settings(
    state: tauri::State<'_, AppState>,
    settings: AppSettings,
) -> CommandResult<AppSettings> {
    state
        .core()
        .settings()
        .save(settings)
        .map_err(to_command_error)
}

/// Restore factory defaults and persist them.
#[tauri::command]
pub fn reset_settings(state: tauri::State<'_, AppState>) -> CommandResult<AppSettings> {
    state
        .core()
        .settings()
        .reset()
        .map_err(to_command_error)
}

/// One selectable value for an enum setting.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnumOption {
    /// Stable value to send back in `save_settings`.
    pub value: String,
    /// Human-readable label for the UI.
    pub label: String,
}

/// The choices and limits the Settings screen must render.
///
/// Served from the backend so the valid ranges live with the validation rules
/// instead of being duplicated (and eventually drifting) in TypeScript.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsOptions {
    pub conflict_policies: Vec<EnumOption>,
    pub themes: Vec<EnumOption>,
    pub log_levels: Vec<EnumOption>,
    pub filename_placeholders: Vec<String>,
    pub max_concurrent_limit: u32,
    pub max_retry_attempts: u32,
    pub timeout_seconds_min: u64,
    pub timeout_seconds_max: u64,
    pub default_download_directory: String,
}

/// Enumerate the settings a user can choose between.
#[tauri::command]
pub fn get_settings_options() -> CommandResult<SettingsOptions> {
    let conflict_policies = ConflictPolicy::all()
        .iter()
        .map(|v| EnumOption {
            value: v.as_str().to_string(),
            label: conflict_label(*v).to_string(),
        })
        .collect();
    let themes = Theme::all()
        .iter()
        .map(|v| EnumOption {
            value: v.as_str().to_string(),
            label: title_case(v.as_str()),
        })
        .collect();
    let log_levels = LogLevel::all()
        .iter()
        .map(|v| EnumOption {
            value: v.as_str().to_string(),
            label: title_case(v.as_str()),
        })
        .collect();

    Ok(SettingsOptions {
        conflict_policies,
        themes,
        log_levels,
        filename_placeholders: TEMPLATE_PLACEHOLDERS
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
        max_concurrent_limit: MAX_CONCURRENT_LIMIT,
        max_retry_attempts: 10,
        timeout_seconds_min: 5,
        timeout_seconds_max: 600,
        default_download_directory: default_download_directory(),
    })
}

fn conflict_label(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Rename => "Keep both (add a number)",
        ConflictPolicy::Overwrite => "Replace the existing file",
        ConflictPolicy::Skip => "Skip the download",
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
