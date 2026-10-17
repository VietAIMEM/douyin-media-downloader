//! The rule that keeps the library's own tests out of the user's files.
//!
//! Integration tests have their own copy of this in `tests/support/mod.rs`,
//! because they link the library as an external crate and cannot see anything
//! behind `#[cfg(test)]`. This is the same rule for the tests that live inside
//! the library, and it is the same rule for the same reason: a harness that
//! boots the whole application with a temporary *data* directory and leaves the
//! download directory alone gets the machine's real `Downloads\Douyin`, which is
//! how probe files and mock downloads ended up sitting in the user's folder.
//!
//! The duplication is deliberate and small. A single copy would have to be
//! public production code that the product never calls, which is a worse trade
//! than twenty lines written twice.

use std::path::Path;

use crate::config::AppPaths;
use crate::models::settings::default_download_directory;
use crate::storage::filenames::resolved_is_within;

/// The two directories on this machine that a test must never write into.
///
/// The configured folder is deliberately not read from the real database: a
/// test must not open the user's database to find out where not to write, and
/// the default is exactly what a harness that configures nothing will use.
pub fn real_paths() -> (std::path::PathBuf, std::path::PathBuf) {
    let download = std::path::PathBuf::from(default_download_directory());
    let data = AppPaths::resolve()
        .map(|paths| paths.data_dir)
        .unwrap_or_else(|_| std::path::PathBuf::from("<unresolvable>"));
    (download, data)
}

fn overlaps(candidate: &Path, real: &Path) -> bool {
    resolved_is_within(real, candidate) || resolved_is_within(candidate, real)
}

/// Refuse to run a test whose destination overlaps the machine's real data.
///
/// Both sides are resolved first, so a junction, a differently-cased spelling or
/// a path that walks through `..` is caught rather than compared as text.
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

/// Point a booted core at `dir`, then check that it took.
///
/// The check is the point: a settings write that silently did nothing would
/// leave the core on the default while the test believed otherwise.
pub fn redirect_downloads_to(core: &crate::services::AppCore, dir: &Path, label: &str) {
    assert_isolated_destination(label, dir);
    core.settings()
        .update(|settings| {
            settings.download_directory = dir.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("redirect the download directory");

    let applied = std::path::PathBuf::from(core.settings().current().download_directory);
    assert!(
        applied == dir,
        "{label}: the download directory was set to {dir:?} but the core reports {applied:?}"
    );
    assert_isolated_destination(label, &applied);
}
