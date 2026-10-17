//! Settings integration tests: defaults, validation, persistence, and the
//! update/reset paths.
//!
//! Settings are the one place a user's input reaches SQLite, so these tests
//! care about two things in particular: that an invalid value can never be
//! stored, and that a stored value is still there after the application is
//! restarted.

use douyin_downloader_lib::database::{repositories::settings as settings_repo, Database};
use douyin_downloader_lib::errors::AppErrorKind;
use douyin_downloader_lib::models::settings::{validate_filename_template, validate_folder_template};
use douyin_downloader_lib::models::{AppSettings, ConflictPolicy, LogLevel, Theme};
use douyin_downloader_lib::settings::SettingsService;

fn service(temp: &tempfile::TempDir) -> SettingsService {
    let database = Database::open(temp.path().join("test.db")).expect("open database");
    SettingsService::new(database).expect("settings service")
}

fn temp_service() -> (tempfile::TempDir, SettingsService) {
    let temp = tempfile::tempdir().expect("temp dir");
    let service = service(&temp);
    (temp, service)
}

// --- defaults ---------------------------------------------------------------

#[test]
fn a_fresh_install_starts_from_valid_sensible_defaults() {
    let (_temp, settings) = temp_service();
    let current = settings.current();

    assert!(!current.download_directory.trim().is_empty());
    assert!(current.max_concurrent_downloads >= 1);
    assert!(!current.filename_template.trim().is_empty());
    assert_eq!(current.conflict_policy, ConflictPolicy::Rename);
    assert_eq!(current.theme, Theme::System);
    assert_eq!(current.log_level, LogLevel::Info);
    assert!(current.request_timeout_seconds >= 5);

    // The defaults must satisfy the same validation the save path applies.
    assert!(current.validate().is_ok(), "defaults must be self-consistent");
}

#[test]
fn the_first_run_installs_defaults_as_a_single_stored_blob() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::open(temp.path().join("test.db")).unwrap();

    {
        let _settings = SettingsService::new(database.clone()).unwrap();
    }

    // Settings are persisted as one JSON document under one key, not as a
    // column per preference. That is what lets the model gain fields without a
    // migration, and it is worth pinning down in a test.
    let rows = database.with_conn(|conn| settings_repo::all(conn)).unwrap();
    assert_eq!(rows.len(), 1, "defaults occupy exactly one settings row");

    let (key, value) = &rows[0];
    assert!(!key.trim().is_empty());
    let parsed: AppSettings = serde_json::from_str(value).expect("the blob parses back");
    assert_eq!(parsed, AppSettings::default());
}

// --- save and reload --------------------------------------------------------

#[test]
fn saved_settings_are_returned_as_they_were_stored() {
    let (_temp, settings) = temp_service();

    let mut wanted = settings.current();
    wanted.max_concurrent_downloads = 5;
    wanted.theme = Theme::Dark;
    wanted.conflict_policy = ConflictPolicy::Skip;
    wanted.filename_template = "{creator}_{id}".into();
    wanted.folder_template = "{creator}/{type}".into();

    let stored = settings.save(wanted).expect("save");

    assert_eq!(stored.max_concurrent_downloads, 5);
    assert_eq!(stored.theme, Theme::Dark);
    assert_eq!(stored.conflict_policy, ConflictPolicy::Skip);
    assert_eq!(stored.filename_template, "{creator}_{id}");

    // The value returned by `save` is the one on disk, not an optimistic copy:
    // reading the service again must agree exactly.
    assert_eq!(settings.reload().unwrap(), stored);
}

#[test]
fn an_absolute_download_directory_is_required() {
    let (_temp, settings) = temp_service();

    let mut relative = settings.current();
    relative.download_directory = "Downloads/Douyin".into();
    assert_eq!(
        settings.save(relative).unwrap_err().kind(),
        AppErrorKind::Validation,
        "a relative download folder would resolve against the working directory"
    );

    let mut traversing = settings.current();
    traversing.download_directory = "C:/Downloads/../../Windows".into();
    assert_eq!(
        settings.save(traversing).unwrap_err().kind(),
        AppErrorKind::Validation
    );
}

#[test]
fn settings_survive_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("test.db");

    {
        let database = Database::open(&path).unwrap();
        let settings = SettingsService::new(database).unwrap();
        let mut wanted = settings.current();
        wanted.max_concurrent_downloads = 7;
        wanted.log_level = LogLevel::Debug;
        wanted.retry_max_attempts = 1;
        wanted.organize_by_creator = false;
        settings.save(wanted).unwrap();
    }

    // A new process would do exactly this: open the same file, read settings.
    let reopened = SettingsService::new(Database::open(&path).unwrap()).unwrap();
    let current = reopened.current();

    assert_eq!(current.max_concurrent_downloads, 7);
    assert_eq!(current.log_level, LogLevel::Debug);
    assert_eq!(current.retry_max_attempts, 1);
    assert!(!current.organize_by_creator);
    // Untouched fields keep their defaults rather than being reset.
    assert_eq!(current.conflict_policy, ConflictPolicy::Rename);
}

#[test]
fn reload_rereads_the_stored_values() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::open(temp.path().join("test.db")).unwrap();
    let first = SettingsService::new(database.clone()).unwrap();
    let second = SettingsService::new(database).unwrap();

    let mut wanted = first.current();
    wanted.max_concurrent_downloads = 9;
    first.save(wanted).unwrap();

    // `second` was constructed from the same database, so a reload must see
    // what the other service wrote.
    let reloaded = second.reload().unwrap();
    assert_eq!(reloaded.max_concurrent_downloads, 9);
}

#[test]
fn a_partial_update_only_touches_the_fields_it_names() {
    let (_temp, settings) = temp_service();

    let before = settings.current();
    let after = settings
        .update(|s| {
            s.max_concurrent_downloads = 2;
            Ok(())
        })
        .expect("update");

    assert_eq!(after.max_concurrent_downloads, 2);
    assert_eq!(after.theme, before.theme);
    assert_eq!(after.download_directory, before.download_directory);
    assert_eq!(after.filename_template, before.filename_template);
}

#[test]
fn reset_restores_every_default() {
    let (_temp, settings) = temp_service();

    let mut wanted = settings.current();
    wanted.theme = Theme::Light;
    wanted.max_concurrent_downloads = 12;
    wanted.folder_template = String::new();
    settings.save(wanted).unwrap();

    let reset = settings.reset().expect("reset");
    let defaults = AppSettings::default();

    assert_eq!(reset.theme, defaults.theme);
    assert_eq!(reset.max_concurrent_downloads, defaults.max_concurrent_downloads);
    assert_eq!(reset.folder_template, defaults.folder_template);
}

// --- validation -------------------------------------------------------------

#[test]
fn an_invalid_setting_is_rejected_and_nothing_is_written() {
    let (_temp, settings) = temp_service();
    let before = settings.current();

    let mut bad = settings.current();
    bad.max_concurrent_downloads = 0;
    let err = settings.save(bad).expect_err("zero concurrency must be rejected");

    assert_eq!(err.kind(), AppErrorKind::Validation);
    assert_eq!(
        settings.current().max_concurrent_downloads,
        before.max_concurrent_downloads,
        "a rejected save must leave the stored value untouched"
    );
}

#[test]
fn rejected_values_cover_every_validated_field() {
    let (_temp, settings) = temp_service();

    let mut empty_directory = settings.current();
    empty_directory.download_directory = "   ".into();
    assert_eq!(
        settings.save(empty_directory).unwrap_err().kind(),
        AppErrorKind::Validation
    );

    let mut empty_filename = settings.current();
    empty_filename.filename_template = String::new();
    assert_eq!(
        settings.save(empty_filename).unwrap_err().kind(),
        AppErrorKind::Validation
    );

    let mut too_many_retries = settings.current();
    too_many_retries.retry_max_attempts = 99;
    assert_eq!(
        settings.save(too_many_retries).unwrap_err().kind(),
        AppErrorKind::Validation
    );

    let mut timeout = settings.current();
    timeout.request_timeout_seconds = 1;
    assert_eq!(
        settings.save(timeout).unwrap_err().kind(),
        AppErrorKind::Validation
    );
}

#[test]
fn validation_rejections_do_not_leak_internal_detail_to_the_user() {
    let (_temp, settings) = temp_service();

    let mut bad = settings.current();
    bad.filename_template = "a/b".into();
    let err = settings.save(bad).unwrap_err();

    let user_message = err.user_message();
    assert!(!user_message.is_empty());
    assert!(
        !user_message.contains("a/b"),
        "the user message must not echo raw input: {user_message}"
    );
    // The technical message is what logs get, and it may be specific.
    assert!(!err.message().is_empty());
}

#[test]
fn path_traversal_in_a_folder_template_is_refused() {
    let (_temp, settings) = temp_service();

    for template in ["../../etc", "/absolute", r"a\b", "a//b"] {
        let mut bad = settings.current();
        bad.folder_template = template.into();
        assert_eq!(
            settings.save(bad).unwrap_err().kind(),
            AppErrorKind::Validation,
            "folder template {template:?} must be refused"
        );
    }

    // An empty template means "no subfolders", which is legitimate.
    assert!(validate_folder_template("").is_ok());
}

#[test]
fn template_validators_are_reusable_independently_of_the_service() {
    // The same functions the settings form will call for inline feedback.
    assert!(validate_filename_template("{date}_{id}").is_ok());
    assert!(validate_filename_template("clip_2024").is_ok());
    assert!(validate_filename_template("a/b").is_err());
    assert!(validate_filename_template("..").is_err());
    assert!(validate_filename_template("").is_err());

    assert!(validate_folder_template("{creator}/{type}").is_ok());
    assert!(validate_folder_template("").is_ok());
    assert!(validate_folder_template("../escape").is_err());
}
