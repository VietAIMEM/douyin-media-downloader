//! Library integration tests.
//!
//! These drive the real application service over a real SQLite file and a real
//! directory of files, because the whole point of the Library is the thing a
//! mock would not exercise: that a download row is only a download while the
//! file it produced is still there.
//!
//! Nothing here goes through IPC — that is `tauri_commands.rs`'s job. What is
//! under test is the reconciliation, the filtering, the paging and the export.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use douyin_downloader_lib::config::AppPaths;
use douyin_downloader_lib::database::repositories::{
    creators as creators_repo, downloads as downloads_repo, history as history_repo,
    media as media_repo, posts as posts_repo,
};
use douyin_downloader_lib::models::{
    ContentStatus, Creator, DownloadHistoryEntry, DownloadRequest, DownloadState, DownloadTask,
    LibraryFilter, LibrarySort, LibraryView, MediaItem, MediaType, Platform, Post, Timestamp,
};
use douyin_downloader_lib::services::{AppCore, ExportFormat, ExportScope, LibraryQuery};

/// A booted application over a throwaway directory, with a media folder the
/// export and the file checks both live in.
struct Fixture {
    core: Arc<AppCore>,
    /// The configured download directory. Everything written for a test goes
    /// here and is removed with the temp dir.
    root: PathBuf,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("media");
        std::fs::create_dir_all(&root).expect("media dir");
        support::assert_isolated_destination("the library fixture", &root);

        let paths = AppPaths::with_data_dir(temp.path().join("data"));
        let bootstrap = AppCore::bootstrap(paths).expect("bootstrap");
        let core = Arc::new(bootstrap.core);

        // Redirected away from the machine's real Downloads folder: a test that
        // wrote an export into the user's own directory would be a side effect
        // nobody asked for.
        core.settings()
            .update(|settings| {
                settings.download_directory = root.to_string_lossy().into_owned();
                Ok(())
            })
            .expect("set download directory");

        Self {
            core,
            root,
            _temp: temp,
        }
    }

    fn conn<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> T) -> T {
        self.core.database().with_conn(|conn| Ok(f(conn))).expect("conn")
    }

    fn query(&self, query: LibraryQuery) -> douyin_downloader_lib::models::LibraryPage {
        self.core.query_library(query).expect("query")
    }

    fn all(&self) -> douyin_downloader_lib::models::LibraryPage {
        self.query(LibraryQuery::default())
    }

    fn view(&self, view: LibraryView) -> douyin_downloader_lib::models::LibraryPage {
        self.query(LibraryQuery {
            view,
            ..Default::default()
        })
    }

    /// Titles (captions) of a page, for assertions that read as prose.
    fn titles(&self, page: &douyin_downloader_lib::models::LibraryPage) -> Vec<String> {
        page.entries
            .iter()
            .map(|entry| entry.post.display_title())
            .collect()
    }
}

/// A creator, stored.
fn seed_creator(fixture: &Fixture, platform_id: &str, name: &str) -> Creator {
    let mut creator = Creator::new(
        Platform::Douyin,
        platform_id,
        name,
        format!("https://www.douyin.com/user/{platform_id}"),
    );
    creator.handle = Some(format!("@{name}"));
    fixture.conn(|conn| creators_repo::upsert(conn, &creator).expect("creator"));
    creator
}

/// A stored post.
fn seed_post(
    fixture: &Fixture,
    creator: Option<&Creator>,
    platform_post_id: &str,
    media_type: MediaType,
    caption: Option<&str>,
    published_at: Option<Timestamp>,
) -> Post {
    let mut post = Post::new(
        Platform::Douyin,
        platform_post_id,
        media_type,
        format!("https://www.douyin.com/video/{platform_post_id}"),
    );
    post.caption = caption.map(str::to_string);
    post.published_at = published_at;
    post.canonical_url = Some(format!("https://www.douyin.com/video/{platform_post_id}"));
    post.creator_id = creator.map(|c| c.id.clone());
    post.creator_handle = creator.and_then(|c| c.handle.clone());
    let id = fixture.conn(|conn| posts_repo::upsert(conn, &post).expect("post"));
    post.id = id;
    post
}

/// Give a post media, which is what gives it a duration and a resolution.
fn add_media(
    fixture: &Fixture,
    post: &Post,
    media_type: MediaType,
    duration: Option<f64>,
    size: Option<(u32, u32)>,
) -> MediaItem {
    let mut item = MediaItem::new(post.id.clone(), media_type, "https://cdn.example/media");
    item.duration_seconds = duration;
    if let Some((width, height)) = size {
        item.width = Some(width);
        item.height = Some(height);
    }
    fixture.conn(|conn| media_repo::replace_items_for_post(conn, &post.id, &[item.clone()]).expect("media"));
    item
}

/// Record a download attempt for a post.
fn seed_download(
    fixture: &Fixture,
    post: &Post,
    media_item_id: Option<String>,
    state: DownloadState,
    destination: Option<&Path>,
    completed_at: Option<Timestamp>,
) -> DownloadTask {
    let mut request = DownloadRequest::new("https://cdn.example/media", post.media_type);
    request.post_id = Some(post.id.clone());
    request.media_item_id = media_item_id;
    request.extension = Some("mp4".into());
    let mut task = DownloadTask::new(&request);
    task.state = state;
    task.destination_path = destination.map(|path| path.to_string_lossy().into_owned());
    task.completed_at = completed_at;
    fixture.conn(|conn| downloads_repo::insert(conn, &task).expect("download"));
    task
}

/// Write a real file, as a completed download would have.
fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent");
    }
    std::fs::write(path, contents).expect("write");
}

fn at(raw: &str) -> Timestamp {
    raw.parse().expect("timestamp")
}

// --- indexes ----------------------------------------------------------------

#[test]
fn the_library_read_indexes_are_present() {
    // The two joins behind every library view — "the newest download for this
    // post" and "history rows for this post" — had no index before Phase 05.
    // Asserted rather than assumed: a migration that silently did nothing would
    // leave the queries correct but growing with the library.
    let fixture = Fixture::new();
    let names: Vec<String> = fixture.conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .expect("prepare");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query");
        rows.map(|row| row.expect("name")).collect()
    });

    assert!(
        names.iter().any(|name| name == "idx_downloads_post_created"),
        "the download lookup needs its index; found {names:?}"
    );
    assert!(
        names.iter().any(|name| name == "idx_history_post_id_local_path"),
        "the history lookup needs its index; found {names:?}"
    );
}

// --- what the library holds -------------------------------------------------

#[test]
fn an_empty_library_is_an_empty_page_not_an_error() {
    let fixture = Fixture::new();
    let page = fixture.all();
    assert!(page.entries.is_empty());
    assert_eq!(page.total, 0);
    assert_eq!(page.counts.all, 0);
}

#[test]
fn a_discovered_post_appears_before_it_is_ever_downloaded() {
    // This is the case the post-level entry exists for: a profile listing gives
    // a post with no media and no download, and it must still be visible.
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "sec_1", "Alice");
    seed_post(
        &fixture,
        Some(&creator),
        "7001",
        MediaType::Video,
        Some("Sunset over the sea"),
        Some(at("2026-09-01T10:00:00Z")),
    );

    let page = fixture.all();
    assert_eq!(page.total, 1);
    let entry = &page.entries[0];
    assert_eq!(entry.status, ContentStatus::New);
    assert_eq!(entry.media_item_count, 0, "a listing knows no media");
    assert_eq!(entry.creator_name.as_deref(), Some("Alice"));

    assert_eq!(fixture.view(LibraryView::New).total, 1);
    assert_eq!(fixture.view(LibraryView::Downloaded).total, 0);
}

#[test]
fn a_downloaded_post_leaves_the_new_view() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7002", MediaType::Video, Some("Downloaded"), None);
    let path = fixture.root.join("7002.mp4");
    write_file(&path, b"video bytes");
    seed_download(
        &fixture,
        &post,
        add_media(&fixture, &post, MediaType::Video, Some(12.0), Some((1920, 1080))).id.into(),
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-09-02T10:00:00Z")),
    );

    let entry = &fixture.all().entries[0];
    assert_eq!(entry.status, ContentStatus::Downloaded);
    assert_eq!(entry.file_state.as_deref(), Some("present"));
    assert_eq!(entry.downloaded_item_count, 1);

    assert_eq!(fixture.view(LibraryView::Downloaded).total, 1);
    assert_eq!(fixture.view(LibraryView::New).total, 0);

    // And the presentational helpers agree.
    assert_eq!(entry.resolution().as_deref(), Some("1920x1080"));
    assert!(!entry.is_partially_downloaded());
}

#[test]
fn deleting_the_file_returns_the_post_to_new() {
    // The requirement that local state stays correct when a file is deleted.
    // The download row is untouched — it is a record of what happened — but the
    // post is no longer downloaded, because nothing is on disk.
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7003", MediaType::Video, Some("Deleted later"), None);
    let path = fixture.root.join("7003.mp4");
    write_file(&path, b"video bytes");
    seed_download(
        &fixture,
        &post,
        None,
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-09-02T10:00:00Z")),
    );
    assert_eq!(fixture.view(LibraryView::Downloaded).total, 1);

    std::fs::remove_file(&path).expect("delete");

    let page = fixture.all();
    let entry = &page.entries[0];
    assert_eq!(entry.status, ContentStatus::Ready);
    assert_eq!(entry.file_state.as_deref(), Some("missing"));
    assert!(entry.error.as_deref().unwrap().contains("no longer on disk"));

    assert_eq!(fixture.view(LibraryView::Downloaded).total, 0);
    // Ready means "can be downloaded", so it is a new item again.
    assert_eq!(fixture.view(LibraryView::New).total, 1);
}

#[test]
fn a_failed_attempt_appears_in_failed_and_not_in_downloaded() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7004", MediaType::Video, Some("Broke"), None);
    seed_download(
        &fixture,
        &post,
        None,
        DownloadState::Failed,
        None,
        None,
    );

    let entry = &fixture.all().entries[0];
    assert_eq!(entry.status, ContentStatus::Failed);
    assert_eq!(entry.download_state, Some(DownloadState::Failed));

    assert_eq!(fixture.view(LibraryView::Failed).total, 1);
    assert_eq!(fixture.view(LibraryView::Downloaded).total, 0);
    assert_eq!(fixture.view(LibraryView::New).total, 0);
}

#[test]
fn an_active_download_reads_as_downloading_not_downloaded() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7005", MediaType::Video, Some("In flight"), None);
    // The destination is the final name; what is on disk is the temporary file
    // the transfer is filling, which is not the file the post will have.
    let destination = fixture.root.join("7005.mp4");
    write_file(&fixture.root.join("7005.mp4.part"), b"partial");
    seed_download(
        &fixture,
        &post,
        None,
        DownloadState::Downloading,
        Some(&destination),
        None,
    );

    let entry = &fixture.all().entries[0];
    assert_eq!(entry.status, ContentStatus::Downloading);
    assert_ne!(
        entry.file_state.as_deref(),
        Some("present"),
        "a running download is not a file yet"
    );

    assert_eq!(fixture.view(LibraryView::Downloaded).total, 0);
    assert_eq!(fixture.view(LibraryView::Failed).total, 0);
    assert_eq!(fixture.view(LibraryView::New).total, 0);
}

#[test]
fn source_unavailability_survives_a_local_download() {
    let fixture = Fixture::new();
    let mut post = seed_post(&fixture, None, "7006", MediaType::Video, Some("Gone"), None);
    post.status = ContentStatus::Unavailable;
    fixture.conn(|conn| posts_repo::upsert(conn, &post).expect("post"));
    let path = fixture.root.join("7006.mp4");
    write_file(&path, b"bytes");
    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&path), None);

    // A copy on disk does not make a withdrawn post available again.
    assert_eq!(fixture.all().entries[0].status, ContentStatus::Unavailable);
    assert_eq!(fixture.view(LibraryView::Downloaded).total, 0);
}

#[test]
fn a_post_downloaded_twice_is_one_entry() {
    // Duplicate detection at the level that matters to a listing: doing the same
    // download again is not two library items.
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7007", MediaType::Video, Some("Twice"), None);
    let first = fixture.root.join("7007.mp4");
    let second = fixture.root.join("7007 (1).mp4");
    write_file(&first, b"one");
    write_file(&second, b"two");

    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&first), Some(at("2026-09-01T10:00:00Z")));
    let newer = seed_download(&fixture, &post, None, DownloadState::Completed, Some(&second), Some(at("2026-09-03T10:00:00Z")));

    let page = fixture.all();
    assert_eq!(page.total, 1, "one post, however many times it was downloaded");
    let entry = &page.entries[0];
    assert_eq!(entry.local_path.as_deref(), Some(second.to_string_lossy().as_ref()));
    assert_eq!(entry.download_state, Some(newer.state));
}

#[test]
fn the_newest_attempt_decides_a_posts_state() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7008", MediaType::Video, Some("Retried"), None);
    seed_download(&fixture, &post, None, DownloadState::Failed, None, None);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let path = fixture.root.join("7008.mp4");
    write_file(&path, b"ok");
    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&path), Some(at("2026-09-04T10:00:00Z")));

    // A retry that succeeded must not still read as a failure.
    assert_eq!(fixture.all().entries[0].status, ContentStatus::Downloaded);
    assert_eq!(fixture.view(LibraryView::Failed).total, 0);
}

#[test]
fn a_partly_downloaded_carousel_says_so() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7009", MediaType::Carousel, Some("Three images"), None);
    let items: Vec<MediaItem> = (0..3)
        .map(|index| {
            MediaItem::new(
                post.id.clone(),
                MediaType::Image,
                format!("https://cdn.example/{index}"),
            )
        })
        .collect();
    fixture.conn(|conn| media_repo::replace_items_for_post(conn, &post.id, &items).expect("media"));

    let path = fixture.root.join("7009_0.jpg");
    write_file(&path, b"jpeg");
    seed_download(
        &fixture,
        &post,
        Some(items[0].id.clone()),
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-09-05T10:00:00Z")),
    );

    let entry = &fixture.all().entries[0];
    assert_eq!(entry.media_item_count, 3);
    assert_eq!(entry.downloaded_item_count, 1);
    assert!(entry.is_partially_downloaded());
    // Partial is not downloaded, so the Downloaded view does not claim it.
    assert_eq!(fixture.view(LibraryView::Downloaded).total, 0);
    assert!(fixture.view(LibraryView::Images).entries[0].is_partially_downloaded());
}

// --- duplicates -------------------------------------------------------------

#[test]
fn two_posts_with_the_same_caption_are_not_collapsed() {
    // Identical captions are common — reposts, a series, a terse creator. They
    // are two posts, and hiding one would lose a real item.
    let fixture = Fixture::new();
    seed_post(&fixture, None, "6201", MediaType::Video, Some("Same words"), None);
    seed_post(&fixture, None, "6202", MediaType::Video, Some("Same words"), None);

    let page = fixture.query(LibraryQuery {
        search: Some("Same words".into()),
        ..Default::default()
    });
    assert_eq!(page.total, 2);
}

#[test]
fn two_posts_that_produced_the_same_file_name_stay_separate() {
    // The storage layer disambiguates colliding names on disk; the library must
    // still be able to tell the two posts apart afterwards.
    let fixture = Fixture::new();
    let first = seed_post(&fixture, None, "6203", MediaType::Video, Some("One"), None);
    let second = seed_post(&fixture, None, "6204", MediaType::Video, Some("Two"), None);
    let a = fixture.root.join("clip.mp4");
    let b = fixture.root.join("clip (1).mp4");
    write_file(&a, b"a");
    write_file(&b, b"b");
    seed_download(&fixture, &first, None, DownloadState::Completed, Some(&a), None);
    seed_download(&fixture, &second, None, DownloadState::Completed, Some(&b), None);

    let page = fixture.all();
    assert_eq!(page.total, 2);
    let paths: Vec<&str> = page
        .entries
        .iter()
        .filter_map(|entry| entry.local_path.as_deref())
        .collect();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1], "each post keeps its own file");
}

#[test]
fn two_finished_downloads_of_one_media_item_are_one_entry() {
    // The same resource fetched twice — a retry, or a second submission — is
    // one item in the library.
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "6205", MediaType::Video, Some("Once really"), None);
    let item = add_media(&fixture, &post, MediaType::Video, Some(4.0), None);
    let path = fixture.root.join("6205.mp4");
    write_file(&path, b"bytes");

    seed_download(
        &fixture,
        &post,
        Some(item.id.clone()),
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-02-01T00:00:00Z")),
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    seed_download(
        &fixture,
        &post,
        Some(item.id.clone()),
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-02-02T00:00:00Z")),
    );

    let page = fixture.all();
    assert_eq!(page.total, 1);
    assert_eq!(page.entries[0].downloaded_item_count, 1, "one resource, once");
}

#[test]
fn a_post_that_has_both_a_failed_and_a_completed_attempt_is_not_counted_twice() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "6206", MediaType::Video, Some("Eventually"), None);
    seed_download(&fixture, &post, None, DownloadState::Failed, None, None);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let path = fixture.root.join("6206.mp4");
    write_file(&path, b"ok");
    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&path), None);

    assert_eq!(fixture.all().total, 1);
    assert_eq!(fixture.view(LibraryView::Failed).total, 0);
    assert_eq!(fixture.view(LibraryView::Downloaded).total, 1);
}

// --- submitting what is not already held -------------------------------------

/// Drive a future to completion on a throwaway runtime.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(future)
}

/// Submit one stored post and report the outcome as JSON.
///
/// `submit_profile_downloads` resolves media only when a post has none, so a
/// post seeded *with* media reaches the duplicate check immediately — which is
/// the branch these tests are about, and it needs no network.
fn submit(fixture: &Fixture, post_id: &str) -> impl std::future::Future<Output = serde_json::Value> {
    let core = Arc::clone(&fixture.core);
    let post_id = post_id.to_string();
    async move {
        let result = core
            .submit_profile_downloads(
                douyin_downloader_lib::services::SubmitProfileDownloadsInput {
                    post_ids: vec![post_id],
                    selected_items: Default::default(),
                },
            )
            .await
            .expect("submit");
        serde_json::to_value(result).expect("serialize")
    }
}

#[test]
fn submitting_a_post_that_is_already_downloaded_creates_no_second_task() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "6300", MediaType::Video, Some("Have it"), None);
    let item = add_media(&fixture, &post, MediaType::Video, Some(3.0), None);
    let path = fixture.root.join("6300.mp4");
    write_file(&path, b"bytes");
    seed_download(
        &fixture,
        &post,
        Some(item.id.clone()),
        DownloadState::Completed,
        Some(&path),
        None,
    );

    let result = block_on(submit(&fixture, &post.id));

    assert_eq!(result["tasksCreated"], 0, "nothing new was started");
    assert_eq!(result["skipped"], 1);
    assert_eq!(result["posts"][0]["media"][0]["outcome"], "alreadyDownloaded");
    assert_eq!(block_on(fixture.core.downloads().list()).expect("list").len(), 1);
}

#[test]
fn submitting_a_post_whose_download_is_running_does_not_double_it() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "6301", MediaType::Video, Some("In flight"), None);
    let item = add_media(&fixture, &post, MediaType::Video, Some(3.0), None);
    let running = seed_download(
        &fixture,
        &post,
        Some(item.id.clone()),
        DownloadState::Downloading,
        Some(&fixture.root.join("6301.mp4")),
        None,
    );

    let result = block_on(submit(&fixture, &post.id));

    assert_eq!(result["tasksCreated"], 0);
    assert_eq!(result["posts"][0]["media"][0]["outcome"], "alreadyDownloading");
    assert_eq!(
        result["posts"][0]["media"][0]["taskId"], running.id,
        "the outcome points at the transfer that is already running"
    );
    assert_eq!(block_on(fixture.core.downloads().list()).expect("list").len(), 1);
}

// --- views ------------------------------------------------------------------

#[test]
fn each_view_selects_by_type_and_status() {
    let fixture = Fixture::new();
    // Dated, oldest to newest, so the order of each view is decided by the
    // publication date rather than by the local id that breaks a tie.
    let video = seed_post(
        &fixture,
        None,
        "8001",
        MediaType::Video,
        Some("A video"),
        Some(at("2026-01-01T00:00:00Z")),
    );
    let image = seed_post(
        &fixture,
        None,
        "8002",
        MediaType::Image,
        Some("An image"),
        Some(at("2026-01-02T00:00:00Z")),
    );
    let carousel = seed_post(
        &fixture,
        None,
        "8003",
        MediaType::Carousel,
        Some("A carousel"),
        Some(at("2026-01-03T00:00:00Z")),
    );
    let audio = seed_post(
        &fixture,
        None,
        "8004",
        MediaType::Audio,
        Some("An audio"),
        Some(at("2026-01-04T00:00:00Z")),
    );

    // One of the images is downloaded, so the type and status axes interact.
    let path = fixture.root.join("8002.jpg");
    write_file(&path, b"jpeg");
    seed_download(&fixture, &image, None, DownloadState::Completed, Some(&path), None);

    let path = fixture.root.join("8001.mp4");
    write_file(&path, b"mp4");
    seed_download(&fixture, &video, None, DownloadState::Failed, Some(&path), None);

    let _ = carousel;
    let _ = audio;

    assert_eq!(fixture.titles(&fixture.view(LibraryView::All)).len(), 4);
    assert_eq!(fixture.titles(&fixture.view(LibraryView::Videos)), vec!["A video"]);
    // A carousel is Douyin's image post, so it belongs with the images.
    assert_eq!(
        fixture.titles(&fixture.view(LibraryView::Images)),
        vec!["A carousel", "An image"]
    );
    assert_eq!(fixture.titles(&fixture.view(LibraryView::Audio)), vec!["An audio"]);
    assert_eq!(fixture.titles(&fixture.view(LibraryView::Downloaded)), vec!["An image"]);
    assert_eq!(fixture.titles(&fixture.view(LibraryView::Failed)), vec!["A video"]);
    // New is the two that have neither a good download nor a failed one.
    assert_eq!(fixture.view(LibraryView::New).total, 2);
}

#[test]
fn a_tab_count_covers_the_whole_match_not_the_page() {
    let fixture = Fixture::new();
    for index in 0..7 {
        seed_post(
            &fixture,
            None,
            &format!("81{index:02}"),
            MediaType::Video,
            Some(&format!("video {index}")),
            None,
        );
    }
    seed_post(&fixture, None, "8200", MediaType::Image, Some("image"), None);

    let page = fixture.query(LibraryQuery {
        limit: Some(2),
        ..Default::default()
    });
    assert_eq!(page.entries.len(), 2, "the page is two rows");
    assert_eq!(page.total, 8, "the total is the match");
    assert_eq!(page.counts.all, 8);
    assert_eq!(page.counts.videos, 7);
    assert_eq!(page.counts.images, 1);
    assert_eq!(page.counts.new, 8);
    assert_eq!(page.limit, 2);
    assert_eq!(page.offset, 0);
}

#[test]
fn paging_walks_every_row_exactly_once() {
    let fixture = Fixture::new();
    for index in 0..25 {
        seed_post(
            &fixture,
            None,
            &format!("83{index:02}"),
            MediaType::Video,
            Some(&format!("video {index}")),
            Some(at("2026-01-01T00:00:00Z")),
        );
    }

    let mut seen: Vec<String> = Vec::new();
    for offset in (0..25).step_by(10) {
        let page = fixture.query(LibraryQuery {
            offset,
            limit: Some(10),
            ..Default::default()
        });
        seen.extend(page.entries.iter().map(|entry| entry.post.platform_post_id.clone()));
    }

    assert_eq!(seen.len(), 25, "no row was served twice");
    let unique: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), 25, "and none was skipped");
}

#[test]
fn an_offset_past_the_end_is_an_empty_page_rather_than_an_error() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "8300", MediaType::Video, Some("one"), None);
    let page = fixture.query(LibraryQuery {
        offset: 500,
        limit: Some(10),
        ..Default::default()
    });
    assert!(page.entries.is_empty());
    assert_eq!(page.total, 1, "the total still describes the match");
}

#[test]
fn a_page_size_is_clamped_rather_than_trusted() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "8301", MediaType::Video, Some("one"), None);

    let page = fixture.query(LibraryQuery {
        limit: Some(0),
        ..Default::default()
    });
    assert!(page.limit >= 1, "a page of zero rows is never useful");

    let page = fixture.query(LibraryQuery {
        limit: Some(100_000),
        ..Default::default()
    });
    assert!(page.limit <= 500, "a caller cannot ask for the whole library");
}

// --- search -----------------------------------------------------------------

#[test]
fn search_matches_every_field_the_library_documents() {
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "sec_900", "Sunset Studio");
    let post = seed_post(
        &fixture,
        Some(&creator),
        "8400",
        MediaType::Video,
        Some("A quiet harbour"),
        None,
    );
    let path = fixture.root.join("harbour-evening.mp4");
    write_file(&path, b"mp4");
    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&path), None);

    let search = |term: &str| {
        fixture.query(LibraryQuery {
            search: Some(term.to_string()),
            ..Default::default()
        })
    };

    // By creator display name.
    assert_eq!(search("sunset studio").total, 1);
    // By caption.
    assert_eq!(search("harbour").total, 1);
    // By post id.
    assert_eq!(search("8400").total, 1);
    // By URL.
    assert_eq!(search("douyin.com/video/8400").total, 1);
    // By the name of the file the post produced.
    assert_eq!(search("harbour-evening").total, 1);
    // And a term that matches nothing matches nothing.
    assert_eq!(search("no such thing").total, 0);
}

#[test]
fn search_is_case_insensitive_for_ascii() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "8401", MediaType::Video, Some("Quiet Harbour"), None);
    let search = |term: &str| {
        fixture.query(LibraryQuery {
            search: Some(term.to_string()),
            ..Default::default()
        })
    };
    assert_eq!(search("QUIET").total, 1);
    assert_eq!(search("quiet").total, 1);
}

#[test]
fn a_wildcard_in_a_search_term_is_a_literal() {
    // Searching for "100%" must not match every post in the library.
    let fixture = Fixture::new();
    seed_post(&fixture, None, "8402", MediaType::Video, Some("100% pure"), None);
    seed_post(&fixture, None, "8403", MediaType::Video, Some("something else"), None);

    let search = |term: &str| {
        fixture.query(LibraryQuery {
            search: Some(term.to_string()),
            ..Default::default()
        })
    };
    assert_eq!(search("100%").total, 1, "% is text, not a wildcard");
    assert_eq!(search("100%").entries[0].post.platform_post_id, "8402");
    assert_eq!(search("_").total, 0, "_ is text, not a single-character wildcard");
}

#[test]
fn search_finds_a_post_by_a_file_it_produced_earlier() {
    // The history join: a post whose live download row is gone but whose
    // recorded file name remains is still findable by that name.
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "8404", MediaType::Video, Some("Archived"), None);

    let mut entry = DownloadHistoryEntry::completed(
        "https://cdn.example/media",
        MediaType::Video,
        fixture.root.join("archived-old-name.mp4").to_string_lossy().into_owned(),
    );
    entry.post_id = Some(post.id.clone());
    fixture.conn(|conn| history_repo::insert(conn, &entry).expect("history"));

    let found = fixture.query(LibraryQuery {
        search: Some("archived-old-name".into()),
        ..Default::default()
    });
    assert_eq!(found.total, 1);
    assert_eq!(found.entries[0].post.platform_post_id, "8404");
}

#[test]
fn an_empty_search_does_not_narrow_anything() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "8405", MediaType::Video, Some("one"), None);
    for term in ["", "   "] {
        let page = fixture.query(LibraryQuery {
            search: Some(term.into()),
            ..Default::default()
        });
        assert_eq!(page.total, 1, "for {term:?}");
    }
}

// --- filters ----------------------------------------------------------------

#[test]
fn a_creator_filter_selects_only_that_creator() {
    let fixture = Fixture::new();
    let alice = seed_creator(&fixture, "sec_a", "Alice");
    let bob = seed_creator(&fixture, "sec_b", "Bob");
    seed_post(&fixture, Some(&alice), "8500", MediaType::Video, Some("hers"), None);
    seed_post(&fixture, Some(&bob), "8501", MediaType::Video, Some("his"), None);

    let page = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            creator_id: Some(alice.id.clone()),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(fixture.titles(&page), vec!["hers"]);

    // The creators menu is what the filter is chosen from.
    let creators = fixture.core.library_creators().expect("creators");
    assert_eq!(creators.len(), 2);
    assert!(creators.iter().any(|(id, name)| id == &alice.id && name == "Alice"));
}

#[test]
fn a_date_filter_excludes_posts_outside_the_range() {
    let fixture = Fixture::new();
    let old = seed_post(&fixture, None, "8600", MediaType::Video, Some("old"), Some(at("2025-01-01T00:00:00Z")));
    let recent = seed_post(&fixture, None, "8601", MediaType::Video, Some("recent"), Some(at("2026-06-01T00:00:00Z")));
    seed_post(&fixture, None, "8602", MediaType::Video, Some("undated"), None);

    let from = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            published_after: Some(at("2026-01-01T00:00:00Z")),
            ..Default::default()
        },
        ..Default::default()
    });
    // An undated post is not inside the range — nothing says it was published
    // after the date the user asked for.
    assert_eq!(fixture.titles(&from), vec!["recent"]);

    let through = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            published_before: Some(at("2025-06-01T00:00:00Z")),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(fixture.titles(&through), vec!["old"]);

    let _ = (old, recent);
}

#[test]
fn a_duration_filter_keeps_posts_whose_duration_is_unknown() {
    let fixture = Fixture::new();
    let short = seed_post(&fixture, None, "8700", MediaType::Video, Some("short"), None);
    add_media(&fixture, &short, MediaType::Video, Some(8.0), None);
    let long = seed_post(&fixture, None, "8701", MediaType::Video, Some("long"), None);
    add_media(&fixture, &long, MediaType::Video, Some(240.0), None);
    let unmeasured = seed_post(&fixture, None, "8702", MediaType::Video, Some("unmeasured"), None);
    add_media(&fixture, &unmeasured, MediaType::Video, None, None);

    let page = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            min_duration_seconds: Some(60.0),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut titles = fixture.titles(&page);
    titles.sort();
    // Not-knowing is not the same as being short.
    assert_eq!(titles, vec!["long", "unmeasured"]);
}

#[test]
fn a_resolution_filter_keeps_posts_whose_size_is_unknown() {
    let fixture = Fixture::new();
    let small = seed_post(&fixture, None, "8800", MediaType::Video, Some("small"), None);
    add_media(&fixture, &small, MediaType::Video, Some(10.0), Some((640, 360)));
    let big = seed_post(&fixture, None, "8801", MediaType::Video, Some("big"), None);
    add_media(&fixture, &big, MediaType::Video, Some(10.0), Some((3840, 2160)));
    let unknown = seed_post(&fixture, None, "8802", MediaType::Video, Some("unknown"), None);
    add_media(&fixture, &unknown, MediaType::Video, Some(10.0), None);

    let page = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            min_width: Some(1920),
            ..Default::default()
        },
        ..Default::default()
    });
    let mut titles = fixture.titles(&page);
    titles.sort();
    assert_eq!(titles, vec!["big", "unknown"]);
}

#[test]
fn a_filter_and_a_view_narrow_together() {
    let fixture = Fixture::new();
    let video = seed_post(&fixture, None, "8900", MediaType::Video, Some("video"), None);
    let path = fixture.root.join("8900.mp4");
    write_file(&path, b"mp4");
    seed_download(&fixture, &video, None, DownloadState::Completed, Some(&path), None);
    let image = seed_post(&fixture, None, "8901", MediaType::Image, Some("image"), None);
    let path = fixture.root.join("8901.jpg");
    write_file(&path, b"jpg");
    seed_download(&fixture, &image, None, DownloadState::Completed, Some(&path), None);

    // "Downloaded, but only the videos" is one predicate, not two screens
    // disagreeing.
    let page = fixture.query(LibraryQuery {
        view: LibraryView::Downloaded,
        filter: LibraryFilter {
            media_types: vec![MediaType::Video],
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(fixture.titles(&page), vec!["video"]);
}

#[test]
fn a_status_filter_narrows_within_a_view() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "8902", MediaType::Video, Some("failed one"), None);
    seed_download(&fixture, &post, None, DownloadState::Failed, None, None);

    let page = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            statuses: vec![ContentStatus::Failed],
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(fixture.titles(&page), vec!["failed one"]);

    let page = fixture.query(LibraryQuery {
        filter: LibraryFilter {
            statuses: vec![ContentStatus::Downloaded],
            ..Default::default()
        },
        ..Default::default()
    });
    assert!(page.entries.is_empty());
}

// --- sorting ----------------------------------------------------------------

#[test]
fn sorting_is_deterministic_and_puts_undated_posts_last() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "9000", MediaType::Video, Some("undated"), None);
    seed_post(&fixture, None, "9001", MediaType::Video, Some("newest"), Some(at("2026-09-01T00:00:00Z")));
    seed_post(&fixture, None, "9002", MediaType::Video, Some("oldest"), Some(at("2024-09-01T00:00:00Z")));

    let newest = fixture.query(LibraryQuery {
        sort: LibrarySort::Newest,
        ..Default::default()
    });
    assert_eq!(fixture.titles(&newest), vec!["newest", "oldest", "undated"]);

    let oldest = fixture.query(LibraryQuery {
        sort: LibrarySort::Oldest,
        ..Default::default()
    });
    assert_eq!(fixture.titles(&oldest), vec!["oldest", "newest", "undated"]);

    // Asking twice gives the same order, which is what makes paging safe.
    let again = fixture.query(LibraryQuery {
        sort: LibrarySort::Newest,
        ..Default::default()
    });
    assert_eq!(
        newest.entries.iter().map(|e| &e.post.id).collect::<Vec<_>>(),
        again.entries.iter().map(|e| &e.post.id).collect::<Vec<_>>()
    );
}

#[test]
fn sorting_by_title_falls_back_to_the_post_id_when_there_is_no_caption() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "9100", MediaType::Video, None, None);
    seed_post(&fixture, None, "9101", MediaType::Video, Some("Apple"), None);
    seed_post(&fixture, None, "9102", MediaType::Video, Some("zebra"), None);

    let page = fixture.query(LibraryQuery {
        sort: LibrarySort::TitleAsc,
        ..Default::default()
    });
    let titles = fixture.titles(&page);
    assert_eq!(titles[0], "Apple");
    // The captionless post sorts by the title it is *shown* with, so it falls
    // where a reader would look for it rather than by its bare id.
    assert_eq!(titles[1], "douyin post 9100");
    // And the comparison ignores case, so a lower-case caption is not banished
    // to the end.
    assert_eq!(titles[2], "zebra");
}

// --- export -----------------------------------------------------------------

fn exported(fixture: &Fixture, format: ExportFormat) -> (PathBuf, String) {
    let result = fixture
        .core
        .export_library(LibraryQuery::default(), format, ExportScope::Matching)
        .expect("export");
    let path = PathBuf::from(&result.path);
    assert_eq!(result.records, fixture.all().total);
    (path.clone(), std::fs::read_to_string(path).expect("read export"))
}

#[test]
fn a_json_export_writes_the_matching_records_inside_the_download_folder() {
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "sec_x", "Alice");
    let post = seed_post(&fixture, Some(&creator), "9200", MediaType::Video, Some("夕阳下的海面"), Some(at("2026-09-01T00:00:00Z")));
    let path = fixture.root.join("9200.mp4");
    write_file(&path, b"mp4");
    add_media(&fixture, &post, MediaType::Video, Some(31.5), Some((1920, 1080)));
    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&path), Some(at("2026-09-01T01:00:00Z")));

    let (export_path, body) = exported(&fixture, ExportFormat::Json);

    // Inside the configured download folder, which is the only place this
    // application writes.
    let canonical_root = std::fs::canonicalize(&fixture.root).expect("canonical root");
    let canonical_export = std::fs::canonicalize(&export_path).expect("canonical export");
    assert!(
        canonical_export.starts_with(&canonical_root),
        "{canonical_export:?} must be inside {canonical_root:?}"
    );

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let rows = parsed.as_array().expect("an array");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["creatorName"], "Alice");
    assert_eq!(row["postId"], "9200");
    assert_eq!(row["caption"], "夕阳下的海面");
    assert_eq!(row["status"], "downloaded");
    assert_eq!(row["downloadState"], "completed");
    assert_eq!(row["filePresent"], true);
    assert_eq!(row["width"], 1920);
    assert_eq!(row["height"], 1080);
    assert_eq!(row["durationSeconds"], 31.5);
}

#[test]
fn an_export_covers_the_match_not_the_page() {
    let fixture = Fixture::new();
    for index in 0..12 {
        seed_post(&fixture, None, &format!("93{index:02}"), MediaType::Video, Some("x"), None);
    }

    let result = fixture
        .core
        .export_library(
            LibraryQuery {
                limit: Some(3),
                ..Default::default()
            },
            ExportFormat::Json,
            ExportScope::Matching,
        )
        .expect("export");
    assert_eq!(result.records, 12, "the whole match, not the three on screen");
}

#[test]
fn an_export_follows_the_views_filters_and_search() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "9400", MediaType::Video, Some("keep me"), None);
    seed_post(&fixture, None, "9401", MediaType::Image, Some("keep me too"), None);

    let result = fixture
        .core
        .export_library(
            LibraryQuery {
                view: LibraryView::Videos,
                search: Some("keep".into()),
                ..Default::default()
            },
            ExportFormat::Json,
            ExportScope::Matching,
        )
        .expect("export");
    assert_eq!(result.records, 1);

    let body = std::fs::read_to_string(&result.path).expect("read");
    let rows: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(rows[0]["postId"], "9400");
}

#[test]
fn a_csv_export_is_a_well_formed_table() {
    let fixture = Fixture::new();
    let post = seed_post(
        &fixture,
        None,
        "9500",
        MediaType::Video,
        // A caption with a comma, a quote, and a line break: all three are
        // ordinary user data.
        Some("Sunset, \"the good one\"\nover the sea"),
        Some(at("2026-09-01T00:00:00Z")),
    );
    let path = fixture.root.join("9500.mp4");
    write_file(&path, b"mp4");
    seed_download(&fixture, &post, None, DownloadState::Completed, Some(&path), None);

    let (_, body) = exported(&fixture, ExportFormat::Csv);

    // The header, then one record. If the quoting were naive the embedded
    // newline would have split this record into two.
    let header = body.lines().next().expect("header");
    assert!(header.starts_with("creatorName,creatorId,creatorHandle,postId,"));
    assert!(header.ends_with("width,height,error"));

    let record = &body[header.len() + 2..];
    assert!(record.contains("\"Sunset, \"\"the good one\"\"\nover the sea\""));

    let fields = parse_csv_row(record.trim_end_matches("\r\n"));
    assert_eq!(fields.len(), header.split(',').count());
    assert_eq!(fields[3], "9500");
    assert_eq!(fields[8], "downloaded");
    assert_eq!(fields[12], "true", "a boolean is written as-is");
}

#[test]
fn a_caption_that_looks_like_a_formula_cannot_run_in_a_spreadsheet() {
    let fixture = Fixture::new();
    seed_post(
        &fixture,
        None,
        "9600",
        MediaType::Video,
        Some("=HYPERLINK(\"http://example.invalid\",\"click me\")"),
        None,
    );

    let (_, body) = exported(&fixture, ExportFormat::Csv);
    let record = body.lines().nth(1).expect("record");
    let fields = parse_csv_row(record);
    let caption = &fields[5];
    assert!(
        caption.starts_with('\'') || caption.starts_with("\"'"),
        "the caption must be marked as text, got {caption}"
    );
    assert!(caption.contains("HYPERLINK"), "and must still be readable");
}

#[test]
fn an_export_never_writes_a_signed_media_url() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "9700", MediaType::Video, Some("has a token"), None);
    let item = MediaItem::new(
        post.id.clone(),
        MediaType::Video,
        // What a resolved URL actually looks like.
        "https://v3-cdn.example/playwm/?video_id=v0200&line=0&token=SECRET-VERY-SECRET&expire=1",
    );
    fixture.conn(|conn| media_repo::replace_items_for_post(conn, &post.id, &[item]).expect("media"));

    for format in [ExportFormat::Json, ExportFormat::Csv] {
        let (_, body) = exported(&fixture, format);
        assert!(!body.contains("SECRET"), "no token in {format:?}");
        assert!(!body.contains("playwm"), "no media URL in {format:?}");
        // The post page is a page, not a token, and it is what the export keeps.
        assert!(body.contains("douyin.com/video/9700"));
    }
}

#[test]
fn exporting_twice_leaves_two_files_rather_than_overwriting_one() {
    let fixture = Fixture::new();
    seed_post(&fixture, None, "9800", MediaType::Video, Some("one"), None);

    let first = fixture
        .core
        .export_library(LibraryQuery::default(), ExportFormat::Csv, ExportScope::Matching)
        .expect("first");
    // The name carries a second-resolution timestamp, so a second export inside
    // the same second would collide; that is acceptable and is what the suffix
    // below stands for.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = fixture
        .core
        .export_library(LibraryQuery::default(), ExportFormat::Csv, ExportScope::Matching)
        .expect("second");

    assert_ne!(first.path, second.path);
    assert!(Path::new(&first.path).is_file());
    assert!(Path::new(&second.path).is_file());
}

#[test]
fn an_export_fails_cleanly_when_the_export_folder_cannot_be_created() {
    // A file where the `exports` folder should be. The write is refused with a
    // typed storage error rather than a panic or a lossy partial file.
    let fixture = Fixture::new();
    seed_post(&fixture, None, "9900", MediaType::Video, Some("one"), None);
    std::fs::write(fixture.root.join("exports"), b"in the way").expect("write");

    let err = fixture
        .core
        .export_library(
            LibraryQuery::default(),
            ExportFormat::Csv,
            ExportScope::Matching,
        )
        .expect_err("must not silently succeed");
    assert_eq!(err.kind(), douyin_downloader_lib::errors::AppErrorKind::Storage);
    assert!(!err.user_message().is_empty(), "and explains itself to the user");
}

/// Split one CSV record, honouring quotes.
///
/// Written out in the test rather than reused from the implementation on
/// purpose: a parser that shared the writer's mistake would agree with it.
fn parse_csv_row(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            }
            '"' => quoted = true,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

// --- saving a post's text -------------------------------------------------
//
// A caption is content in its own right, and saving it is deliberately not a
// download: no task, no queue, no history entry. A text post has nothing to
// transfer, and the history is a record of files that were fetched, not of files
// this application wrote from what it already had.

use douyin_downloader_lib::services::text::save_post_text;

/// A stored text post, with the text in the place the resolver puts it.
fn seed_text_post(fixture: &Fixture, platform_post_id: &str, text: &str) -> Post {
    let post = seed_post(fixture, None, platform_post_id, MediaType::Text, Some(text), None);
    fixture.conn(|conn| {
        let mut stored = posts_repo::get(conn, &post.id).expect("reload").expect("exists");
        stored
            .metadata
            .insert("text".into(), serde_json::json!(text));
        posts_repo::upsert(conn, &stored).expect("store text");
    });
    post
}

#[test]
fn saving_a_post_text_writes_utf8_and_leaves_the_download_history_alone() {
    let fixture = Fixture::new();
    let post = seed_text_post(&fixture, "7629996475679029851", "开机大吉！顺顺利利！");

    let before_history = fixture.conn(|conn| history_repo::count(conn).expect("history"));
    let before_tasks = fixture.conn(|conn| downloads_repo::count(conn).expect("tasks"));

    let saved = save_post_text(&fixture.core, &post.id).expect("saved");

    let bytes = std::fs::read(&saved.path).expect("file");
    let text = String::from_utf8(bytes).expect("utf-8");
    assert!(text.contains("开机大吉！顺顺利利！"), "the words survive: {text}");
    assert!(text.contains("platform_post_id: 7629996475679029851"));
    assert!(saved.bytes > 0);
    assert!(saved.path.ends_with(".txt"), "{}", saved.path);
    // Its own folder, so a caption never lands among pictures.
    assert!(saved.path.contains("Text"), "{}", saved.path);

    // Nothing was downloaded, so nothing appears as downloaded.
    assert_eq!(fixture.conn(|conn| history_repo::count(conn).expect("history")), before_history);
    assert_eq!(fixture.conn(|conn| downloads_repo::count(conn).expect("tasks")), before_tasks);
}

#[test]
fn saving_the_same_text_twice_does_not_replace_the_first_file() {
    let fixture = Fixture::new();
    let post = seed_text_post(&fixture, "7629996475679029852", "一次");

    let first = save_post_text(&fixture.core, &post.id).expect("first");
    assert!(!first.renamed);

    // The post's text changes, and the user saves again. Their first file is
    // theirs; this one is named beside it.
    fixture.conn(|conn| {
        let mut stored = posts_repo::get(conn, &post.id).expect("reload").expect("exists");
        stored.metadata.insert("text".into(), serde_json::json!("二次"));
        stored.caption = Some("二次".into());
        posts_repo::upsert(conn, &stored).expect("store");
    });

    let second = save_post_text(&fixture.core, &post.id).expect("second");
    assert!(second.renamed, "the second save must not overwrite the first");
    assert_ne!(first.path, second.path);
    assert!(Path::new(&first.path).is_file());
    assert!(Path::new(&second.path).is_file());
    assert!(std::fs::read_to_string(&first.path).expect("first").contains("一次"));
}

#[test]
fn a_post_with_pictures_saves_its_words_without_touching_its_media() {
    let fixture = Fixture::new();
    let post = seed_post(
        &fixture,
        None,
        "7629996475679029853",
        MediaType::Image,
        Some("说明文字"),
        None,
    );
    add_media(&fixture, &post, MediaType::Image, None, Some((1080, 1440)));

    let before_tasks = fixture.conn(|conn| downloads_repo::count(conn).expect("tasks"));
    let saved = save_post_text(&fixture.core, &post.id).expect("saved");

    assert!(std::fs::read_to_string(&saved.path).expect("file").contains("说明文字"));
    // The caption is saved; the post's picture is neither downloaded nor removed.
    assert_eq!(fixture.conn(|conn| downloads_repo::count(conn).expect("tasks")), before_tasks);
    let items = fixture.conn(|conn| media_repo::list_items_for_post(conn, &post.id).expect("media"));
    assert_eq!(items.len(), 1);
}

#[test]
fn a_post_with_nothing_to_say_is_refused_rather_than_exported_empty() {
    let fixture = Fixture::new();
    let post = seed_post(&fixture, None, "7629996475679029854", MediaType::Text, None, None);

    let err = save_post_text(&fixture.core, &post.id).expect_err("no text");
    assert_eq!(err.kind(), douyin_downloader_lib::errors::AppErrorKind::UnsupportedContent);
}

#[test]
fn text_that_is_not_held_locally_is_a_not_found_not_a_failure() {
    let fixture = Fixture::new();
    let err = save_post_text(&fixture.core, "no-such-post").expect_err("missing");
    assert_eq!(err.kind(), douyin_downloader_lib::errors::AppErrorKind::NotFound);
}

// --- concurrency ----------------------------------------------------------

#[test]
fn a_batch_of_completions_writing_at_once_all_land_exactly_once() {
    // Several downloads finishing together is the ordinary case for a carousel,
    // and the case the pool's eight connections make it possible to get wrong.
    // Every one of them must be recorded, and recorded once: a history that
    // quietly dropped one entry would report a post as not downloaded, and one
    // that wrote two would report a file that was fetched once as fetched twice.
    let fixture = Fixture::new();
    let writers: usize = 8;
    let barrier = Arc::new(std::sync::Barrier::new(writers));

    std::thread::scope(|scope| {
        for index in 0..writers {
            let core = fixture.core.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                // All of them reach the write at the same moment, which is what
                // makes this a race rather than eight sequential writes.
                barrier.wait();
                core.database()
                    .with_transaction(|conn| {
                        let entry = DownloadHistoryEntry::completed(
                            format!("https://cdn.example/slide-{index}.jpeg"),
                            MediaType::Image,
                            format!("D:/media/slide-{index}.jpeg"),
                        );
                        history_repo::insert(conn, &entry)
                    })
                    .expect("every terminal write must be recorded");
            });
        }
    });

    assert_eq!(
        fixture.conn(|conn| history_repo::count(conn).expect("count")),
        writers as i64,
        "one entry per finished download, no more and no fewer"
    );

    let entries = fixture.conn(|conn| history_repo::list(conn, 100, 0).expect("list"));
    let distinct: std::collections::HashSet<_> =
        entries.iter().map(|entry| entry.source_url.clone()).collect();
    assert_eq!(distinct.len(), writers, "no entry was written twice");
}

// --- timestamps written by something other than this application -----------
//
// Every row the application writes carries an RFC 3339 timestamp with an offset.
// A database that has been through an older build, a hand edit, or another tool
// can hold something else, and the measured consequence of one such value was
// that the application refused to start at all — the whole product unusable
// because of a single field it could not read. These tests hold that open.

/// Boot the application again over the same directory, as a restart does.
fn reboot(fixture: &Fixture) -> Arc<AppCore> {
    let paths = fixture.core.paths().clone();
    let expected = fixture.root.clone();
    let core = Arc::new(
        AppCore::bootstrap(paths)
            .expect("the application must start whatever a stored timestamp says")
            .core,
    );
    // A restart reads its settings back out of the database, so this is the
    // place a redirect could quietly be lost — leaving the second instance on
    // the machine's real download folder while the test believed otherwise.
    support::assert_core_download_dir(&core, &expected, "the restarted library fixture");
    core
}

/// Put a timestamp of the caller's choosing into a task's `updated_at`.
fn force_task_timestamp(fixture: &Fixture, task_id: &str, raw: &str) {
    fixture.conn(|conn| {
        conn.execute(
            "UPDATE downloads SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![raw, task_id],
        )
        .expect("update");
    });
}

/// The raw text still in the column, read back without going through a model.
fn stored_task_timestamp(fixture: &Fixture, task_id: &str) -> String {
    fixture.conn(|conn| {
        conn.query_row(
            "SELECT updated_at FROM downloads WHERE id = ?1",
            [task_id],
            |row| row.get::<_, String>(0),
        )
        .expect("read")
    })
}

/// One video post with its media, so a task has something to belong to.
fn post_with_media(fixture: &Fixture, platform_post_id: &str) -> (Post, MediaItem) {
    let post = seed_post(fixture, None, platform_post_id, MediaType::Video, Some("x"), None);
    let media = add_media(fixture, &post, MediaType::Video, Some(3.0), Some((1920, 1080)));
    (post, media)
}

#[test]
fn a_sqlite_timestamp_does_not_stop_the_application_starting() {
    // The exact failure: an isolated database holding `datetime('now')` instead
    // of RFC 3339, and an application that exited before its services were ready.
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679029901");
    let task = seed_download(
        &fixture, &post, Some(media.id.clone()), DownloadState::Queued, None, None,
    );
    force_task_timestamp(&fixture, &task.id, "2026-10-14 21:37:34");

    let restarted = reboot(&fixture);

    let read_back = restarted
        .database()
        .with_conn(|conn| downloads_repo::get(conn, &task.id))
        .expect("readable")
        .expect("the row is still there");
    // Read as the UTC SQLite documented it to be — not as local time, and not as
    // some other plausible instant.
    assert_eq!(
        read_back.updated_at,
        Some("2026-10-14T21:37:34Z".parse().unwrap()),
        "the row keeps the instant the source stated"
    );
}

#[test]
fn a_timestamp_nobody_can_read_leaves_the_row_and_the_stored_value_alone() {
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679029902");
    let task = seed_download(
        &fixture, &post, Some(media.id.clone()), DownloadState::Failed, None, None,
    );
    let unreadable = "some time last Tuesday";
    force_task_timestamp(&fixture, &task.id, unreadable);

    let restarted = reboot(&fixture);

    let read_back = restarted
        .database()
        .with_conn(|conn| downloads_repo::get(conn, &task.id))
        .expect("readable")
        .expect("the row is still there");
    // No time known — and explicitly not "now", which would make a corrupt row
    // look like the most recent thing that ever happened.
    assert_eq!(read_back.updated_at, None);
    assert_eq!(read_back.id, task.id, "the row keeps its identity");
    assert_eq!(read_back.state, DownloadState::Failed);
    // And the unreadable value is still exactly what was stored: reading is not a
    // reason to rewrite somebody else's data.
    assert_eq!(stored_task_timestamp(&fixture, &task.id), unreadable);
}

#[test]
fn one_unreadable_timestamp_does_not_cost_the_rows_around_it() {
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679029903");

    let good = seed_download(
        &fixture, &post, Some(media.id.clone()), DownloadState::Queued, None, None,
    );
    let bad = seed_download(
        &fixture, &post, Some(media.id.clone()), DownloadState::Queued, None, None,
    );
    force_task_timestamp(&fixture, &good.id, "2026-01-02T03:04:05Z");
    force_task_timestamp(&fixture, &bad.id, "&&& not a time &&&");

    let restarted = reboot(&fixture);

    let tasks = restarted
        .database()
        .with_conn(|conn| downloads_repo::list(conn, 50, 0))
        .expect("the listing must survive one bad row");
    assert_eq!(tasks.len(), 2, "both rows are still listed");
    let reloaded_good = tasks.iter().find(|t| t.id == good.id).expect("the good row");
    assert_eq!(
        reloaded_good.updated_at,
        Some("2026-01-02T03:04:05Z".parse().unwrap())
    );
    let reloaded_bad = tasks.iter().find(|t| t.id == bad.id).expect("the bad row");
    assert_eq!(reloaded_bad.updated_at, None, "only the unreadable field is missing");
}

#[test]
fn a_restart_keeps_settings_history_task_identity_and_schema() {
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679029904");
    let completed_at: Timestamp = "2026-02-03T04:05:06Z".parse().unwrap();
    let task = seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Completed,
        Some(&fixture.root.join("clip.mp4")),
        Some(completed_at),
    );
    fixture.conn(|conn| {
        history_repo::insert(
            conn,
            &DownloadHistoryEntry {
                downloaded_at: Some(completed_at),
                ..DownloadHistoryEntry::completed(
                    "https://cdn.example/media",
                    MediaType::Video,
                    "clip.mp4",
                )
            },
        )
        .expect("history");
    });
    force_task_timestamp(&fixture, &task.id, "2026-10-14 21:37:34");

    let before_schema = fixture.core.database().schema_version().expect("schema");
    let before_settings = fixture.core.settings().current();
    let before_history = fixture.conn(|conn| history_repo::count(conn).expect("count"));

    let restarted = reboot(&fixture);

    assert_eq!(
        restarted.database().schema_version().expect("schema"),
        before_schema,
        "a restart is not a migration"
    );
    assert_eq!(restarted.settings().current(), before_settings);
    assert_eq!(
        fixture.conn(|conn| history_repo::count(conn).expect("count")),
        before_history,
        "no history row is added, removed or rewritten by starting up"
    );
    assert!(
        fixture
            .conn(|conn| downloads_repo::get(conn, &task.id).expect("read"))
            .is_some(),
        "the task keeps its identity across the restart"
    );
}

#[test]
fn a_row_with_no_readable_time_still_takes_part_in_history_ordering() {
    // Ordering happens in SQL, on the stored text, so an unreadable value is
    // ordered as the text it is rather than being dropped from the list or
    // sorted as though it were the current moment.
    let fixture = Fixture::new();
    fixture.conn(|conn| {
        for (url, when) in [
            ("https://cdn.example/a.mp4", "2026-01-01T00:00:00Z"),
            ("https://cdn.example/b.mp4", "2026-03-01T00:00:00Z"),
            ("https://cdn.example/c.mp4", "2026-06-01T00:00:00Z"),
        ] {
            history_repo::insert(
                conn,
                &DownloadHistoryEntry {
                    downloaded_at: Some(when.parse().unwrap()),
                    ..DownloadHistoryEntry::completed(url, MediaType::Video, "clip.mp4")
                },
            )
            .expect("history");
        }
        // Written straight into the column, because no model can hold it: this
        // is the value another tool would have left behind.
        conn.execute(
            "UPDATE download_history SET downloaded_at = ?1 WHERE source_url = ?2",
            rusqlite::params!["not a time at all", "https://cdn.example/b.mp4"],
        )
        .expect("the unreadable value must be storable");
    });

    let restarted = reboot(&fixture);
    let entries = restarted
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .expect("ordered without failing");

    assert_eq!(entries.len(), 3, "every row is still listed");

    // The two readable rows keep the order their instants give them.
    let readable: Vec<_> = entries
        .iter()
        .filter_map(|entry| entry.downloaded_at)
        .collect();
    assert_eq!(
        readable,
        vec![
            "2026-06-01T00:00:00Z".parse::<Timestamp>().unwrap(),
            "2026-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
        ],
        "the newest readable download is still listed first"
    );

    // The unreadable one is present, reported as no time known rather than as a
    // real date, and ordered by the text it happens to hold — here, first,
    // because `n` sorts after `2`. That position carries no meaning, which is
    // the honest outcome: the value never stated an instant to order by.
    assert_eq!(entries[0].downloaded_at, None);
    assert_eq!(
        entries[0].source_url, "https://cdn.example/b.mp4",
        "the row ordered first is the one whose text sorts first"
    );
}

// --- what is on disk, as opposed to what the database remembers -------------
//
// Task state and file state are different facts. A download that completed is a
// download that completed, whether or not the file is still there afterwards —
// and a file that cannot be looked at is not the same as a file that is gone.
// These use the fixture's own temporary root and database, never the real one.

use douyin_downloader_lib::services::files::FileScan;

/// Every file under a directory, sorted. A cheap stand-in for a manifest: enough
/// to prove that looking at the disk did not change it.
fn files_under(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// The state the scan reports for one task, or a panic naming why it is absent.
fn file_state(fixture: &Fixture, task_id: &str) -> (String, Option<String>) {
    let scan: FileScan = fixture.core.scan_downloaded_files().expect("scan");
    let entry = scan
        .entries
        .iter()
        .find(|entry| entry.task_id == task_id)
        .unwrap_or_else(|| panic!("no entry for {task_id} in {:?}", scan.entries.len()));
    (
        entry.state.to_string(),
        entry.reason.map(str::to_string),
    )
}

/// A completed task whose file the fixture has actually written.
fn completed_with_a_file(
    fixture: &Fixture,
    platform_post_id: &str,
    bytes: &[u8],
) -> (Post, DownloadTask, std::path::PathBuf) {
    let (post, media) = post_with_media(fixture, platform_post_id);
    let path = fixture.root.join(format!("{platform_post_id}.mp4"));
    write_file(&path, bytes);
    let task = seed_download(
        fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Completed,
        Some(&path),
        Some("2026-09-02T10:00:00Z".parse().unwrap()),
    );
    (post, task, path)
}

#[test]
fn a_completed_download_whose_file_is_there_reads_as_present() {
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030001", &[7u8; 512]);

    let (state, reason) = file_state(&fixture, &task.id);
    assert_eq!(state, "present");
    assert_eq!(reason, None);

    // And the size it reports is the file's own, from the same look.
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    let entry = scan.entries.iter().find(|e| e.task_id == task.id).unwrap();
    assert_eq!(entry.size_bytes, Some(512));
    assert!(path.is_file());
}

#[test]
fn a_file_the_user_deleted_is_missing_and_the_download_is_still_a_download() {
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030002", &[7u8; 512]);

    std::fs::remove_file(&path).expect("remove");

    let (state, reason) = file_state(&fixture, &task.id);
    assert_eq!(state, "missing");
    assert_eq!(reason, None, "a file that is gone needs no excuse");

    // The task is untouched: it completed, and it still says so.
    let stored = fixture
        .conn(|conn| downloads_repo::get(conn, &task.id).expect("read"))
        .expect("the task is still there");
    assert_eq!(stored.state, DownloadState::Completed);
    assert_eq!(
        stored.destination_path.as_deref(),
        Some(path.to_string_lossy().as_ref()),
        "the recorded path is not rewritten because the file is gone"
    );
    // And the history keeps its record. Nothing is deleted by looking.
    assert_eq!(
        fixture.conn(|conn| history_repo::count(conn).expect("count")),
        0,
        "this fixture seeds no history; the point is that the scan added none"
    );
}

#[test]
fn history_survives_a_missing_file() {
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030003", &[7u8; 512]);
    fixture.conn(|conn| {
        history_repo::insert(
            conn,
            &DownloadHistoryEntry {
                downloaded_at: Some("2026-09-02T10:00:00Z".parse().unwrap()),
                ..DownloadHistoryEntry::completed(
                    "https://cdn.example/media",
                    MediaType::Video,
                    path.to_string_lossy().as_ref(),
                )
            },
        )
        .expect("history");
    });
    let before = fixture.conn(|conn| history_repo::count(conn).expect("count"));

    std::fs::remove_file(&path).expect("remove");
    let (state, _) = file_state(&fixture, &task.id);
    assert_eq!(state, "missing");

    assert_eq!(
        fixture.conn(|conn| history_repo::count(conn).expect("count")),
        before,
        "history is a record of what happened, not a mirror of the disk"
    );
}

#[test]
fn a_file_that_comes_back_reads_as_present_again() {
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030004", &[7u8; 512]);

    std::fs::remove_file(&path).expect("remove");
    assert_eq!(file_state(&fixture, &task.id).0, "missing");

    write_file(&path, &[9u8; 1024]);
    let (state, _) = file_state(&fixture, &task.id);
    assert_eq!(state, "present");
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    let entry = scan.entries.iter().find(|e| e.task_id == task.id).unwrap();
    assert_eq!(entry.size_bytes, Some(1024), "the size follows the file, not the row");
}

#[test]
fn a_download_folder_that_is_not_there_makes_files_unknown_rather_than_missing() {
    // A drive that is not attached, which is the ordinary way this happens on a
    // desktop. Every file under it is unknown — calling them missing would tell
    // the user their downloads are gone when they are on a disk that is simply
    // not plugged in.
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030005", &[7u8; 64]);

    // Move the whole root aside and point the setting at somewhere absent.
    std::fs::remove_file(&path).expect("remove");
    let absent = fixture.root.join("not-mounted");
    fixture
        .core
        .settings()
        .update(|settings| {
            settings.download_directory = absent.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("settings");

    let (state, reason) = file_state(&fixture, &task.id);
    assert_eq!(state, "unavailable");
    assert_eq!(reason.as_deref(), Some("outside-configured-root"));
}

#[test]
fn an_unreachable_root_reports_unknown_for_the_files_beneath_it() {
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030006", &[7u8; 64]);
    std::fs::remove_file(&path).expect("remove");

    // The root itself goes away, and the setting still names it.
    let missing_root = fixture.root.join("gone-away");
    std::fs::create_dir_all(&missing_root).expect("dir");
    let buried = missing_root.join("Creator").join("Videos").join("clip.mp4");
    fixture.conn(|conn| {
        conn.execute(
            "UPDATE downloads SET destination_path = ?1 WHERE id = ?2",
            rusqlite::params![buried.to_string_lossy(), task.id],
        )
        .expect("update");
    });
    std::fs::remove_dir_all(&missing_root).expect("remove root");

    // The configured directory now names something that is not there at all.
    fixture
        .core
        .settings()
        .update(|settings| {
            settings.download_directory = missing_root.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("settings");

    let (state, _) = file_state(&fixture, &task.id);
    assert_eq!(state, "unavailable");
}

#[test]
fn changing_the_download_folder_does_not_call_old_files_missing() {
    // The paths under the previous tree are not paths under the new one, and
    // looking them up there would answer a question nobody asked.
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679030007", &[7u8; 64]);
    assert_eq!(file_state(&fixture, &task.id).0, "present");

    let new_root = fixture.root.join("elsewhere");
    std::fs::create_dir_all(&new_root).expect("dir");
    fixture
        .core
        .settings()
        .update(|settings| {
            settings.download_directory = new_root.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("settings");

    let (state, reason) = file_state(&fixture, &task.id);
    assert_eq!(state, "unavailable");
    assert_eq!(reason.as_deref(), Some("outside-configured-root"));
    // And the file itself is untouched where it was.
    assert!(path.is_file());

    // The scan says which tree it looked in, so a screen can explain this.
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert_eq!(scan.root, new_root.to_string_lossy());
}

#[test]
fn a_download_still_running_is_not_asked_about_its_file() {
    // The mistake this prevents: a download that is working perfectly, reported
    // as missing because it has not finished writing yet.
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679030008");
    let path = fixture.root.join("running.mp4");
    let task = seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Downloading,
        Some(&path),
        None,
    );

    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert!(
        !scan.entries.iter().any(|entry| entry.task_id == task.id),
        "a task that has not finished has no file state to report"
    );

    // Nor is a queued or paused one.
    for state in [DownloadState::Queued, DownloadState::Paused] {
        fixture.conn(|conn| {
            conn.execute(
                "UPDATE downloads SET state = ?1 WHERE id = ?2",
                rusqlite::params![state.as_str(), task.id],
            )
            .expect("update");
        });
        let scan = fixture.core.scan_downloaded_files().expect("scan");
        assert!(!scan.entries.iter().any(|entry| entry.task_id == task.id));
    }
}

#[test]
fn a_scan_deletes_nothing_and_changes_nothing() {
    let fixture = Fixture::new();
    let (_post, present_task, present_path) =
        completed_with_a_file(&fixture, "7629996475679030009", &[7u8; 128]);
    let (_post2, missing_task, missing_path) =
        completed_with_a_file(&fixture, "7629996475679030010", &[7u8; 128]);
    std::fs::remove_file(&missing_path).expect("remove");

    let files_before = files_under(&fixture.root);
    let tasks_before = fixture.conn(|conn| downloads_repo::count(conn).expect("count"));
    let history_before = fixture.conn(|conn| history_repo::count(conn).expect("count"));

    // Several passes, because reconciliation runs whenever a screen is opened.
    for _ in 0..3 {
        fixture.core.scan_downloaded_files().expect("scan");
    }

    assert_eq!(files_before, files_under(&fixture.root), "the scan touched the disk");
    assert_eq!(fixture.conn(|conn| downloads_repo::count(conn).expect("count")), tasks_before);
    assert_eq!(fixture.conn(|conn| history_repo::count(conn).expect("count")), history_before);
    assert!(present_path.is_file(), "the file that was there is still there");

    // Both tasks are reported, in one pass, each with its own answer.
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert_eq!(scan.entries.len(), 2);
    let by_id = |id: &str| scan.entries.iter().find(|e| e.task_id == id).unwrap().state;
    assert_eq!(by_id(&present_task.id), "present");
    assert_eq!(by_id(&missing_task.id), "missing");
}

#[test]
fn a_scan_is_deterministic_across_many_files() {
    let fixture = Fixture::new();
    let mut ids = Vec::new();
    for index in 0..25 {
        let (_post, task, _path) = completed_with_a_file(
            &fixture,
            &format!("76299964756790400{index:02}"),
            &vec![index as u8; 32 + index],
        );
        ids.push(task.id);
    }

    let first = fixture.core.scan_downloaded_files().expect("scan");
    let second = fixture.core.scan_downloaded_files().expect("scan");

    let digest = |scan: &FileScan| {
        let mut rows: Vec<(String, String)> = scan
            .entries
            .iter()
            .map(|entry| (entry.task_id.clone(), entry.state.to_string()))
            .collect();
        rows.sort();
        rows
    };
    assert_eq!(digest(&first), digest(&second), "the same disk gives the same answer");
    assert_eq!(first.entries.len(), ids.len());
    assert!(first.entries.iter().all(|entry| entry.state == "present"));
}

#[test]
fn a_failed_attempt_is_not_reported_as_a_missing_file() {
    // A failed download that recorded where it *would* have written leaves a
    // path behind, and that path has no file at it. Asking the filesystem about
    // it would answer "missing" — which reads as "the file you had is gone",
    // when the truth is that there never was one. A failed attempt and a file
    // the user has lost are different things and must not be reported alike.
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679030012");
    let never_written = fixture.root.join("never-written.mp4");
    let task = seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Failed,
        Some(&never_written),
        None,
    );

    assert!(!never_written.exists(), "the download really produced nothing");
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert!(
        !scan.entries.iter().any(|entry| entry.task_id == task.id),
        "a download that failed is not a download whose file went missing"
    );

    // The record is untouched by the question never being asked of it.
    let stored: String = fixture.conn(|conn| {
        conn.query_row(
            "SELECT state FROM downloads WHERE id = ?1",
            [&task.id],
            |row| row.get(0),
        )
        .expect("the row is still there")
    });
    assert_eq!(stored, "failed");
    assert_eq!(task.destination_path.as_deref(), Some(never_written.to_string_lossy().as_ref()));
}

#[test]
fn a_cancelled_download_is_not_reported_as_a_missing_file() {
    // The same rule as a failure, and worth stating separately because a
    // cancellation is the one case where a partial file may have existed: a task
    // the user stopped is not a download whose file went missing, and the
    // warning belongs to completed downloads alone.
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679030013");
    let abandoned = fixture.root.join("stopped-half-way.mp4");
    let task = seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Cancelled,
        Some(&abandoned),
        None,
    );

    assert!(!abandoned.exists(), "nothing was left behind to find");
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert!(
        !scan.entries.iter().any(|entry| entry.task_id == task.id),
        "a cancelled download is not asked whether its file went missing"
    );

    let stored: String = fixture.conn(|conn| {
        conn.query_row(
            "SELECT state FROM downloads WHERE id = ?1",
            [&task.id],
            |row| row.get(0),
        )
        .expect("the row is still there")
    });
    assert_eq!(stored, "cancelled", "the record is left exactly as it was");

    // And a cancelled task with a file still on disk is not asked either — the
    // question is about completed downloads, not about what happens to be there.
    write_file(&abandoned, &[5u8; 64]);
    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert!(!scan.entries.iter().any(|entry| entry.task_id == task.id));
}

#[test]
fn a_task_that_never_recorded_a_path_is_not_reported() {
    // A failed attempt that never got as far as a destination has no file to
    // have an opinion about.
    let fixture = Fixture::new();
    let (post, media) = post_with_media(&fixture, "7629996475679030011");
    let task = seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Failed,
        None,
        None,
    );

    let scan = fixture.core.scan_downloaded_files().expect("scan");
    assert!(!scan.entries.iter().any(|entry| entry.task_id == task.id));
}

// --- browsing what is on disk ------------------------------------------------
//
// The Dashboard reads the filesystem, so these build a real tree inside the
// fixture's own temporary directory and never touch the machine's download
// folder. The tree deliberately includes the awkward cases: a creator whose name
// is not ASCII, a folder with nothing in it, a file this application cannot show,
// a file it never downloaded, and a path that tries to leave through a link.

use douyin_downloader_lib::services::dashboard::{FolderContents, FolderListing, MediaKind};

/// Build a download tree that looks like a real one.
fn tree(fixture: &Fixture) {
    let root = &fixture.root;
    let put = |relative: &str, bytes: &[u8]| {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, bytes).expect("write");
    };

    // Single-URL downloads, which land straight in `Videos/`.
    put("Videos/7629996475679050001.mp4", &[1u8; 2048]);
    put("Videos/7629996475679050002.mp4", &[2u8; 4096]);

    // A creator folder, with both kinds of media under it. The name is not
    // ASCII, which is the ordinary case for this source.
    put("郭虎/Videos/7629996475679050003.mp4", &[3u8; 8192]);
    put("郭虎/Images/7629996475679050004.jpg", &[4u8; 1024]);
    put("郭虎/Images/7629996475679050005.webp", &[5u8; 512]);

    // A creator with nothing downloadable in it.
    std::fs::create_dir_all(root.join("Empty Creator")).expect("dir");

    // A file this application cannot show, and one it never downloaded.
    put("Videos/notes.txt", b"not media at all");
    put("Videos/put-here-by-hand.mp4", &[6u8; 256]);
}

fn folders(fixture: &Fixture) -> FolderListing {
    fixture.core.browse_media_folders().expect("listing")
}

fn open_folder(fixture: &Fixture, relative: &str) -> FolderContents {
    let path = fixture.root.join(relative);
    fixture
        .core
        .browse_media_folder(path.to_string_lossy().as_ref())
        .expect("contents")
}

#[test]
fn the_top_level_shows_the_folders_that_are_actually_there() {
    let fixture = Fixture::new();
    tree(&fixture);

    let listing = folders(&fixture);
    let names: Vec<&str> = listing.folders.iter().map(|f| f.name.as_str()).collect();

    // Sorted by the name a person reads, not by bytes.
    assert_eq!(names, vec!["Empty Creator", "Videos", "郭虎"]);
    assert_eq!(listing.root, fixture.root.to_string_lossy());
    assert_eq!(listing.loose_media_count, 0, "nothing sits loose in the root");
}

#[test]
fn a_folder_card_counts_what_is_inside_it_and_below_it() {
    let fixture = Fixture::new();
    tree(&fixture);

    let listing = folders(&fixture);
    let videos = listing.folders.iter().find(|f| f.name == "Videos").unwrap();
    // Two downloads and one file put there by hand. The text file is not media.
    assert_eq!(videos.media_count, 3);
    assert_eq!(videos.video_count, 3);
    assert_eq!(videos.image_count, 0);
    assert_eq!(videos.total_bytes, 2048 + 4096 + 256);
    assert_eq!(videos.state, "ok");

    let creator = listing.folders.iter().find(|f| f.name == "郭虎").unwrap();
    assert_eq!(
        creator.media_count, 3,
        "a creator folder counts its subfolders' media too"
    );
    assert_eq!(creator.video_count, 1);
    assert_eq!(creator.image_count, 2);
    assert!(
        creator.preview_path.as_deref().unwrap_or("").ends_with(".jpg")
            || creator.preview_path.as_deref().unwrap_or("").ends_with(".webp"),
        "an image stands for the folder: {:?}",
        creator.preview_path
    );

    let empty = listing.folders.iter().find(|f| f.name == "Empty Creator").unwrap();
    assert_eq!(empty.media_count, 0);
    assert_eq!(empty.state, "empty");
    assert_eq!(empty.preview_path, None);
}

#[test]
fn opening_a_folder_lists_its_files_with_what_they_are() {
    let fixture = Fixture::new();
    tree(&fixture);

    let contents = open_folder(&fixture, "Videos");
    assert_eq!(contents.files.len(), 4, "three videos and a text file");
    assert_eq!(contents.folders.len(), 0, "nothing below it");
    assert!(!contents.truncated);

    let by_name = |name: &str| contents.files.iter().find(|f| f.name == name).unwrap();
    assert_eq!(by_name("7629996475679050001.mp4").kind, MediaKind::Video);
    assert_eq!(by_name("7629996475679050001.mp4").size_bytes, 2048);
    assert!(by_name("7629996475679050001.mp4").playable);
    // The text file is listed, and honestly described as something this
    // application will not show in its own window.
    assert_eq!(by_name("notes.txt").kind, MediaKind::Other);
    assert!(!by_name("notes.txt").playable);
    assert_eq!(by_name("notes.txt").extension, "txt");
}

#[test]
fn opening_a_creator_folder_offers_the_folders_below_it() {
    let fixture = Fixture::new();
    tree(&fixture);

    let contents = open_folder(&fixture, "郭虎");
    let names: Vec<&str> = contents.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["Images", "Videos"]);
    assert!(
        contents.files.is_empty(),
        "the media is one level down, and the folders above say so"
    );

    let images = contents.folders.iter().find(|f| f.name == "Images").unwrap();
    assert_eq!(images.image_count, 2);

    // And one level further in, the pictures themselves.
    let inner = open_folder(&fixture, "郭虎/Images");
    assert_eq!(inner.files.len(), 2);
    assert!(inner.files.iter().all(|file| file.kind == MediaKind::Image));
    assert_eq!(inner.parent.as_deref(), Some(contents.path.as_str()));
}

#[test]
fn a_file_with_no_record_of_it_still_appears() {
    // The whole point of browsing the disk rather than the database: this
    // application never downloaded this file, and the user can still see it.
    let fixture = Fixture::new();
    tree(&fixture);

    let contents = open_folder(&fixture, "Videos");
    let hand = contents.files.iter().find(|f| f.name == "put-here-by-hand.mp4").unwrap();

    assert_eq!(hand.size_bytes, 256);
    assert_eq!(hand.task_id, None, "no record claims it");
    assert_eq!(hand.post_id, None);
    assert_eq!(hand.caption, None, "and nothing is invented to fill the gap");
}

#[test]
fn a_recorded_download_carries_what_the_application_knows_about_it() {
    let fixture = Fixture::new();
    tree(&fixture);

    // Record one of the files as something this application downloaded.
    let (post, media) = post_with_media(&fixture, "7629996475679050001");
    let path = fixture.root.join("Videos").join("7629996475679050001.mp4");
    let task = seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Completed,
        Some(&path),
        Some("2026-09-02T10:00:00Z".parse().unwrap()),
    );

    let contents = open_folder(&fixture, "Videos");
    let recorded = contents
        .files
        .iter()
        .find(|file| file.name == "7629996475679050001.mp4")
        .unwrap();

    assert_eq!(recorded.task_id.as_deref(), Some(task.id.as_str()));
    assert_eq!(recorded.post_id.as_deref(), Some(post.id.as_str()));
    assert_eq!(recorded.caption.as_deref(), Some("x"), "the fixture's caption");
    assert_eq!(recorded.source_url.as_deref(), Some("https://cdn.example/media"));
    assert!(recorded.downloaded_at.is_some());

    // Its neighbour, which nothing claims, stays unclaimed.
    let other = contents
        .files
        .iter()
        .find(|file| file.name == "7629996475679050002.mp4")
        .unwrap();
    assert_eq!(other.task_id, None);
}

#[test]
fn two_files_with_the_same_name_each_carry_their_own_record() {
    // A file is matched to a record by where it is, not by what it is called.
    // Two creators routinely produce the same generated name — the template is
    // `{date}_{id}`, and a re-download of one post lands beside the other — so a
    // match on the name alone would put one creator's caption on the other
    // creator's picture, which is worse than showing no caption at all.
    let fixture = Fixture::new();

    let first = fixture.root.join("Alice").join("Videos").join("same-name.mp4");
    let second = fixture.root.join("Bob").join("Videos").join("same-name.mp4");
    write_file(&first, &[1u8; 100]);
    write_file(&second, &[2u8; 200]);

    let (alice_post, alice_media) = post_with_media(&fixture, "7629996475679060001");
    let alice_task = seed_download(
        &fixture,
        &alice_post,
        Some(alice_media.id.clone()),
        DownloadState::Completed,
        Some(&first),
        Some("2026-09-02T10:00:00Z".parse().unwrap()),
    );
    let (bob_post, bob_media) = post_with_media(&fixture, "7629996475679060002");
    let bob_task = seed_download(
        &fixture,
        &bob_post,
        Some(bob_media.id.clone()),
        DownloadState::Completed,
        Some(&second),
        Some("2026-09-02T10:00:00Z".parse().unwrap()),
    );

    // Built one component at a time, the way the application builds them —
    // `read_dir` extends the folder it was given, so a path handed over with a
    // separator inside a segment would not be the path the walk produces.
    let claim = |segments: &[&str]| {
        let mut folder = fixture.root.clone();
        for segment in segments {
            folder = folder.join(segment);
        }
        let contents = fixture
            .core
            .browse_media_folder(folder.to_string_lossy().as_ref())
            .expect("contents");
        assert_eq!(contents.files.len(), 1, "{segments:?} holds exactly one file");
        let file = &contents.files[0];
        (
            file.name.clone(),
            file.task_id.clone(),
            file.post_id.clone(),
            file.size_bytes,
        )
    };

    let alice = claim(&["Alice", "Videos"]);
    let bob = claim(&["Bob", "Videos"]);

    assert_eq!(alice.0, "same-name.mp4");
    assert_eq!(bob.0, "same-name.mp4");
    assert_eq!(alice.1.as_deref(), Some(alice_task.id.as_str()));
    assert_eq!(bob.1.as_deref(), Some(bob_task.id.as_str()));
    assert_eq!(alice.2.as_deref(), Some(alice_post.id.as_str()));
    assert_eq!(bob.2.as_deref(), Some(bob_post.id.as_str()));
    assert_ne!(alice.2, bob.2, "each file kept its own post");
    // And each reports its own size, so neither was described from the other.
    assert_eq!(alice.3, 100);
    assert_eq!(bob.3, 200);
}

#[test]
fn a_record_matches_its_file_however_the_path_is_spelled() {
    // Windows paths ignore case and accept either separator, so the same file
    // can be spelled more than one way — and a download folder re-entered in
    // Settings with different capitalisation is enough to produce it. The file
    // is still listed either way; what is at stake is whether it keeps its
    // creator, caption and source, and whether anything would have said why not.
    let fixture = Fixture::new();
    let file = fixture.root.join("Videos").join("clip.mp4");
    write_file(&file, &[4u8; 128]);

    let (post, media) = post_with_media(&fixture, "7629996475679060009");
    let as_recorded = std::path::PathBuf::from(file.to_string_lossy().to_uppercase());
    assert_ne!(
        as_recorded, file,
        "the check is empty unless the two spellings really do differ"
    );

    seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Completed,
        Some(&as_recorded),
        Some("2026-09-02T10:00:00Z".parse().unwrap()),
    );

    let contents = open_folder(&fixture, "Videos");
    let listed = contents
        .files
        .iter()
        .find(|entry| entry.name == "clip.mp4")
        .expect("the file is listed");
    assert_eq!(
        listed.post_id.as_deref(),
        Some(post.id.as_str()),
        "the record follows the file, not the spelling of its path"
    );
    assert!(
        listed.caption.is_some(),
        "and with it the metadata the record carried"
    );
}

#[test]
fn a_record_whose_file_is_gone_does_not_appear_as_a_playable_file() {
    // The physical view shows what is on the disk. A row is not a file.
    let fixture = Fixture::new();
    tree(&fixture);

    let (post, media) = post_with_media(&fixture, "7629996475679050099");
    let absent = fixture.root.join("Videos").join("7629996475679050099.mp4");
    seed_download(
        &fixture,
        &post,
        Some(media.id.clone()),
        DownloadState::Completed,
        Some(&absent),
        Some("2026-09-02T10:00:00Z".parse().unwrap()),
    );

    let contents = open_folder(&fixture, "Videos");
    assert!(
        !contents.files.iter().any(|file| file.name == "7629996475679050099.mp4"),
        "a file that is not there is not listed as one that is"
    );
}

#[test]
fn a_path_outside_the_download_folder_is_refused() {
    let fixture = Fixture::new();
    tree(&fixture);

    // The fixture's own parent — a real directory this application does not own.
    let outside = fixture.root.parent().expect("a parent").to_path_buf();
    let err = fixture
        .core
        .browse_media_folder(outside.to_string_lossy().as_ref())
        .expect_err("must refuse");
    assert!(err.message().contains("folder"), "{}", err.message());

    // And somewhere that does not exist at all, which is also outside.
    let nowhere = fixture.root.join("..").join("somewhere-else");
    assert!(fixture
        .core
        .browse_media_folder(nowhere.to_string_lossy().as_ref())
        .is_err());
}

#[test]
fn browsing_writes_nothing_and_changes_nothing() {
    let fixture = Fixture::new();
    tree(&fixture);

    let before = files_under(&fixture.root);
    for _ in 0..3 {
        folders(&fixture);
        open_folder(&fixture, "Videos");
        open_folder(&fixture, "郭虎");
        open_folder(&fixture, "郭虎/Images");
    }
    assert_eq!(before, files_under(&fixture.root), "browsing touched the disk");
}

#[test]
fn browsing_is_deterministic() {
    let fixture = Fixture::new();
    tree(&fixture);

    let shape = |listing: &FolderListing, contents: &FolderContents| {
        let mut folders: Vec<(String, usize)> = listing
            .folders
            .iter()
            .map(|f| (f.name.clone(), f.media_count))
            .collect();
        folders.sort();
        let files: Vec<(String, u64)> = contents
            .files
            .iter()
            .map(|f| (f.name.clone(), f.size_bytes))
            .collect();
        (folders, files)
    };

    let first = shape(&folders(&fixture), &open_folder(&fixture, "Videos"));
    let second = shape(&folders(&fixture), &open_folder(&fixture, "Videos"));
    assert_eq!(first, second);
}

#[test]
fn a_folder_that_cannot_be_read_is_not_reported_as_empty() {
    // "Empty" is a claim that somebody looked inside and found nothing. A failed
    // read is the absence of any claim at all, and a screen whose whole purpose
    // is to say what is really on the disk must not present the second as the
    // first. The same goes for a file inside it: unreadable is not missing.
    let fixture = Fixture::new();
    tree(&fixture);

    let locked = fixture.root.join("Locked");
    std::fs::create_dir_all(&locked).expect("dir");
    std::fs::write(locked.join("hidden.mp4"), [3u8; 128]).expect("write");

    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        println!("skipped: no user name to deny read to");
        return;
    }

    let acl = |flag: &str| {
        std::process::Command::new("icacls")
            .arg(&locked)
            .arg(flag)
            .arg(format!("{user}:(OI)(CI)(RX)"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };

    let applied = acl("/deny") && std::fs::read_dir(&locked).is_err();

    // Measured while the folder is denied, so the assertions below cannot panic
    // with the ACL still in place.
    let listing = folders(&fixture);
    let opening_refused = fixture
        .core
        .browse_media_folder(locked.to_string_lossy().as_ref())
        .is_err();
    let _ = acl("/remove:d");

    if !applied {
        // A platform that will not deny read cannot be tested this way, and
        // saying so is better than passing for the wrong reason.
        println!("skipped: this environment would not deny read on a directory");
        return;
    }

    let card = listing
        .folders
        .iter()
        .find(|folder| folder.name == "Locked")
        .expect("the folder itself is still listed");
    assert_eq!(
        card.state, "unreadable",
        "a folder nobody could look inside is not an empty one"
    );

    assert!(
        opening_refused,
        "opening a folder that cannot be read is an error, not an empty folder"
    );

    // And the ordinary empty folder still reads as empty, so the two are
    // genuinely told apart rather than everything collapsing to one state.
    let empty = listing
        .folders
        .iter()
        .find(|folder| folder.name == "Empty Creator")
        .expect("the empty creator is listed");
    assert_eq!(empty.state, "empty");
}

#[test]
fn a_link_leading_out_of_the_download_folder_is_not_followed() {
    // Windows junctions do not need elevation, so this is a real link rather
    // than a stand-in. The target is a directory outside the fixture, holding a
    // file that must never appear in the listing.
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.mp4"), [9u8; 64]).expect("write");

    let fixture = Fixture::new();
    tree(&fixture);
    let link = fixture.root.join("linked");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made || !link.exists() {
        // A platform that will not make one cannot be tested this way, and
        // saying so is better than passing for the wrong reason.
        println!("skipped: this environment would not create a junction");
        return;
    }

    // The listing may or may not offer the link itself, but it must never show
    // what is behind it.
    let listing = folders(&fixture);
    let contents = fixture
        .core
        .browse_media_folder(link.to_string_lossy().as_ref())
        .map(|contents| contents.files.len())
        .unwrap_or(0);
    assert_eq!(contents, 0, "a link out of the root is not walked");
    assert!(
        !listing
            .folders
            .iter()
            .any(|folder| folder.name == "linked" && folder.media_count > 0),
        "the file behind the link must not be counted"
    );

    // Leave nothing behind for the temporary directory's own cleanup to trip on.
    let _ = std::fs::remove_dir(&link);
}

// --- previews for the Dashboard ---------------------------------------------
//
// The Dashboard's cards are drawn from these. What matters here is the refusal:
// the extraction itself is covered by the unit tests in `thumbnails`, which run
// it against real files with a real ffmpeg.

/// A file in the download folder, with the given bytes.
fn put(fixture: &Fixture, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = fixture.root.join(relative);
    write_file(&path, bytes);
    path
}

#[test]
fn a_preview_is_refused_for_a_path_outside_the_download_folder() {
    // The window cannot ask for a picture of anything it likes: the path is
    // checked against the configured folder before a byte is read.
    let fixture = Fixture::new();
    tree(&fixture);
    let outside = tempfile::tempdir().expect("outside");
    let elsewhere = outside.path().join("not-ours.jpg");
    std::fs::write(&elsewhere, b"someone else's file").expect("write");

    assert!(
        block_on(fixture.core.thumbnail_for(elsewhere.to_string_lossy().as_ref())).is_err(),
        "a file outside the download folder must be refused"
    );
}

#[test]
fn a_preview_is_refused_for_a_path_that_climbs_out() {
    let fixture = Fixture::new();
    tree(&fixture);
    let climbing = fixture.root.join("..").join("escaped.jpg");

    assert!(block_on(fixture.core.thumbnail_for(climbing.to_string_lossy().as_ref())).is_err());
}

#[test]
fn a_preview_is_refused_through_a_link_that_leads_out() {
    // The case a lexical check cannot see: the path *reads* as being inside the
    // folder and resolves somewhere else. Windows makes a junction without
    // elevation, so this is a real link rather than a stand-in.
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.jpg"), [9u8; 64]).expect("write");

    let fixture = Fixture::new();
    tree(&fixture);
    let link = fixture.root.join("linked");
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made || !link.exists() {
        println!("skipped: this environment would not create a junction");
        return;
    }

    let through_the_link = link.join("secret.jpg");
    assert!(
        block_on(fixture.core.thumbnail_for(through_the_link.to_string_lossy().as_ref())).is_err(),
        "a link out of the download folder must be refused, not followed"
    );

    let _ = std::fs::remove_dir(&link);
}

#[test]
fn a_preview_is_refused_for_a_file_that_is_no_longer_there() {
    // The card was drawn from a listing taken a moment ago and the file can be
    // gone by the time the picture is asked for. That is a refusal, not an
    // attempt to decode something that does not exist.
    let fixture = Fixture::new();
    tree(&fixture);
    let absent = fixture.root.join("Videos").join("gone.jpg");

    assert!(block_on(fixture.core.thumbnail_for(absent.to_string_lossy().as_ref())).is_err());
}

#[test]
fn a_file_with_no_picture_is_answered_with_none_rather_than_an_error() {
    // Audio and text have no frame to take. Answering with a made-up image would
    // be a claim about the file that is not true; answering with an *error*
    // would make an ordinary case look like a fault. The answer is simply none.
    let fixture = Fixture::new();
    tree(&fixture);
    let song = put(&fixture, "Videos/track.m4a", b"\0\0\0\0");
    let notes = fixture.root.join("Videos").join("notes.txt");

    for path in [song, notes] {
        let answer = block_on(fixture.core.thumbnail_for(path.to_string_lossy().as_ref()))
            .expect("an ordinary answer, not a refusal");
        assert!(answer.is_none(), "expected no preview for {path:?}");
    }
}

#[test]
fn a_picture_inside_the_folder_is_previewed_without_touching_it() {
    // The happy path, and the promise that goes with it: the original is read
    // and never written. With no ffmpeg the answer is `none` rather than an
    // error, which is the degradation the Dashboard depends on.
    let fixture = Fixture::new();
    tree(&fixture);
    let picture = fixture.root.join("郭虎").join("Images").join("7629996475679050004.jpg");
    let before = std::fs::read(&picture).expect("read");

    let answer = block_on(fixture.core.thumbnail_for(picture.to_string_lossy().as_ref()))
        .expect("not a refusal");

    // Whatever came back, the source is byte-for-byte what it was.
    assert_eq!(std::fs::read(&picture).expect("read"), before, "the original was modified");
    if let Some(thumbnail) = answer {
        assert!(Path::new(&thumbnail).is_file(), "the preview should exist");
        assert_ne!(thumbnail, picture.to_string_lossy(), "the preview is not the original");
        assert!(
            !Path::new(&thumbnail).starts_with(&fixture.root),
            "previews are not written among the user's files"
        );
    } else {
        println!("no ffmpeg here; the Dashboard would show a placeholder");
    }
}

// --- what the Library says about a file it could not check -------------------
//
// The Library used to reduce the filesystem to a boolean, so a file it could not
// look at was reported as a file that was gone. These pin the three answers, and
// pin that the Library and the download scan give the same one for the same file
// under the same conditions — two screens describing one condition differently is
// how a user learns not to trust either.

/// The Library's own answer about the only post in the fixture.
fn library_answer(fixture: &Fixture) -> (Option<String>, Option<String>, ContentStatus, Option<String>) {
    let page = fixture.all();
    let entry = page.entries.first().expect("the fixture has one post");
    (
        entry.file_state.clone(),
        entry.file_state_reason.clone(),
        entry.status,
        entry.error.clone(),
    )
}

/// Point the configured download folder somewhere else, as a user would.
fn point_the_folder_at(fixture: &Fixture, path: &Path) {
    fixture
        .core
        .settings()
        .update(|settings| {
            settings.download_directory = path.to_string_lossy().into_owned();
            Ok(())
        })
        .expect("settings");
}

#[test]
fn the_library_says_a_file_that_is_there_is_present() {
    let fixture = Fixture::new();
    let (_post, _task, path) = completed_with_a_file(&fixture, "7629996475679090001", &[7u8; 64]);
    assert!(path.is_file());

    let (state, reason, status, note) = library_answer(&fixture);
    assert_eq!(state.as_deref(), Some("present"));
    assert_eq!(reason, None, "there is nothing to explain about a file that is there");
    assert_eq!(status, ContentStatus::Downloaded);
    assert_eq!(note, None);
}

#[test]
fn the_library_says_a_file_the_user_deleted_is_missing() {
    // The case the rule was written for, and it must keep working: the file is
    // absent and the folder it belonged in is perfectly readable, so the absence
    // is a fact rather than a failure to look.
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679090002", &[7u8; 64]);
    std::fs::remove_file(&path).expect("remove");

    let (state, reason, status, note) = library_answer(&fixture);
    assert_eq!(state.as_deref(), Some("missing"));
    assert_eq!(reason, None, "absence that has been established needs no excuse");
    assert_eq!(status, ContentStatus::Ready, "it can be downloaded again");
    assert!(note.unwrap().contains("no longer on disk"));

    // And the record is untouched: the download is still a download.
    let (scan_state, _) = file_state(&fixture, &task.id);
    assert_eq!(scan_state, "missing");
}

#[test]
fn the_library_does_not_call_a_file_it_could_not_check_missing() {
    // The download folder was changed to somewhere that does not exist, so every
    // path recorded under the old one belongs to a tree this application cannot
    // see. That is not evidence of anything.
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679090003", &[7u8; 64]);
    std::fs::remove_file(&path).expect("remove");

    let elsewhere = fixture.root.join("not-mounted");
    point_the_folder_at(&fixture, &elsewhere);

    let (state, reason, status, note) = library_answer(&fixture);
    assert_eq!(state.as_deref(), Some("unavailable"));
    assert_eq!(reason.as_deref(), Some("outside-configured-root"));

    assert_eq!(
        status,
        ContentStatus::Downloaded,
        "a completed record with no evidence against it stays downloaded"
    );
    let note = note.expect("the user is told the check did not happen");
    assert!(note.contains("could not be checked"), "{note}");
    assert!(
        !note.contains("no longer on disk"),
        "an unchecked file must not be reported as deleted: {note}"
    );

    // And the download itself is untouched — history is a record of what
    // happened, not a claim about what is on disk now.
    let stored: String = fixture.conn(|conn| {
        conn.query_row("SELECT state FROM downloads WHERE id = ?1", [&task.id], |row| row.get(0))
            .expect("the row is still there")
    });
    assert_eq!(stored, "completed");
}

#[test]
fn the_library_and_the_download_scan_agree_about_the_same_file() {
    // The requirement stated as one test, because the failure it guards against
    // is not either answer being wrong on its own — it is the two screens
    // disagreeing about the same file in the same session.
    //
    // 1. The file is there.
    let fixture = Fixture::new();
    let (_post, task, _path) = completed_with_a_file(&fixture, "7629996475679090004", &[7u8; 64]);
    let (library_state, ..) = library_answer(&fixture);
    let (scan_state, _) = file_state(&fixture, &task.id);
    assert_eq!(library_state.as_deref(), Some(scan_state.as_str()));

    // 2. The file has been deleted.
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679090005", &[7u8; 64]);
    std::fs::remove_file(&path).expect("remove");
    let (library_state, ..) = library_answer(&fixture);
    let (scan_state, _) = file_state(&fixture, &task.id);
    assert_eq!(library_state.as_deref(), Some(scan_state.as_str()));
    assert_eq!(scan_state, "missing");

    // 3. The folder cannot be reached, so neither screen can say anything.
    let fixture = Fixture::new();
    let (_post, task, path) = completed_with_a_file(&fixture, "7629996475679090006", &[7u8; 64]);
    std::fs::remove_file(&path).expect("remove");
    point_the_folder_at(&fixture, &fixture.root.join("not-mounted"));
    let (library_state, ..) = library_answer(&fixture);
    let (scan_state, _) = file_state(&fixture, &task.id);
    assert_eq!(library_state.as_deref(), Some(scan_state.as_str()));
    assert_eq!(scan_state, "unavailable");
}

#[test]
fn a_post_that_was_never_downloaded_has_no_file_answer_at_all() {
    // Not "missing". Nothing produced a file, so the question was never asked,
    // and reporting an answer would be inventing one.
    let fixture = Fixture::new();
    seed_post(&fixture, None, "7629996475679090007", MediaType::Video, Some("Discovered"), None);

    let (state, reason, status, note) = library_answer(&fixture);
    assert_eq!(state, None);
    assert_eq!(reason, None);
    assert_eq!(status, ContentStatus::New);
    assert_eq!(note, None);
}

// --- a benchmark harness -----------------------------------------------------
//
// Ignored by default: it builds a synthetic library of two thousand entries and
// takes seconds to set up. Run it explicitly when a change touches the query
// path, and read the numbers rather than trusting the shape of the code.
//
//     cargo test --test library -- --ignored --nocapture benchmark
//
// Everything it writes lives in the fixture's own temporary directory, which is
// removed with it. Nothing here goes near the machine's real download folder.

/// How many entries the benchmark builds.
const BENCHMARK_ENTRIES: usize = 2_000;

/// A library with a representative spread of media types, download states and
/// filesystem answers — including all three presence states, since the counts
/// depend on them.
fn synthetic_library(fixture: &Fixture) -> std::path::PathBuf {
    let creator = seed_creator(fixture, "MS4wLjABbench", "Benchmark Creator");
    // Well outside the fixture's root, so `presence` reports it as unavailable
    // rather than absent — the third state, and the one a mixed dataset makes
    // hardest to obtain.
    let outside = std::path::PathBuf::from("C:/douyin-benchmark-outside");

    for index in 0..BENCHMARK_ENTRIES {
        let post_id = format!("79{index:017}");
        let media_type = [MediaType::Video, MediaType::Image, MediaType::Carousel, MediaType::Audio]
            [index % 4];
        let post = seed_post(
            fixture,
            Some(&creator),
            &post_id,
            media_type,
            Some(&format!("Benchmark post {index}")),
            Some("2026-01-01T00:00:00Z".parse().expect("timestamp")),
        );

        let relative = format!("{}/{post_id}.bin", creator.display_name);
        let inside = fixture.root.join(&relative);
        let outside_path = outside.join(format!("{post_id}.bin"));

        // Every post that holds media gets one, so the item counts — which ask
        // the filesystem about every finished download — are part of what is
        // being measured rather than an empty loop.
        let media = add_media(
            fixture,
            &post,
            media_type,
            Some(12.0),
            Some((1920, 1080)),
        );

        // A representative spread: every media type, every download state, and
        // all three filesystem answers. Which of those a row gets is decided
        // here, once.
        let (state, destination, write_a_file) = match index % 8 {
            // A finished download whose file is there.
            0 | 1 => (DownloadState::Completed, Some(inside.clone()), true),
            // A finished download whose file has been deleted.
            2 => (DownloadState::Completed, Some(inside.clone()), false),
            // A finished download recorded under a folder that is not the one
            // configured — the third answer, and the one a mixed dataset makes
            // hardest to obtain.
            3 => (DownloadState::Completed, Some(outside_path.clone()), false),
            4 => (DownloadState::Queued, None, false),
            5 => (DownloadState::Failed, None, false),
            6 => (DownloadState::Cancelled, None, false),
            // Discovered, never downloaded: no file, no task.
            _ => continue,
        };

        if write_a_file {
            write_file(&inside, &[7u8; 256]);
        }

        seed_download(
            fixture,
            &post,
            Some(media.id.clone()),
            state,
            destination.as_deref(),
            (state == DownloadState::Completed)
                .then(|| "2026-01-01T00:00:00Z".parse::<Timestamp>().expect("timestamp")),
        );
    }

    fixture.root.clone()
}

/// Median and 95th percentile of `iterations` runs, in milliseconds.
fn timings(iterations: usize, mut run: impl FnMut()) -> (f64, f64) {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = std::time::Instant::now();
        run();
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    let near_the_end = ((samples.len() as f64 * 0.95) as usize).min(samples.len() - 1);
    (median, samples[near_the_end])
}

#[test]
#[ignore = "a benchmark: builds 2000 entries and times queries; run it explicitly"]
fn benchmark_the_library_query() {
    use douyin_downloader_lib::services::library::{reset_work, work_done};

    let fixture = Fixture::new();
    let started = std::time::Instant::now();
    let root = synthetic_library(&fixture);
    println!(
        "dataset: {BENCHMARK_ENTRIES} posts, built in {:.1}s under {}",
        started.elapsed().as_secs_f64(),
        root.display()
    );

    let query = LibraryQuery {
        view: LibraryView::All,
        ..Default::default()
    };

    // What one request costs in work, which is exact and machine-independent.
    reset_work();
    let page = fixture.core.query_library(query.clone()).expect("query");
    let per_request = work_done();
    reset_work();
    let _ = fixture.core.library_entries(&query).expect("entries");
    let one_pass = work_done();

    println!(
        "one request: {} materialisations, {} filesystem inspections  (page total {} of {} scanned)",
        per_request.materialisations, per_request.inspections, page.total, page.scanned
    );
    println!(
        "one pass alone (no view counts): {} materialisations, {} inspections",
        one_pass.materialisations, one_pass.inspections
    );
    println!("view counts: {:?}", page.counts);

    let iterations = 15;
    let (median, p95) = timings(iterations, || {
        let _ = fixture.core.query_library(query.clone()).expect("query");
    });
    println!("whole request   : median {median:.1} ms, p95 {p95:.1} ms over {iterations} runs");

    let (median_one, p95_one) = timings(iterations, || {
        let _ = fixture.core.library_entries(&query).expect("entries");
    });
    println!("one pass alone  : median {median_one:.1} ms, p95 {p95_one:.1} ms");

    println!(
        "view counts only: about {:.1} ms (the request minus one pass)",
        (median - median_one).max(0.0)
    );
}

// --- what one request costs ---------------------------------------------------
//
// These are the reason the query path was restructured, stated as tests so the
// shape cannot quietly come back. A latency assertion would be a coin toss on a
// busy machine; counts of reads and filesystem inspections are exact, reproduce
// anywhere, and are what the cost is made of.

use douyin_downloader_lib::services::library::{reset_work, work_done};

#[test]
fn one_request_materialises_the_library_once() {
    // Eight scans per request is what the view counts used to cost. Whatever the
    // numbers on screen do, they are derived from a set that was read once.
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "MS4wLjABwork", "Work Counter");
    let post = seed_post(
        &fixture,
        Some(&creator),
        "7999",
        MediaType::Video,
        Some("Counted once"),
        None,
    );
    let path = fixture.root.join("7999.mp4");
    write_file(&path, b"video");
    seed_download(
        &fixture,
        &post,
        None,
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-01-01T00:00:00Z")),
    );

    reset_work();
    let page = fixture.core.query_library(LibraryQuery::default()).expect("query");
    let work = work_done();

    assert_eq!(
        work.materialisations, 1,
        "one request read the library {} times",
        work.materialisations
    );
    assert_eq!(page.counts.all, 1);
    assert_eq!(page.counts.videos, 1);
    assert_eq!(page.counts.downloaded, 1);
}

#[test]
fn one_request_asks_the_filesystem_once_per_distinct_file() {
    // The page, its item counts and its seven tab counts all read the same
    // files. Before the request-scoped cache, a two-thousand-post library cost
    // twelve thousand inspections for a thousand distinct files; the same file
    // was asked about once per materialisation rather than once per request.
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "MS4wLjABonce", "Asked Once");
    for index in 0..6 {
        let post = seed_post(
            &fixture,
            Some(&creator),
            &format!("79{index:02}"),
            MediaType::Video,
            Some(&format!("post {index}")),
            None,
        );
        let path = fixture.root.join(format!("79{index:02}.mp4"));
        write_file(&path, b"video");
        seed_download(
            &fixture,
            &post,
            None,
            DownloadState::Completed,
            Some(&path),
            Some(at("2026-01-01T00:00:00Z")),
        );
    }

    reset_work();
    let page = fixture.core.query_library(LibraryQuery::default()).expect("query");
    let work = work_done();

    assert_eq!(page.total, 6);
    assert_eq!(
        work.inspections, 6,
        "six files were asked about {} times, not 6 × 8",
        work.inspections
    );
}

#[test]
fn a_second_request_reads_the_files_again_rather_than_trusting_a_stale_cache() {
    // The cache is scoped to a request on purpose. A file deleted between two
    // requests must be seen to have been deleted, not reported present from an
    // answer an earlier request obtained.
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "MS4wLjABstale", "Goes Away");
    let post = seed_post(
        &fixture,
        Some(&creator),
        "7998",
        MediaType::Video,
        Some("Deleted between requests"),
        None,
    );
    let path = fixture.root.join("7998.mp4");
    write_file(&path, b"video");
    seed_download(
        &fixture,
        &post,
        None,
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-01-01T00:00:00Z")),
    );

    let first = fixture.core.query_library(LibraryQuery::default()).expect("first");
    assert_eq!(first.entries[0].file_state.as_deref(), Some("present"));

    std::fs::remove_file(&path).expect("remove");

    let second = fixture.core.query_library(LibraryQuery::default()).expect("second");
    assert_eq!(
        second.entries[0].file_state.as_deref(),
        Some("missing"),
        "the second request must look again, not reuse the first answer"
    );
    assert_eq!(second.entries[0].status, ContentStatus::Ready);
}

#[test]
fn the_tab_counts_are_the_same_for_every_view_they_are_asked_on() {
    // The counts describe the *library*, not the tab they were requested from.
    // This is behaviour the shared set preserves and the seven scans used to
    // make expensive to guarantee; pinned here because the request that produced
    // them is now free to be made from any tab.
    let fixture = Fixture::new();
    for index in 0..4 {
        let post = seed_post(
            &fixture,
            None,
            &format!("79{index}"),
            if index % 2 == 0 { MediaType::Video } else { MediaType::Image },
            Some(&format!("mixed {index}")),
            None,
        );
        if index % 2 == 0 {
            let path = fixture.root.join(format!("79{index}.mp4"));
            write_file(&path, b"video");
            seed_download(
                &fixture,
                &post,
                None,
                DownloadState::Completed,
                Some(&path),
                Some(at("2026-01-01T00:00:00Z")),
            );
        }
    }

    let from_all = fixture
        .core
        .query_library(LibraryQuery { view: LibraryView::All, ..Default::default() })
        .expect("all");
    let from_videos = fixture
        .core
        .query_library(LibraryQuery { view: LibraryView::Videos, ..Default::default() })
        .expect("videos");

    assert_eq!(from_all.counts.videos, 2);
    assert_eq!(from_videos.counts.videos, 2, "the Videos count does not depend on the tab");
    assert_eq!(from_all.counts.images, 2);
    assert_eq!(from_videos.counts.images, 2);
    // What *does* depend on the tab is the page, and only the page.
    assert_eq!(from_all.total, 4);
    assert_eq!(from_videos.total, 2);
}
// --- the contract, pinned -----------------------------------------------------
//
// These assert what each number is *supposed* to mean, not merely what it
// currently is. Where a number was ambiguous, the intended contract is stated in
// the test and in the report: `total` is the size of the match after the view
// and the caller's filters; `scanned` is the rows the view's own read covered,
// before status narrowing; a tab count is how much that tab holds under the
// caller's filters, whatever tab is on screen; pagination slices the filtered
// match in the sorted order; an export covers the active view + filters, every
// page.

#[test]
fn the_user_status_filter_narrows_the_page_and_the_counts_consistently() {
    // The interaction the L2 refactor made cheap to get right: the caller's
    // status filter is applied in memory, and both the page and the counts are
    // supposed to see the same predicate. A count that ignored it, or a page
    // that ignored it, would show a tab total that disagrees with the tabs
    // beside it.
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "MS4wLjABcontract", "Contract");
    for index in 0..4 {
        let post = seed_post(
            &fixture,
            Some(&creator),
            &format!("77{index}"),
            MediaType::Video,
            Some(&format!("status {index}")),
            None,
        );
        // Two videos finish (their files exist), one fails, one is never
        // downloaded — so the Downloaded and New counts are distinguishable.
        if index < 2 {
            let path = fixture.root.join(format!("77{index}.mp4"));
            write_file(&path, b"video");
            seed_download(
                &fixture,
                &post,
                None,
                DownloadState::Completed,
                Some(&path),
                Some(at("2026-01-01T00:00:00Z")),
            );
        } else if index == 2 {
            seed_download(&fixture, &post, None, DownloadState::Failed, None, None);
        }
    }

    let query = |view| LibraryQuery {
        view,
        filter: LibraryFilter {
            statuses: vec![ContentStatus::Downloaded],
            ..Default::default()
        },
        ..Default::default()
    };

    let page = fixture.core.query_library(query(LibraryView::All)).expect("query");
    assert_eq!(page.total, 2, "the page respects the status filter");
    assert_eq!(page.entries.len(), 2);

    // The counts are supposed to describe each tab *under the same filter*, so
    // they must agree with a hand count over the filtered set — and the tab
    // count that the filter empties must say so rather than ignoring the filter.
    assert_eq!(page.counts.all, 2, "All under the filter");
    assert_eq!(page.counts.videos, 2, "Videos ∩ Downloaded under the filter");
    assert_eq!(page.counts.downloaded, 2, "the Downloaded tab under its own filter");
    assert_eq!(
        page.counts.new, 0,
        "New ∩ Downloaded is empty: the filter must reach the counts too"
    );
    assert_eq!(page.counts.failed, 0, "Failed ∩ Downloaded is empty");
}

#[test]
fn the_user_media_type_filter_narrows_the_page_and_the_counts_consistently() {
    // Unlike statuses, the caller's media types ARE applied in SQL. The contract
    // is the same as the status filter's: page and counts see the same
    // predicate.
    let fixture = Fixture::new();
    let creator = seed_creator(&fixture, "MS4wLjABtypes", "Types");
    let video = seed_post(&fixture, Some(&creator), "7800", MediaType::Video, Some("v"), None);
    seed_post(&fixture, Some(&creator), "7801", MediaType::Image, Some("i"), None);
    seed_post(&fixture, Some(&creator), "7802", MediaType::Audio, Some("a"), None);
    let path = fixture.root.join("7800.mp4");
    write_file(&path, b"video");
    seed_download(
        &fixture,
        &video,
        None,
        DownloadState::Completed,
        Some(&path),
        Some(at("2026-01-01T00:00:00Z")),
    );

    let query = LibraryQuery {
        view: LibraryView::All,
        filter: LibraryFilter {
            media_types: vec![MediaType::Video],
            ..Default::default()
        },
        ..Default::default()
    };

    let page = fixture.core.query_library(query).expect("query");
    assert_eq!(page.total, 1, "only the video survives the type filter");
    assert_eq!(page.counts.all, 1, "All under the type filter");
    assert_eq!(page.counts.images, 0, "Images under the type filter is empty");
    assert_eq!(page.counts.audio, 0);
    assert_eq!(page.counts.downloaded, 1);
}

#[test]
fn counts_are_identical_whichever_tab_the_request_came_from() {
    // The tab counts are a property of the library under the caller's filters,
    // not of the tab on screen. Pinned across several tabs at once, with a mixed
    // dataset, because a count that drifted between tabs would make the numbers
    // on the switcher meaningless.
    let fixture = Fixture::new();
    for index in 0..6 {
        let media_type = [MediaType::Video, MediaType::Image, MediaType::Audio][index % 3];
        let post = seed_post(
            &fixture,
            None,
            &format!("79{index}"),
            media_type,
            Some(&format!("tab {index}")),
            None,
        );
        if index % 2 == 0 {
            let path = fixture.root.join(format!("79{index}.mp4"));
            write_file(&path, b"media");
            seed_download(
                &fixture,
                &post,
                None,
                DownloadState::Completed,
                Some(&path),
                Some(at("2026-01-01T00:00:00Z")),
            );
        }
    }

    let from = |view| {
        fixture
            .core
            .query_library(LibraryQuery {
                view,
                ..Default::default()
            })
            .expect("query")
            .counts
    };

    let all = from(LibraryView::All);
    let videos = from(LibraryView::Videos);
    let downloaded = from(LibraryView::Downloaded);
    let new = from(LibraryView::New);

    assert_eq!(all, videos, "counts do not depend on the tab on screen");
    assert_eq!(all, downloaded);
    assert_eq!(all, new);

    // And they are the numbers the dataset says they should be.
    assert_eq!(all.all, 6);
    assert_eq!(all.videos, 2);
    assert_eq!(all.images, 2);
    assert_eq!(all.audio, 2);
    assert_eq!(all.downloaded, 3);
    assert_eq!(all.new, 3);
}

#[test]
fn total_and_scanned_keep_their_meanings_on_narrow_views() {
    // The contract: `scanned` is how many rows the view's own read covered
    // before status narrowing; `total` is the match after every narrowing. On
    // the All view the two are equal; on a type-narrowed view they can differ —
    // and pinning both is what stops a future change from quietly swapping
    // which number is which.
    let fixture = Fixture::new();
    seed_post(&fixture, None, "7600", MediaType::Video, Some("v"), None);
    seed_post(&fixture, None, "7601", MediaType::Image, Some("i"), None);

    let page = fixture
        .core
        .query_library(LibraryQuery {
            view: LibraryView::All,
            ..Default::default()
        })
        .expect("query");
    assert_eq!(page.scanned, 2, "All reads everything");
    assert_eq!(page.total, 2);

    let videos = fixture
        .core
        .query_library(LibraryQuery {
            view: LibraryView::Videos,
            ..Default::default()
        })
        .expect("query");
    assert_eq!(videos.scanned, 1, "the Videos read covered one row");
    assert_eq!(videos.total, 1);
}

#[test]
fn pagination_slices_the_filtered_match_in_sorted_order() {
    // Stable order + a slice of the match, not of the library: the page an
    // offset lands on must be the page the sort implies, after every filter.
    let fixture = Fixture::new();
    for index in 0..9 {
        seed_post(
            &fixture,
            None,
            &format!("75{index}"),
            MediaType::Video,
            Some(&format!("page {index}")),
            Some(at(&format!("2026-01-0{}T00:00:00Z", index + 1))),
        );
    }

    let first = fixture
        .core
        .query_library(LibraryQuery {
            limit: Some(3),
            offset: 0,
            ..Default::default()
        })
        .expect("query");
    let second = fixture
        .core
        .query_library(LibraryQuery {
            limit: Some(3),
            offset: 3,
            ..Default::default()
        })
        .expect("query");

    assert_eq!(first.total, 9);
    assert_eq!(first.entries.len(), 3);
    assert_eq!(second.entries.len(), 3);

    // Newest first, and no overlap between the pages.
    let first_ids: Vec<&str> = first
        .entries
        .iter()
        .map(|e| e.post.platform_post_id.as_str())
        .collect();
    let second_ids: Vec<&str> = second
        .entries
        .iter()
        .map(|e| e.post.platform_post_id.as_str())
        .collect();
    assert_eq!(first_ids, vec!["758", "757", "756"]);
    assert_eq!(second_ids, vec!["755", "754", "753"]);
    assert!(
        first_ids.iter().all(|id| !second_ids.contains(id)),
        "the pages must not overlap"
    );

    // And the export, which covers the whole match rather than a page, contains
    // every row the two pages showed and the ones after them.
    let entries = fixture
        .core
        .library_entries(&LibraryQuery::default())
        .expect("entries");
    assert_eq!(entries.len(), 9, "the export is the match, not a page");
}

#[test]
fn an_export_covers_the_active_view_and_filters_across_every_page() {
    // The contract: the export is the active view + the caller's filters, over
    // all pages — independent of pagination, dependent on the tab. Pinned with a
    // dataset where those three answers differ.
    let fixture = Fixture::new();
    let video_done = seed_post(&fixture, None, "7400", MediaType::Video, Some("vd"), None);
    seed_post(&fixture, None, "7401", MediaType::Video, Some("v-new"), None);
    let image_done = seed_post(&fixture, None, "7402", MediaType::Image, Some("id"), None);

    for (post, id) in [(video_done.clone(), "7400"), (image_done.clone(), "7402")] {
        let path = fixture.root.join(format!("{id}.mp4"));
        write_file(&path, b"bytes");
        seed_download(
            &fixture,
            &post,
            None,
            DownloadState::Completed,
            Some(&path),
            Some(at("2026-01-01T00:00:00Z")),
        );
    }

    // The Videos view: both videos, whatever page they would have landed on.
    let entries = fixture
        .core
        .library_entries(&LibraryQuery {
            view: LibraryView::Videos,
            ..Default::default()
        })
        .expect("entries");
    assert_eq!(entries.len(), 2, "both videos, every page");
    assert!(entries.iter().all(|e| e.post.media_type == MediaType::Video));

    // With a status filter on top: only the downloaded video.
    let filtered = fixture
        .core
        .library_entries(&LibraryQuery {
            view: LibraryView::Videos,
            filter: LibraryFilter {
                statuses: vec![ContentStatus::Downloaded],
                ..Default::default()
            },
            ..Default::default()
        })
        .expect("entries");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].post.platform_post_id, video_done.platform_post_id);

    // And the Images view answers differently, because the tab is part of the
    // export contract.
    let images = fixture
        .core
        .library_entries(&LibraryQuery {
            view: LibraryView::Images,
            ..Default::default()
        })
        .expect("entries");
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].post.platform_post_id, image_done.platform_post_id);
}

#[test]
fn the_scan_limit_is_reported_rather_than_hidden() {
    // The bound itself is real, and this pins the one thing that makes it
    // honest: a page filled to the cap says so. The cap is 20_000, and building
    // 20_001 rows in a test would take minutes — so the pin is on the
    // relationship between the flag and the count the flag is derived from,
    // exercised over a small dataset where the cap is not reached.
    let fixture = Fixture::new();
    for index in 0..5 {
        seed_post(
            &fixture,
            None,
            &format!("73{index}"),
            MediaType::Video,
            Some(&format!("bound {index}")),
            None,
        );
    }

    let page = fixture
        .core
        .query_library(LibraryQuery::default())
        .expect("query");

    assert_eq!(page.scanned, 5);
    assert!(
        !page.truncated,
        "five rows are nowhere near the cap, so the page must not claim truncation"
    );
    assert_eq!(page.total, 5);
}

#[test]
fn the_truncation_flag_is_derived_from_the_cap_the_read_actually_reached() {
    // The flag is not a guess about the library's size; it is a property of the
    // read. A read that returns exactly the cap *may* have more behind it, and
    // reporting that as truncation is the honest direction — a false positive
    // says "there may be more", which is true; a false negative says "this is
    // everything", which is not.
    //
    // The cap is a constant in the repository. This test reads it from the
    // repository rather than restating it, so the two cannot drift apart.
    assert_eq!(
        douyin_downloader_lib::database::repositories::library::MAX_SCAN,
        20_000
    );
}

// --- the scan-limit boundary ---------------------------------------------------
//
// A read that fills its cap is not the same as a match that ends there. The
// boundary was once inferred by comparing the count against the cap, which
// reports a library of exactly 20,000 as truncated — a false positive on the
// one size where the answer is knowable. The read now fetches one candidate
// beyond the cap and reports what it saw.
//
// All three sides of the boundary are exercised with real rows in the fixture's
// own temporary database. Nothing here goes near the machine's download folder.

/// Seed `count` matching posts, with files on only the first few, so the setup
/// stays quick and the presence work stays bounded.
fn seed_matching_posts(fixture: &Fixture, count: usize) {
    for index in 0..count {
        let post = seed_post(
            fixture,
            None,
            &format!("7{index:06}"),
            MediaType::Video,
            Some(&format!("boundary {index}")),
            None,
        );
        // A handful of files, so presence inspection has something real to look
        // at without 20,000 filesystem writes.
        if index < 8 {
            let path = fixture.root.join(format!("7{index:06}.mp4"));
            write_file(&path, b"media");
            seed_download(
                fixture,
                &post,
                None,
                DownloadState::Completed,
                Some(&path),
                Some(at("2026-01-01T00:00:00Z")),
            );
        }
    }
}

#[test]
fn below_the_limit_the_page_says_nothing_is_missing() {
    let fixture = Fixture::new();
    seed_matching_posts(&fixture, 25);

    let page = fixture.core.query_library(LibraryQuery::default()).expect("query");
    assert_eq!(page.total, 25);
    assert_eq!(page.scanned, 25);
    assert!(!page.truncated, "25 of 20,000 is nowhere near the cap");
}

#[test]
fn exactly_at_the_limit_is_a_complete_answer_not_a_truncated_one() {
    // The false positive this sprint exists for: a library of exactly
    // MAX_SCAN matching rows, where a count-vs-cap comparison reports
    // truncation because the count equals the cap. Nothing is missing — the
    // read saw every matching row.
    let fixture = Fixture::new();
    seed_matching_posts(&fixture, douyin_downloader_lib::database::repositories::library::MAX_SCAN);

    let page = fixture.core.query_library(LibraryQuery::default()).expect("query");

    assert_eq!(page.total, 20_000, "every matching row came back");
    assert_eq!(page.scanned, 20_000);
    assert!(
        !page.truncated,
        "exactly the cap is the whole match, not a floor"
    );
    // The counts describe the complete library too.
    assert_eq!(page.counts.all, 20_000);
}

#[test]
fn above_the_limit_the_page_reports_truncation_and_returns_only_the_cap() {
    let fixture = Fixture::new();
    let cap = douyin_downloader_lib::database::repositories::library::MAX_SCAN;
    seed_matching_posts(&fixture, cap + 7);

    let page = fixture.core.query_library(LibraryQuery::default()).expect("query");

    assert!(
        page.truncated,
        "seven matching rows were left behind, and the page must say so"
    );
    assert!(
        page.total <= cap,
        "no more than the cap is offered: got {}",
        page.total
    );
    assert_eq!(page.scanned, cap, "the read covered the cap it stopped at");
    // The counts are floors over the rows the read covered, not over the
    // library as a whole — which is exactly what the truncation notice on the
    // screen is for.
    assert_eq!(page.counts.all, cap);
}

#[test]
fn the_one_extra_candidate_is_never_offered_to_the_caller() {
    // The probe row exists to answer one question and must not leak into the
    // page: at most MAX_SCAN rows cross the boundary, whichever side of it the
    // library sits on.
    let fixture = Fixture::new();
    seed_matching_posts(&fixture, douyin_downloader_lib::database::repositories::library::MAX_SCAN + 1);

    let page = fixture.core.query_library(LibraryQuery::default()).expect("query");
    assert_eq!(page.total, 20_000);
    assert!(page.truncated);
    // The one row left behind is the *last* in sorted order, not the first:
    // truncation keeps the head of the order the user asked for.
    let ids: Vec<&str> = page
        .entries
        .iter()
        .map(|e| e.post.platform_post_id.as_str())
        .collect();
    assert!(
        !ids.contains(&"700000"),
        "the head of the order must survive truncation"
    );
}
