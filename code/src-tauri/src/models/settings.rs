//! Application settings model.
//!
//! [`AppSettings`] is the single source of truth for user preferences. It is
//! serialized to JSON, persisted through the settings service, and never
//! exposes storage details to the frontend.
//!
//! Every field carries a `serde` default so that settings written by an older
//! build still deserialize after new fields are added.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};

use super::common::{ConflictPolicy, LogLevel, Theme};

/// Placeholders understood by filename and folder templates.
///
/// `{type}` expands to the media folder name (`Videos`, `Images`, `Audio`), so
/// the default `{creator}/{type}` produces the documented layout and matches
/// what the `organize_by_*` switches would do. Values substituted in from the
/// source (caption, handle, ...) are flattened, so they cannot introduce a
/// directory level or a `..` hop.
pub const TEMPLATE_PLACEHOLDERS: &[&str] = &[
    "{creator}", "{handle}", "{id}", "{date}", "{time}", "{type}", "{index}", "{caption}", "{ext}",
];

/// Upper bound for concurrent downloads. Keeps the app from hammering the
/// source and the disk.
pub const MAX_CONCURRENT_LIMIT: u32 = 16;
/// Highest retry count a download may be configured with.
pub const MAX_RETRY_ATTEMPTS: u32 = 10;
/// Shortest request timeout the settings screen allows, in seconds.
pub const MIN_REQUEST_TIMEOUT_SECONDS: u64 = 5;
/// Longest request timeout the settings screen allows, in seconds.
pub const MAX_REQUEST_TIMEOUT_SECONDS: u64 = 600;

/// Characters Windows forbids in file and folder names.
const ILLEGAL_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// User-configurable application settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// Root directory downloads are written into. Absolute path.
    #[serde(default = "default_download_directory")]
    pub download_directory: String,

    /// How many downloads may run at once.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_downloads: u32,

    /// File name template, without extension.
    #[serde(default = "default_filename_template")]
    pub filename_template: String,

    /// Folder layout template, relative to the download directory.
    #[serde(default = "default_folder_template")]
    pub folder_template: String,

    /// Group files into a per-creator subfolder.
    #[serde(default = "default_true")]
    pub organize_by_creator: bool,

    /// Split each creator folder by media type (Videos/Images/Audio).
    #[serde(default = "default_true")]
    pub organize_by_media_type: bool,

    /// Allow resolution to open a page in an installed browser when the plain
    /// HTTP path cannot obtain the content.
    ///
    /// On by default, because it is what makes some links work at all. It is
    /// exposed so a user who does not want a browser process started can say so
    /// — the application never launches one silently against their wishes.
    #[serde(default = "default_true")]
    pub browser_assisted_resolution: bool,

    /// Load profiles through the application's own signed-in browser profile.
    ///
    /// Off by default, and deliberately: an anonymous visitor is what this
    /// application has always been, and using an account is a decision the user
    /// makes rather than one the application makes for them.
    ///
    /// This says only that the user *asked* for it. It is not a claim that a
    /// sign-in exists or still works — that question is asked of the browser
    /// every time it matters, and answered from the browser's own cookie store
    /// rather than from anything written down here.
    #[serde(default)]
    pub profile_login_enabled: bool,

    /// What to do when the destination file already exists.
    #[serde(default = "default_conflict_policy")]
    pub conflict_policy: ConflictPolicy,

    #[serde(default = "default_theme")]
    pub theme: Theme,

    #[serde(default = "default_log_level")]
    pub log_level: LogLevel,

    /// Mirror logs to a file under the app data directory.
    #[serde(default = "default_true")]
    pub log_to_file: bool,

    /// Attempts per download, including the first.
    #[serde(default = "default_retry_attempts")]
    pub retry_max_attempts: u32,

    /// Per-request network timeout in seconds.
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,

    /// UI language tag, e.g. `en`.
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_true() -> bool {
    true
}
fn default_language() -> String {
    "en".to_string()
}
fn default_max_concurrent() -> u32 {
    3
}
fn default_retry_attempts() -> u32 {
    3
}
fn default_request_timeout() -> u64 {
    30
}
fn default_conflict_policy() -> ConflictPolicy {
    ConflictPolicy::Rename
}
fn default_theme() -> Theme {
    Theme::System
}
fn default_log_level() -> LogLevel {
    LogLevel::Info
}
fn default_filename_template() -> String {
    "{date}_{id}".to_string()
}
fn default_folder_template() -> String {
    "{creator}/{type}".to_string()
}

/// `Downloads/Douyin`, falling back to a temp directory when the OS does not
/// report a Downloads folder.
pub fn default_download_directory() -> String {
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(dl) = dirs.download_dir() {
            return dl.join("Douyin").to_string_lossy().into_owned();
        }
    }
    std::env::temp_dir()
        .join("DouyinDownloads")
        .to_string_lossy()
        .into_owned()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            download_directory: default_download_directory(),
            max_concurrent_downloads: default_max_concurrent(),
            filename_template: default_filename_template(),
            folder_template: default_folder_template(),
            organize_by_creator: true,
            organize_by_media_type: true,
            browser_assisted_resolution: true,
            profile_login_enabled: false,
            conflict_policy: default_conflict_policy(),
            theme: default_theme(),
            log_level: default_log_level(),
            log_to_file: true,
            retry_max_attempts: default_retry_attempts(),
            request_timeout_seconds: default_request_timeout(),
            language: default_language(),
        }
    }
}

impl AppSettings {
    /// Validate the settings before they are persisted or applied.
    ///
    /// Returns a [`AppErrorKind::Validation`](crate::errors::AppErrorKind::Validation)
    /// error whose user-facing message names the offending field, so the
    /// Settings screen can point at the right input.
    pub fn validate(&self) -> AppResult<()> {
        validate_directory(&self.download_directory)?;

        if self.max_concurrent_downloads == 0 {
            return Err(validation_field(
                "max_concurrent_downloads",
                "Concurrent downloads must be at least 1.",
            ));
        }
        if self.max_concurrent_downloads > MAX_CONCURRENT_LIMIT {
            return Err(validation_field(
                "max_concurrent_downloads",
                format!("Concurrent downloads cannot exceed {MAX_CONCURRENT_LIMIT}."),
            ));
        }
        if self.retry_max_attempts > MAX_RETRY_ATTEMPTS {
            return Err(validation_field(
                "retry_max_attempts",
                format!("Retry attempts cannot exceed {MAX_RETRY_ATTEMPTS}."),
            ));
        }
        if !(MIN_REQUEST_TIMEOUT_SECONDS..=MAX_REQUEST_TIMEOUT_SECONDS)
            .contains(&self.request_timeout_seconds)
        {
            return Err(validation_field(
                "request_timeout_seconds",
                format!(
                    "Request timeout must be between {MIN_REQUEST_TIMEOUT_SECONDS} and \
                     {MAX_REQUEST_TIMEOUT_SECONDS} seconds."
                ),
            ));
        }

        validate_filename_template(&self.filename_template)?;
        validate_folder_template(&self.folder_template)?;

        if self.language.trim().is_empty() {
            return Err(validation_field("language", "Language cannot be empty."));
        }

        Ok(())
    }

    /// Bring a loaded settings value back inside its limits, reporting what was
    /// changed.
    ///
    /// Validation answers "may this be saved?". This answers a different
    /// question: "the application is starting and this is what was in the
    /// database — what should it actually use?". A stored blob is not
    /// necessarily one this build wrote: it may predate a limit, or have been
    /// edited by hand in a debug session. Failing to start over a stale
    /// `max_concurrent_downloads` would be absurd, and silently *using* a value
    /// of 1000 would open a thousand sockets.
    ///
    /// Each field is repaired on its own, so one bad value does not cost the
    /// user every other preference they set. The list of what was changed is
    /// returned for logging: a repair the user never hears about is a bug
    /// report nobody can act on.
    pub fn repair(&mut self) -> Vec<&'static str> {
        let mut repaired = Vec::new();
        let defaults = Self::default();

        if self.max_concurrent_downloads == 0 || self.max_concurrent_downloads > MAX_CONCURRENT_LIMIT
        {
            self.max_concurrent_downloads = defaults.max_concurrent_downloads;
            repaired.push("max_concurrent_downloads");
        }
        if self.retry_max_attempts > MAX_RETRY_ATTEMPTS {
            self.retry_max_attempts = defaults.retry_max_attempts;
            repaired.push("retry_max_attempts");
        }
        if !(MIN_REQUEST_TIMEOUT_SECONDS..=MAX_REQUEST_TIMEOUT_SECONDS)
            .contains(&self.request_timeout_seconds)
        {
            self.request_timeout_seconds = defaults.request_timeout_seconds;
            repaired.push("request_timeout_seconds");
        }
        // The download directory is the one that matters most: a relative or
        // empty path is not somewhere a file can be written, and every download
        // would fail with a message about the folder rather than about the
        // setting that is actually wrong.
        if validate_directory(&self.download_directory).is_err() {
            self.download_directory = defaults.download_directory;
            repaired.push("download_directory");
        }
        if validate_filename_template(&self.filename_template).is_err() {
            self.filename_template = defaults.filename_template;
            repaired.push("filename_template");
        }
        if validate_folder_template(&self.folder_template).is_err() {
            self.folder_template = defaults.folder_template;
            repaired.push("folder_template");
        }
        if self.language.trim().is_empty() {
            self.language = defaults.language;
            repaired.push("language");
        }

        repaired
    }
}

fn validation_field(field: &str, message: impl Into<String>) -> AppError {
    let message = message.into();
    AppError::validation(format!("invalid settings field `{field}`: {message}"))
        .with_user_message(message)
        .with_technical_detail(format!("field={field}"))
}

/// Validate the configured download directory.
pub fn validate_directory(raw: &str) -> AppResult<()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(validation_field(
            "download_directory",
            "Choose a download folder.",
        ));
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err(validation_field(
            "download_directory",
            "The download folder must be an absolute path.",
        ));
    }
    if has_traversal(trimmed) {
        return Err(validation_field(
            "download_directory",
            "The download folder cannot contain `..`.",
        ));
    }
    Ok(())
}

/// Validate a filename template (no extension, no separators).
pub fn validate_filename_template(template: &str) -> AppResult<()> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return Err(validation_field(
            "filename_template",
            "The file name template cannot be empty.",
        ));
    }
    if trimmed.contains(['/', '\\']) {
        return Err(validation_field(
            "filename_template",
            "The file name template cannot contain path separators.",
        ));
    }
    if trimmed.chars().any(|c| ILLEGAL_FILENAME_CHARS.contains(&c)) {
        return Err(validation_field(
            "filename_template",
            "The file name template contains characters that are not allowed in file names.",
        ));
    }
    if has_traversal(trimmed) {
        return Err(validation_field(
            "filename_template",
            "The file name template cannot contain `..`.",
        ));
    }
    if trimmed.ends_with(['.', ' ']) {
        return Err(validation_field(
            "filename_template",
            "The file name template cannot end with a dot or space.",
        ));
    }
    Ok(())
}

/// Validate a folder template.
///
/// Forward slashes are allowed as separators; backslashes, drive letters,
/// absolute paths, and `..` segments are rejected so a template can never
/// escape the download directory.
pub fn validate_folder_template(template: &str) -> AppResult<()> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        // An empty template is valid: files go straight into the root.
        return Ok(());
    }
    if trimmed.contains('\\') {
        return Err(validation_field(
            "folder_template",
            "Use `/` as the folder separator.",
        ));
    }
    if trimmed.starts_with('/') || Path::new(trimmed).is_absolute() {
        return Err(validation_field(
            "folder_template",
            "The folder template must be relative to the download folder.",
        ));
    }
    if has_traversal(trimmed) {
        return Err(validation_field(
            "folder_template",
            "The folder template cannot contain `..`.",
        ));
    }
    if trimmed.chars().any(|c| ILLEGAL_FILENAME_CHARS.contains(&c)) {
        return Err(validation_field(
            "folder_template",
            "The folder template contains characters that are not allowed in folder names.",
        ));
    }
    if trimmed.split('/').any(|seg| seg.trim().is_empty()) {
        return Err(validation_field(
            "folder_template",
            "The folder template cannot contain empty segments.",
        ));
    }
    Ok(())
}

/// True when any path segment is `..`.
///
/// Checked on both separators so it also catches traversal on Windows paths
/// regardless of which slash the user typed.
fn has_traversal(value: &str) -> bool {
    value
        .split(['/', '\\'])
        .any(|segment| segment.trim() == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_valid() {
        AppSettings::default().validate().expect("defaults must validate");
    }

    // --- repair on load ------------------------------------------------------

    #[test]
    fn a_settings_value_inside_its_limits_is_left_alone() {
        let mut settings = AppSettings {
            max_concurrent_downloads: 4,
            retry_max_attempts: 3,
            request_timeout_seconds: 45,
            ..Default::default()
        };
        let original = settings.clone();

        assert!(settings.repair().is_empty());
        assert_eq!(settings, original, "nothing is touched");
    }

    #[test]
    fn out_of_range_values_are_replaced_field_by_field() {
        let mut settings = AppSettings {
            max_concurrent_downloads: 1000,
            retry_max_attempts: 99,
            request_timeout_seconds: 1,
            // A path with no drive or root: not somewhere a file can be written.
            download_directory: "relative/folder".into(),
            ..Default::default()
        };
        // A preference that is perfectly valid, to prove the repair is not a
        // wholesale reset.
        settings.log_to_file = false;

        let repaired = settings.repair();

        assert!(repaired.contains(&"max_concurrent_downloads"));
        assert!(repaired.contains(&"retry_max_attempts"));
        assert!(repaired.contains(&"request_timeout_seconds"));
        assert!(repaired.contains(&"download_directory"));
        assert_eq!(settings.max_concurrent_downloads, AppSettings::default().max_concurrent_downloads);
        assert!(validate_directory(&settings.download_directory).is_ok());
        assert!(!settings.log_to_file, "a valid preference survives untouched");
        // And the repaired value now passes the same check the settings screen
        // applies before saving.
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn a_concurrency_of_zero_is_repaired_rather_than_division_by_nothing() {
        let mut settings = AppSettings {
            max_concurrent_downloads: 0,
            ..Default::default()
        };
        assert_eq!(settings.repair(), vec!["max_concurrent_downloads"]);
        assert_eq!(settings.max_concurrent_downloads, 1.max(AppSettings::default().max_concurrent_downloads));
    }

    #[test]
    fn an_invalid_template_is_repaired_but_a_valid_one_is_kept() {
        let mut settings = AppSettings {
            filename_template: "{id}".into(),
            ..Default::default()
        };
        assert!(settings.repair().is_empty());
        assert_eq!(settings.filename_template, "{id}");

        // A separator in a file-name template: the sort of value that would
        // otherwise try to write outside the folder it belongs in.
        settings.filename_template = "../{id}".into();
        let repaired = settings.repair();
        assert_eq!(repaired, vec!["filename_template"]);
        assert_eq!(settings.filename_template, AppSettings::default().filename_template);
    }

    #[test]
    fn default_download_directory_is_absolute() {
        let dir = default_download_directory();
        assert!(Path::new(&dir).is_absolute(), "got {dir}");
    }

    #[test]
    fn rejects_relative_download_directory() {
        let s = AppSettings {
            download_directory: "relative/path".into(),
            ..Default::default()
        };
        let err = s.validate().unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
    }

    #[test]
    fn rejects_traversal_in_directory() {
        let s = AppSettings {
            download_directory: r"C:\Downloads\..\Windows".into(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_zero_and_excessive_concurrency() {
        let mut s = AppSettings {
            max_concurrent_downloads: 0,
            ..Default::default()
        };
        assert!(s.validate().is_err());
        s.max_concurrent_downloads = MAX_CONCURRENT_LIMIT + 1;
        assert!(s.validate().is_err());
        s.max_concurrent_downloads = MAX_CONCURRENT_LIMIT;
        assert!(s.validate().is_ok());
    }

    #[test]
    fn rejects_timeout_out_of_range() {
        let mut s = AppSettings {
            request_timeout_seconds: 1,
            ..Default::default()
        };
        assert!(s.validate().is_err());
        s.request_timeout_seconds = 601;
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_filename_template_with_separators() {
        assert!(validate_filename_template("a/b").is_err());
        assert!(validate_filename_template(r"a\b").is_err());
        assert!(validate_filename_template("..").is_err());
        assert!(validate_filename_template("bad:name").is_err());
        assert!(validate_filename_template("").is_err());
        assert!(validate_filename_template("clip_2024").is_ok());
    }

    #[test]
    fn rejects_folder_template_escape_attempts() {
        assert!(validate_folder_template("../../etc").is_err());
        assert!(validate_folder_template("/absolute").is_err());
        assert!(validate_folder_template(r"a\b").is_err());
        assert!(validate_folder_template("a//b").is_err());
        assert!(validate_folder_template("").is_ok());
        assert!(validate_folder_template("{creator}/{type}").is_ok());
    }

    #[test]
    fn settings_load_with_missing_fields() {
        // Simulates an older build's JSON lacking newly added fields.
        let json = r#"{"downloadDirectory":"C:\\Temp\\Douyin"}"#;
        let parsed: AppSettings = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.download_directory, r"C:\Temp\Douyin");
        assert_eq!(parsed.max_concurrent_downloads, 3);
        assert!(parsed.organize_by_creator);
        // Including the sign-in preference, which is what makes adding it a
        // change with no migration: a settings blob written before this field
        // existed loads, and loads as "the user has not asked for this".
        assert!(!parsed.profile_login_enabled);
    }

    #[test]
    fn the_sign_in_preference_is_off_until_the_user_asks() {
        // Off, and deliberately: using an account is a decision the user makes.
        assert!(!AppSettings::default().profile_login_enabled);
    }

    #[test]
    fn the_sign_in_preference_survives_being_stored() {
        let settings = AppSettings {
            profile_login_enabled: true,
            ..AppSettings::default()
        };

        let json = serde_json::to_string(&settings).unwrap();
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();

        assert!(parsed.profile_login_enabled);
        // Stored as the preference and nothing else: no status, no flag saying
        // a sign-in exists, nothing that could be mistaken for proof of one.
        assert!(!json.contains("signedIn"));
        assert!(!json.contains("loggedIn"));
        assert!(!json.contains("sessionid"));
    }

    #[test]
    fn no_setting_carries_a_credential() {
        // The settings blob is a plain row in a plain database file. Nothing
        // that belongs to a sign-in may reach it: the browser keeps its own
        // session data in its own profile, and this file is not that.
        let settings = AppSettings {
            profile_login_enabled: true,
            ..AppSettings::default()
        };
        let json = serde_json::to_string(&settings).unwrap().to_lowercase();

        for forbidden in ["cookie", "token", "password", "session", "credential", "auth"] {
            assert!(
                !json.contains(forbidden),
                "the settings blob mentions {forbidden}, which is where a credential would end \
                 up: {json}"
            );
        }
    }

    #[test]
    fn settings_round_trip_through_json() {
        let s = AppSettings::default();
        let json = serde_json::to_string(&s).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
