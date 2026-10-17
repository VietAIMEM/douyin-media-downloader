//! The rules that keep automated tests out of the user's own files.
//!
//! These test the guard itself, because the guard is the thing standing between
//! a mistake in a harness and the user's download folder. The mistake already
//! happened once: fifteen mock downloads and ten write-probe files were written
//! into the real `Downloads\Douyin`, beside real downloads, because a harness
//! left the download directory at its default. Nothing here writes outside a
//! temporary directory.

mod support;

use std::path::{Path, PathBuf};

/// Run `f` and report whether it panicked, without letting the panic escape.
fn refused(f: impl FnOnce()) -> bool {
    // `AssertUnwindSafe` because the point is precisely to catch a panic that
    // consulted shared state; nothing is resumed afterwards.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
}

#[test]
fn the_machines_real_directories_are_what_the_guard_compares_against() {
    // Both come from the same places the application reads them, so the guard
    // says what would actually be used rather than what a test assumes.
    let (download, data) = support::real_paths();
    assert!(download.is_absolute(), "got {download:?}");
    assert!(data.is_absolute(), "got {data:?}");
    assert_ne!(download, data);
    println!("the machine's download folder: {download:?}");
    println!("the machine's data folder:     {data:?}");
}

#[test]
fn a_disposable_directory_is_accepted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let dir = support::workspace_subdir(temp.path(), "downloads", "the isolation test");
    // No panic, and the directory is really there and really empty.
    assert!(dir.is_dir());
    assert_eq!(std::fs::read_dir(&dir).expect("read").count(), 0);
}

#[test]
fn the_real_download_folder_is_refused() {
    let (real, _data) = support::real_paths();
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &real)),
        "the guard must refuse the machine's own download folder"
    );
}

#[test]
fn the_real_download_folders_descendants_are_refused() {
    // The case that actually happened: not the folder itself, but a path inside
    // it, produced by the default templates.
    let (real, _data) = support::real_paths();
    let inside = real.join("Alice").join("Videos");
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &inside)),
        "a path inside the real download folder must be refused, however deep"
    );

    // And a directory that does not exist yet is still resolved and refused,
    // which is the shape a destination has before a download creates it.
    let (real, _data) = support::real_paths();
    let unborn = real.join("does-not-exist-yet").join("Images");
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &unborn)),
        "a destination that does not exist yet must be resolved, not compared as text"
    );
}

#[test]
fn a_folder_that_contains_the_real_one_is_refused() {
    // The other direction, and the more dangerous one: a test that pointed at
    // the whole Downloads folder, or at a drive root, would overwrite the user's
    // folder without ever naming it.
    let (real, _data) = support::real_paths();
    let parent = real.parent().expect("the download folder has a parent").to_path_buf();
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &parent)),
        "a folder that contains the real download folder must be refused"
    );
}

#[test]
fn the_real_application_data_folder_is_refused() {
    // Writing into the application's own data folder would put test rows in the
    // user's database and test files among their downloaded ones.
    let (_download, data) = support::real_paths();
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &data)),
        "the guard must refuse the application's own data folder"
    );

    let (_download, data) = support::real_paths();
    let inside = data.join("logs");
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &inside)),
        "a path inside the application's data folder must be refused"
    );
}

#[test]
fn a_path_spelled_differently_is_still_the_same_path() {
    // Windows paths are case-insensitive, so a text comparison would let a
    // differently-cased spelling straight through. Both sides are resolved.
    let (real, _data) = support::real_paths();
    let shouted = PathBuf::from(real.to_string_lossy().to_uppercase());
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &shouted)),
        "a path differing only in case is the same path"
    );

    // And a path that walks up and back down again.
    let (real, _data) = support::real_paths();
    let wandering = real.join("..").join(
        real.file_name().expect("the folder has a name"),
    );
    assert!(
        refused(move || support::assert_isolated_destination("a mistake", &wandering)),
        "`..` must be resolved before the comparison, not taken literally"
    );
}

#[test]
fn separate_tests_get_separate_destinations() {
    // The parallel case: eight workspaces at once, every one distinct, every one
    // accepted, and none of them where anything else is writing.
    let mut handles = Vec::new();
    for index in 0..8 {
        handles.push(std::thread::spawn(move || {
            let temp = tempfile::tempdir().expect("temp dir");
            let dir = support::workspace_subdir(
                temp.path(),
                "downloads",
                &format!("the parallel test {index}"),
            );
            std::fs::write(dir.join("only-mine.txt"), b"x").expect("write");
            dir
        }));
    }
    let dirs: Vec<PathBuf> = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread"))
        .collect();

    let mut sorted = dirs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), dirs.len(), "two tests shared a destination");

    // And once each workspace is dropped, nothing of any of them is left.
    for dir in &dirs {
        assert!(!dir.exists(), "{dir:?} outlived its workspace");
    }
}

#[test]
fn a_redirected_core_reports_the_directory_it_was_given() {
    // The half of the guard that catches a settings write which quietly did
    // nothing — the shape the incident had, where the core believed one thing
    // and used another.
    use douyin_downloader_lib::config::AppPaths;
    use douyin_downloader_lib::services::AppCore;

    let temp = tempfile::tempdir().expect("temp dir");
    let paths = AppPaths::with_data_dir(temp.path().join("data"));
    let core = AppCore::bootstrap(paths).expect("bootstrap").core;

    let dir = support::workspace_subdir(temp.path(), "downloads", "the redirected core");
    support::redirect_downloads_to(&core, &dir, "the redirected core");
    support::assert_core_download_dir(&core, &dir, "the redirected core");

    // And the guard would notice if it had not taken: a check against the wrong
    // directory fails rather than passing quietly.
    let other = support::workspace_subdir(temp.path(), "elsewhere", "the redirected core");
    assert!(
        refused(move || {
            support::assert_core_download_dir(&core, &other, "the redirected core");
        }),
        "a core using a different directory than the test asked for must fail the check"
    );
}

#[test]
fn a_core_that_was_never_redirected_is_caught_before_it_can_download() {
    // The exact defect: a test bootstraps the whole application with a temporary
    // *data* directory and nothing else, and the download directory then defaults
    // to the machine's own. The guard refuses that core outright.
    use douyin_downloader_lib::config::AppPaths;
    use douyin_downloader_lib::services::AppCore;

    let temp = tempfile::tempdir().expect("temp dir");
    let paths = AppPaths::with_data_dir(temp.path().join("data"));
    let core = AppCore::bootstrap(paths).expect("bootstrap").core;

    let unconfigured = PathBuf::from(core.settings().current().download_directory);
    assert!(
        refused(move || support::assert_isolated_destination("an unconfigured harness", &unconfigured)),
        "a harness that configured nothing must be refused, not quietly allowed"
    );
}

#[test]
fn a_workspace_nested_inside_the_real_folder_is_refused() {
    // A test whose "temporary" workspace is itself inside the user's folder —
    // which is what a mistaken `TempDir` base would produce.
    let (real, _data) = support::real_paths();
    let pretend_workspace = real.join("pretend-workspace");
    assert!(
        refused(move || {
            support::workspace_subdir(&pretend_workspace, "downloads", "a mistaken workspace");
        }),
        "a workspace inside the real folder must be refused before anything is created"
    );
    let (real, _data) = support::real_paths();
    assert!(
        !real.join("pretend-workspace").exists(),
        "the refused call must not have created anything"
    );
}

/// A guard is only useful if it is actually reachable from the harnesses.
#[test]
fn the_guard_is_reachable_from_the_harnesses_that_write() {
    for file in [
        "tauri_commands.rs",
        "download_pipeline.rs",
        "library.rs",
        "live_douyin.rs",
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(file);
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("assert_isolated_destination") || text.contains("redirect_downloads_to"),
            "{file} bootstraps the application but never checks where it will write"
        );
    }
}
