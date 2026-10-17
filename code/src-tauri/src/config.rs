//! Filesystem locations the application owns.
//!
//! All paths are resolved in one place so nothing else has to guess where the
//! database or logs live, and so tests can point the app at a temporary
//! directory via [`AppPaths::with_data_dir`].

use std::path::{Path, PathBuf};

use crate::errors::{AppError, AppResult};

/// Environment variable that overrides the data directory.
///
/// Intended for development and automated tests; it also makes the packaged
/// application portable if a user wants everything on one drive.
pub const DATA_DIR_ENV_VAR: &str = "DOUYIN_DOWNLOADER_DATA_DIR";

/// Database file name inside the data directory.
pub const DATABASE_FILE_NAME: &str = "douyin-downloader.db";

/// Directory name for logs inside the data directory.
pub const LOG_DIR_NAME: &str = "logs";

/// Directory the application's own browsers keep their user data in.
///
/// Distinct from the throwaway profiles a render gets: this one is the
/// application's, it lives beside the database rather than in the temp
/// directory, and it is the only place a Douyin sign-in is ever kept. Being
/// under the data directory is what makes it survive an upgrade — the data
/// directory is stable across versions and the installer never touches it —
/// and what keeps it out of the age-based sweep, which only ever looks at the
/// temp directory.
pub const BROWSER_DIR_NAME: &str = "browser";

/// Directory holding generated media thumbnails, inside the data directory.
///
/// Inside the data directory for the same reasons the browser profile is: it
/// survives an upgrade, the installer never touches it, and it is nowhere near
/// the user's own files. Nothing here is a user's file — every entry is a small
/// JPEG this application rendered — so losing the whole folder costs nothing but
/// the time to render them again.
pub const THUMBNAIL_DIR_NAME: &str = "thumbnails";

/// Directory name of the persistent authenticated browser profile.
pub const AUTH_PROFILE_DIR_NAME: &str = "profile";

/// Resolved application directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    /// Root for application state (database, logs).
    pub data_dir: PathBuf,
    /// SQLite database file.
    pub database_path: PathBuf,
    /// Directory that holds rolling log files.
    pub log_dir: PathBuf,
    /// The application's own persistent browser profile.
    ///
    /// Empty when the application could not work out where to put it, which is
    /// the case in tests that only exercise the database; nothing uses it
    /// unless the user asks for an authenticated profile load.
    pub auth_profile_dir: PathBuf,
    /// Where generated media thumbnails are cached.
    pub thumbnail_dir: PathBuf,
}

impl AppPaths {
    /// Resolve the standard per-user application directories.
    ///
    /// Order of preference:
    /// 1. `DOUYIN_DOWNLOADER_DATA_DIR`, when set.
    /// 2. The OS roaming data directory (`%APPDATA%\DouyinDownloader`).
    /// 3. A subdirectory of the temp directory, so the app still starts if the
    ///    OS gives us nothing (and is obviously non-persistent).
    pub fn resolve() -> AppResult<Self> {
        if let Some(override_dir) = std::env::var_os(DATA_DIR_ENV_VAR) {
            if !override_dir.is_empty() {
                return Ok(Self::with_data_dir(PathBuf::from(override_dir)));
            }
        }

        let data_dir = directories::ProjectDirs::from("com", "DouyinDownloader", "DouyinDownloader")
            .map(|dirs| dirs.data_dir().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("DouyinDownloader"));

        Ok(Self::with_data_dir(data_dir))
    }

    /// Build paths rooted at an explicit directory. Used by tests.
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        Self {
            database_path: data_dir.join(DATABASE_FILE_NAME),
            log_dir: data_dir.join(LOG_DIR_NAME),
            auth_profile_dir: data_dir
                .join(BROWSER_DIR_NAME)
                .join(AUTH_PROFILE_DIR_NAME),
            thumbnail_dir: data_dir.join(THUMBNAIL_DIR_NAME),
            data_dir,
        }
    }

    /// Create the data and log directories if they do not exist yet.
    pub fn ensure_directories(&self) -> AppResult<()> {
        create_dir(&self.data_dir)?;
        create_dir(&self.log_dir)?;
        Ok(())
    }
}

fn create_dir(path: &Path) -> AppResult<()> {
    std::fs::create_dir_all(path).map_err(|err| {
        AppError::storage("could not create an application directory")
            .with_user_message("The application could not prepare its data folder.")
            .with_technical_detail(path.to_string_lossy().into_owned())
            .with_source(err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_derived_from_the_data_directory() {
        let paths = AppPaths::with_data_dir(PathBuf::from("C:\\data"));
        assert_eq!(paths.database_path, PathBuf::from("C:\\data\\douyin-downloader.db"));
        assert_eq!(paths.log_dir, PathBuf::from("C:\\data\\logs"));
    }

    #[test]
    fn the_sign_in_profile_is_under_the_data_directory() {
        // Where it is, is the whole of what makes it survive: the data
        // directory is stable across versions, the installer never touches it,
        // and the cleanup that reclaims abandoned render profiles only ever
        // looks in the temp directory.
        let paths = AppPaths::with_data_dir(PathBuf::from("C:\\data"));
        assert_eq!(
            paths.auth_profile_dir,
            PathBuf::from("C:\\data\\browser\\profile")
        );
        assert!(
            paths.auth_profile_dir.starts_with(&paths.data_dir),
            "the sign-in profile must live inside the data directory"
        );
    }

    #[test]
    fn the_sign_in_profile_is_not_something_the_sweep_looks_for() {
        // The cleanup that reclaims abandoned render profiles matches a name
        // prefix. The sign-in profile must not carry it — and does not, which is
        // what makes the sweep safe even for a user who has pointed the data
        // directory at somewhere unusual with DOUYIN_DOWNLOADER_DATA_DIR.
        //
        // The other half of the protection — that the sweep is only ever
        // pointed at the temp directory — is asserted where the sweep is, in
        // `network::chromium`.
        let paths = AppPaths::with_data_dir(PathBuf::from(r"C:\data"));
        let name = paths
            .auth_profile_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("the profile directory has a name");

        assert!(
            !name.starts_with(crate::network::chromium::PROFILE_PREFIX),
            "the sign-in profile is named {name}, which the render-profile sweep would match"
        );
    }

    #[test]
    fn the_sign_in_profile_path_does_not_depend_on_the_version() {
        // Two builds at different versions, same data directory: the same
        // profile. Nothing in the path mentions the application version, which
        // is what makes an upgrade leave the session in place.
        let one = AppPaths::with_data_dir(PathBuf::from("C:\\data"));
        let two = AppPaths::with_data_dir(PathBuf::from("C:\\data"));
        assert_eq!(one.auth_profile_dir, two.auth_profile_dir);
        assert!(!one.auth_profile_dir.to_string_lossy().contains("1.0"));
    }

    #[test]
    fn ensure_directories_creates_both() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_data_dir(temp.path().join("nested").join("app"));
        paths.ensure_directories().unwrap();
        assert!(paths.data_dir.is_dir());
        assert!(paths.log_dir.is_dir());
    }
}
