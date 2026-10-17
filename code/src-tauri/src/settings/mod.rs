//! Persistent settings service.
//!
//! [`SettingsService`] is the only thing that knows how settings are stored.
//! Callers deal in [`AppSettings`] values; the fact that they live as a JSON
//! blob under one key in the `settings` table never leaks out.
//!
//! The service keeps a read-mostly in-memory cache so that hot paths (resolvers
//! asking for the download directory, download workers asking for concurrency)
//! do not hit SQLite, while still writing through on every save so settings
//! survive a restart.

use std::path::PathBuf;

use parking_lot::RwLock;

use crate::database::{repositories::settings as settings_repo, Database};
use crate::errors::{AppError, AppResult};
use crate::models::AppSettings;

/// Storage key under which the settings JSON is persisted.
pub const SETTINGS_KEY: &str = "app_settings";

/// Loads, caches, validates, and persists application settings.
pub struct SettingsService {
    db: Database,
    cache: RwLock<AppSettings>,
}

impl SettingsService {
    /// Build the service, loading persisted settings (or installing defaults).
    ///
    /// Never fails because of *unreadable* stored settings: a corrupt blob is
    /// logged and replaced with defaults so a bad value can never prevent the
    /// application from starting.
    pub fn new(db: Database) -> AppResult<Self> {
        let loaded = match Self::read_stored(&db)? {
            Some(parsed) => parsed,
            None => {
                let defaults = AppSettings::default();
                Self::write_stored(&db, &defaults)?;
                tracing::info!(target: "settings", "created default application settings");
                defaults
            }
        };

        Ok(Self {
            db,
            cache: RwLock::new(loaded),
        })
    }

    fn read_stored(db: &Database) -> AppResult<Option<AppSettings>> {
        let raw = db.with_conn(|conn| settings_repo::get(conn, SETTINGS_KEY))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        match serde_json::from_str::<AppSettings>(&raw) {
            Ok(mut settings) => {
                // Parsing is not the same as being usable. A blob written by an
                // older build, or edited by hand, can parse perfectly and still
                // hold a concurrency of zero or a relative download folder.
                // Those are repaired field by field, so one bad value does not
                // cost the user every other preference they set — and the repair
                // is logged, because a setting that quietly changed is a
                // complaint nobody can act on.
                let repaired = settings.repair();
                if !repaired.is_empty() {
                    tracing::warn!(
                        target: "settings",
                        fields = ?repaired,
                        "stored settings held values out of range; they were reset to defaults"
                    );
                }
                Ok(Some(settings))
            }
            Err(err) => {
                tracing::warn!(
                    target: "settings",
                    error = %err,
                    "stored settings could not be parsed; falling back to defaults"
                );
                Ok(None)
            }
        }
    }

    fn write_stored(db: &Database, settings: &AppSettings) -> AppResult<()> {
        let json = serde_json::to_string_pretty(settings)?;
        db.with_conn(|conn| settings_repo::set(conn, SETTINGS_KEY, &json))?;
        Ok(())
    }

    /// A snapshot of the current settings.
    pub fn current(&self) -> AppSettings {
        self.cache.read().clone()
    }

    /// Re-read settings from storage, refreshing the cache.
    pub fn reload(&self) -> AppResult<AppSettings> {
        let loaded = Self::read_stored(&self.db)?.unwrap_or_default();
        *self.cache.write() = loaded.clone();
        Ok(loaded)
    }

    /// Validate and persist `settings`, returning the stored value.
    ///
    /// Validation happens *before* any write, so an invalid payload never
    /// reaches the database and the in-memory cache stays consistent with what
    /// is on disk.
    pub fn save(&self, settings: AppSettings) -> AppResult<AppSettings> {
        settings.validate()?;
        Self::write_stored(&self.db, &settings)?;
        *self.cache.write() = settings.clone();
        tracing::info!(target: "settings", "application settings saved");
        Ok(settings)
    }

    /// Mutate the current settings and persist them.
    ///
    /// `f` must return the settings to store; returning `Err` aborts without
    /// writing anything.
    pub fn update<F>(&self, f: F) -> AppResult<AppSettings>
    where
        F: FnOnce(&mut AppSettings) -> AppResult<()>,
    {
        let mut draft = self.current();
        f(&mut draft)?;
        self.save(draft)
    }

    /// Restore and persist factory defaults.
    pub fn reset(&self) -> AppResult<AppSettings> {
        let defaults = AppSettings::default();
        let stored = self.save(defaults)?;
        tracing::info!(target: "settings", "application settings reset to defaults");
        Ok(stored)
    }

    /// The configured download directory as a path.
    pub fn download_directory(&self) -> PathBuf {
        PathBuf::from(self.current().download_directory)
    }

    /// The configured download directory, validated to be usable.
    pub fn download_directory_checked(&self) -> AppResult<PathBuf> {
        let current = self.current();
        crate::models::settings::validate_directory(&current.download_directory)?;
        Ok(PathBuf::from(current.download_directory))
    }

    /// Access the underlying database handle (used to build sibling services).
    pub fn database(&self) -> &Database {
        &self.db
    }
}

impl std::fmt::Debug for SettingsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsService")
            .field("settings", &self.cache.read())
            .finish_non_exhaustive()
    }
}

/// Helper for call sites that need a settings error with a user message.
pub fn settings_error(field: &str, message: impl Into<String>) -> AppError {
    let message = message.into();
    AppError::validation(format!("settings field `{field}`: {message}"))
        .with_user_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Theme;

    fn service() -> SettingsService {
        let db = Database::open_in_memory().unwrap();
        SettingsService::new(db).unwrap()
    }

    #[test]
    fn first_run_installs_defaults() {
        let svc = service();
        assert_eq!(svc.current(), AppSettings::default());
    }

    #[test]
    fn save_then_reload_round_trips() {
        let svc = service();
        let mut settings = svc.current();
        settings.max_concurrent_downloads = 7;
        settings.theme = Theme::Light;
        settings.filename_template = "{creator}_{id}".into();
        svc.save(settings.clone()).unwrap();

        // A fresh service over the same database must see the saved values.
        let reopened = SettingsService::new(svc.database().clone()).unwrap();
        assert_eq!(reopened.current(), settings);
    }

    #[test]
    fn invalid_settings_are_rejected_and_not_persisted() {
        let svc = service();
        let mut settings = svc.current();
        settings.max_concurrent_downloads = 0;
        let err = svc.save(settings).unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
        // The stored value is unchanged.
        assert_eq!(svc.current().max_concurrent_downloads, 3);
    }

    #[test]
    fn corrupt_stored_json_falls_back_to_defaults() {
        let db = Database::open_in_memory().unwrap();
        db.with_conn(|conn| settings_repo::set(conn, SETTINGS_KEY, "{not json"))
            .unwrap();
        let svc = SettingsService::new(db).unwrap();
        assert_eq!(svc.current(), AppSettings::default());
    }

    #[test]
    fn update_applies_mutation_then_persists() {
        let svc = service();
        let updated = svc
            .update(|s| {
                s.organize_by_creator = false;
                Ok(())
            })
            .unwrap();
        assert!(!updated.organize_by_creator);
        assert!(!SettingsService::new(svc.database().clone())
            .unwrap()
            .current()
            .organize_by_creator);
    }

    #[test]
    fn update_error_leaves_settings_untouched() {
        let svc = service();
        let before = svc.current();
        let result = svc.update(|s| {
            s.max_concurrent_downloads = 9;
            Err(settings_error("x", "nope"))
        });
        assert!(result.is_err());
        assert_eq!(svc.current(), before);
    }

    #[test]
    fn reset_restores_defaults() {
        let svc = service();
        svc.update(|s| {
            s.theme = Theme::Dark;
            Ok(())
        })
        .unwrap();
        let after = svc.reset().unwrap();
        assert_eq!(after, AppSettings::default());
    }
}
