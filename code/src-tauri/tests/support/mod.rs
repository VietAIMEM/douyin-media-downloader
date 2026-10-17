//! Shared rules for integration tests that can write files.
//!
//! This exists because of something that actually happened. A harness bootstrapped
//! the whole application with a temporary *data* directory but left the download
//! directory at its default, and the default resolves to the machine's real
//! `Downloads\Douyin`. Fifteen mock downloads and ten write-probe files were
//! found sitting in the user's own download folder, beside real downloads,
//! indistinguishable from them. Nothing was lost that time. A test that can write
//! into the user's folder is a test that can one day write over something.
//!
//! So the rule is enforced rather than remembered: every harness that chooses a
//! download directory calls [`assert_isolated_destination`], and every test that
//! does not choose one inherits a directory inside its own temporary workspace.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use douyin_downloader_lib::config::AppPaths;
use douyin_downloader_lib::models::settings::default_download_directory;
use douyin_downloader_lib::storage::filenames::resolved_is_within;

/// The two directories on this machine that a test must never write into.
///
/// Read from the same places the application reads them, so this says what the
/// application would actually use rather than what a test assumes it uses. The
/// configured folder is deliberately *not* read from the real database: a test
/// must not open the user's database to find out where not to write, and the
/// default is what a harness that forgets to configure anything will end up
/// using — which is the case this exists for.
pub fn real_paths() -> (PathBuf, PathBuf) {
    let download = PathBuf::from(default_download_directory());
    let data = AppPaths::resolve()
        .map(|paths| paths.data_dir)
        .unwrap_or_else(|_| PathBuf::from("<unresolvable>"));
    (download, data)
}

/// Whether two paths overlap at all — either contains the other.
///
/// Both sides are resolved first, so a junction or a symlink pointing at the
/// user's folder is caught rather than compared as an innocent-looking string.
/// A directory that does not exist yet is resolved through its nearest existing
/// ancestor, which is how `resolved_is_within` handles a destination the test
/// has created but not yet written into.
fn overlaps(candidate: &Path, real: &Path) -> bool {
    resolved_is_within(real, candidate) || resolved_is_within(candidate, real)
}

/// Refuse to run a test whose destination overlaps the machine's real data.
///
/// Fails the test rather than warning, because the alternative is a test that
/// quietly writes into the user's download folder and passes. The message names
/// both paths so the harness at fault is obvious.
///
/// `label` is what the caller was doing, for the failure message.
pub fn assert_isolated_destination(label: &str, destination: &Path) {
    let (real_download, real_data) = real_paths();
    for (what, real) in [
        ("the configured download folder", &real_download),
        ("the application's data folder", &real_data),
    ] {
        if overlaps(destination, real) {
            panic!(
                "{label} would use {destination:?}, which overlaps {what} ({real:?}).\n\
                 A test must write only inside its own temporary workspace. Give this \
                 harness its own directory — do not fall back to the machine's real one."
            );
        }
    }
}

/// Give a test its own download directory inside its temporary workspace.
///
/// Creates the directory as well, so a caller can rely on it existing without
/// checking, and refuses outright if the workspace itself is somewhere it should
/// not be — a temporary directory that resolves onto the user's folder is a
/// bigger problem than a missing one.
pub fn workspace_subdir(workspace: &Path, name: &str, label: &str) -> PathBuf {
    let dir = workspace.join(name);
    assert_isolated_destination(label, &dir);
    std::fs::create_dir_all(&dir).expect("create the test download directory");
    // And once more, now that it exists: on Windows a path can resolve
    // differently before and after creation if something was planted at that
    // name.
    assert_isolated_destination(label, &dir);
    dir
}

/// Check that a booted core is really using the directory the test intended.
///
/// The other half of the guard: `assert_isolated_destination` checks what the
/// test *meant* to use, this checks what the core actually resolved. A settings
/// hook that silently did nothing would otherwise leave the core on the default
/// while the test believed otherwise — which is the shape the incident had.
pub fn assert_core_download_dir(
    core: &douyin_downloader_lib::services::AppCore,
    expected: &Path,
    label: &str,
) {
    let applied = PathBuf::from(core.settings().current().download_directory);
    assert!(
        applied == expected,
        "{label}: the download directory should be {expected:?} but the core reports {applied:?}"
    );
    assert_isolated_destination(label, &applied);
}

/// Point a booted core's download directory at `dir`, then check it took.
///
/// The check is the point: a settings update that silently did nothing would
/// otherwise leave the core writing to the default, and the failure would only
/// show up as files in the user's folder.
pub fn redirect_downloads_to(
    core: &douyin_downloader_lib::services::AppCore,
    dir: &Path,
    label: &str,
) {
    assert_isolated_destination(label, dir);
    core.settings()
        .update(|settings| {
            settings.download_directory = dir.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("redirect the download directory");

    let applied = PathBuf::from(core.settings().current().download_directory);
    assert!(
        applied == dir,
        "{label}: the download directory was set to {dir:?} but the core reports {applied:?}"
    );
    assert_isolated_destination(label, &applied);
}
