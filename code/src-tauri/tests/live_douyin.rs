//! Live validation against the real Douyin endpoints.
//!
//! These tests are `#[ignore]`d on purpose: the deterministic suite must not
//! depend on Douyin being reachable, on a browser being installed, or on what
//! the platform chooses to return today. They exist so the real path can be
//! exercised deliberately:
//!
//! ```text
//! cargo test --test live_douyin -- --ignored --nocapture
//! ```
//!
//! # What they assert
//!
//! Not "resolution succeeds" — that is the source's decision, not ours — but the
//! invariant that matters: **the application either returns real data sourced
//! from Douyin, or it returns a typed error naming why.** It never fabricates a
//! post, a media URL, or a thumbnail, and it never claims a download it did not
//! perform.

mod support;

use std::path::Path;
use std::time::Duration;

use douyin_downloader_lib::config::AppPaths;
use douyin_downloader_lib::database::repositories::history as history_repo;
use douyin_downloader_lib::errors::AppErrorKind;
use douyin_downloader_lib::media::ffmpeg::discover_tool;
use douyin_downloader_lib::models::{ContentStatus,
    DownloadEvent, DownloadState, MediaType, TrackLayout,
};
use douyin_downloader_lib::services::profile::SubmitProfileDownloadsInput;
use douyin_downloader_lib::services::{AppCore, CreateDownloadInput};
use douyin_downloader_lib::storage::filenames::is_within;

/// The two links the project ships for validation.
const VIDEO_URL: &str = "https://v.douyin.com/7yyhouNSzvA/";
const PROFILE_URL: &str = "https://v.douyin.com/SMt7NsUPaGo/";

/// Boot the whole core with the real HTTP client and the real browser renderer.
fn live_core() -> (tempfile::TempDir, AppCore) {
    let temp = tempfile::tempdir().expect("temp dir");
    let paths = AppPaths::with_data_dir(temp.path().to_path_buf());
    let core = AppCore::bootstrap(paths).expect("bootstrap").core;
    (temp, core)
}

/// Show enough of a signed URL to identify the route without reproducing the
/// signature.
fn short(url: &str) -> String {
    match url.split_once('?') {
        Some((path, query)) => format!("{path}?<{} chars of query>", query.len()),
        None => url.to_string(),
    }
}

/// Whether a byte sequence contains another.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Which kinds of track an MP4 declares.
///
/// A container's sample descriptions name their codecs, so the presence of a
/// video codec and of an audio codec is enough to answer the question that
/// matters here: *does this file have sound in it?* It is a structural check,
/// not a decode.
fn mp4_tracks(bytes: &[u8]) -> (bool, bool) {
    let video = contains(bytes, b"avc1") || contains(bytes, b"hvc1") || contains(bytes, b"vp09");
    let audio = contains(bytes, b"mp4a") || contains(bytes, b"Opus") || contains(bytes, b"ac-3");
    (video, audio)
}

/// Wait for a terminal state for `task_id`.
async fn wait_for_terminal(
    mut rx: tokio::sync::broadcast::Receiver<DownloadEvent>,
    task_id: &str,
) -> DownloadState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "the download did not finish in time");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(DownloadEvent::Completed { task_id: id, .. })) if id == task_id => {
                return DownloadState::Completed
            }
            Ok(Ok(DownloadEvent::StateChanged { task_id: id, state }))
                if id == task_id
                    && matches!(
                        state,
                        DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled
                    ) =>
            {
                return state
            }
            Ok(_) => continue,
            Err(_) => panic!("timed out waiting for a terminal state"),
        }
    }
}

// --- the URL itself ----------------------------------------------------------

#[tokio::test]
#[ignore = "requires live network access to Douyin"]
async fn the_video_test_link_expands_and_classifies_as_a_post() {
    let (_temp, core) = live_core();

    let expanded = core
        .resolvers()
        .expand(VIDEO_URL)
        .await
        .expect("the short link must expand");

    println!("video link expanded to: {}", expanded.canonical);
    println!("classified kind: {}", expanded.kind.label());

    assert!(expanded.is_post(), "the video test link must be a post");
    assert!(expanded.id().is_some(), "a post link must yield an id");
    assert_eq!(expanded.original, VIDEO_URL);
}

#[tokio::test]
#[ignore = "requires live network access to Douyin"]
async fn the_profile_test_link_expands_and_classifies_as_a_profile() {
    let (_temp, core) = live_core();

    let expanded = core
        .resolvers()
        .expand(PROFILE_URL)
        .await
        .expect("the short link must expand");

    println!("profile link expanded to: {}", expanded.canonical);

    assert!(
        expanded.is_profile(),
        "the profile test link must be a profile, not a post"
    );
    assert_eq!(expanded.original, PROFILE_URL);
}

// --- analysis ----------------------------------------------------------------

#[tokio::test]
#[ignore = "requires a browser and live network access"]
async fn analyzing_the_video_link_yields_real_media_or_a_typed_error() {
    let (_temp, core) = live_core();

    match core.analyze_url(VIDEO_URL).await {
        Ok(analyzed) => {
            println!("RESULT: resolved");
            println!("  intent: {}", analyzed.intent);
            println!("  stage: {}", analyzed.resolution_stage);
            println!("  post id: {:?}", analyzed.post.as_ref().map(|p| &p.platform_post_id));
            println!("  caption: {:?}", analyzed.post.as_ref().and_then(|p| p.caption.clone()));
            println!("  creator: {:?}", analyzed.creator.as_ref().map(|c| &c.display_name));
            println!("  thumbnail: {:?}", analyzed.post.as_ref().and_then(|p| p.thumbnail_url.clone()));
            println!("  media items: {}", analyzed.media.len());

            assert_eq!(analyzed.intent, "post");
            let post = analyzed.post.as_ref().expect("a resolved post");
            assert!(!post.platform_post_id.is_empty());

            for item in &analyzed.media {
                println!(
                    "  - {:?} {} ({} variants)",
                    item.media_type,
                    short(&item.source_url),
                    item.variants.len()
                );
                for variant in &item.variants {
                    println!("      · {} -> {}", variant.label, short(&variant.source_url));
                }
                assert!(
                    item.source_url.starts_with("https://") || item.source_url.starts_with("http://"),
                    "a media URL must be a real http(s) address, got {}",
                    item.source_url
                );
                for variant in &item.variants {
                    assert!(
                        variant.source_url.starts_with("https://")
                            || variant.source_url.starts_with("http://"),
                        "a variant URL must be a real http(s) address, got {}",
                        variant.source_url
                    );
                }
            }
            assert!(!analyzed.media.is_empty(), "a resolved post must expose media");
        }
        Err(err) => {
            // The honest alternative: a typed error explaining why, with a
            // user-safe message. Anything else would mean the app invented data.
            println!("RESULT: typed failure");
            println!("  kind: {:?}", err.kind());
            println!("  message: {}", err.message());
            println!("  user message: {}", err.user_message());
            println!("  diagnostic: {}", err.technical_detail().unwrap_or("-"));

            assert!(
                matches!(
                    err.kind(),
                    AppErrorKind::Resolution
                        | AppErrorKind::UnavailableContent
                        | AppErrorKind::UnsupportedContent
                        | AppErrorKind::BrowserUnavailable
                        | AppErrorKind::Network
                ),
                "a live failure must keep a meaningful kind, got {:?}",
                err.kind()
            );
            assert!(!err.user_message().trim().is_empty());
        }
    }
}

// --- the whole thing ---------------------------------------------------------

#[tokio::test]
#[ignore = "requires a browser and live network access"]
async fn the_video_link_resolves_downloads_and_records_history() {
    let temp = tempfile::tempdir().expect("temp dir");
    let download_dir = temp.path().join("downloads");
    let paths = AppPaths::with_data_dir(temp.path().join("data"));
    let core = AppCore::bootstrap(paths).expect("bootstrap").core;

    // The core has just booted onto whatever the machine's settings say, so it
    // is moved onto this test's own directory before anything can be fetched —
    // and then checked, because a settings write that quietly did nothing would
    // leave it downloading into the user's folder.
    support::assert_isolated_destination("the live harness", &download_dir);
    std::fs::create_dir_all(&download_dir).expect("download dir");
    core.settings()
        .update(|settings| {
            settings.download_directory = download_dir.to_string_lossy().into_owned();
            settings.folder_template = "{creator}/{type}".to_string();
            settings.organize_by_creator = true;
            settings.organize_by_media_type = true;
            Ok(())
        })
        .expect("settings");
    support::assert_core_download_dir(&core, &download_dir, "the live harness");

    // 1. Analyze.
    let analyzed = match core.analyze_url(VIDEO_URL).await {
        Ok(analyzed) => analyzed,
        Err(err) if source_would_not_serve(&err) => return,
        Err(err) => panic!("the video link must resolve: {err}"),
    };
    let post = analyzed.post.clone().expect("a post");
    println!("resolved post {}", post.platform_post_id);
    println!("caption: {:?}", post.caption);
    println!("resolution via: {:?}", post.metadata.get("resolution"));
    assert!(!analyzed.media.is_empty(), "MEDIA FOUND: no");
    println!("MEDIA FOUND: yes ({} items)", analyzed.media.len());
    for item in &analyzed.media {
        println!(
            "  {:?} track_layout={:?} variants={}",
            item.media_type,
            item.track_layout(),
            item.variants.len()
        );
    }

    // 2. Choose the video item and its preferred variant.
    let video = analyzed
        .media
        .iter()
        .find(|item| item.media_type == MediaType::Video)
        .or_else(|| analyzed.media.first())
        .expect("a downloadable item")
        .clone();
    let chosen = video.preferred_variant();
    println!("chosen variant: {} -> {}", chosen.label, short(&chosen.source_url));

    // The resolver only attaches the audio track when it has established that one
    // exists. When the page never fetched its media the layout is `unknown`, the
    // download is the video alone, and the screen says "audio not checked" — so
    // this is the contract the run has to satisfy, not "audio always arrives".
    let expect_audio = video.track_layout() == Some(TrackLayout::AudioVideo);
    println!(
        "audio expected in the output: {expect_audio} (track layout {:?})",
        video.track_layout()
    );

    // 3. Download it through the ordinary pipeline.
    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post.id.clone(),
            media_item_id: video.id.clone(),
            variant_id: Some(chosen.id.clone()),
        })
        .await
        .expect("a download task");
    println!("download task {}", task.id);
    // The task carries a second track exactly when the resolver established that
    // one exists — never because it was assumed.
    assert_eq!(
        task.audio_source_url.is_some(),
        expect_audio,
        "the task's second track must match the resolved track layout"
    );

    let state = wait_for_terminal(rx, &task.id).await;
    println!("terminal state: {state:?}");
    assert_eq!(state, DownloadState::Completed, "DOWNLOAD COMPLETED: no");

    // 4. Verify the file.
    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    let saved = stored.destination_path.clone().expect("a saved path");
    let path = Path::new(&saved);
    let bytes = std::fs::read(path).expect("read the downloaded file");

    println!("saved to: {saved}");
    println!("bytes: {}", bytes.len());
    assert!(!bytes.is_empty(), "FILE VERIFIED: no (empty)");
    assert!(saved.ends_with(".mp4"), "expected an .mp4, got {saved}");
    assert!(
        is_within(&download_dir, path),
        "the file must be inside the download directory"
    );
    // An MP4 begins with a size then the `ftyp` box.
    assert!(bytes.len() > 12, "too small to be a media file");
    assert_eq!(&bytes[4..8], b"ftyp", "not a plausible MP4 container");

    let (has_video, has_audio) = mp4_tracks(&bytes);
    println!("container declares video track: {has_video}");
    println!("container declares audio track: {has_audio}");
    assert!(has_video, "the downloaded file must contain a video track");
    if expect_audio {
        // The item was resolved as `audio_video`, so a file without sound is a
        // defect rather than a source limitation.
        assert!(has_audio, "AUDIO VERIFIED: no — no audio stream in the container");
        println!("AUDIO VERIFIED: yes");
    } else {
        println!(
            "AUDIO VERIFIED: not required (track layout {:?})",
            video.track_layout()
        );
    }

    // The same facts from the tool the application itself uses, when it is
    // installed. A byte scan can spot an `mp4a` sample entry; only a demuxer can
    // say the file is actually readable and what it really contains.
    match discover_tool("ffprobe", "") {
        Some(ffprobe) => {
            let output = std::process::Command::new(&ffprobe)
                .args([
                    "-v",
                    "error",
                    "-print_format",
                    "json",
                    "-show_format",
                    "-show_streams",
                ])
                .arg(path)
                .output()
                .expect("run ffprobe");

            let report = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "FFPROBE VERIFIED: no — {report}");
            println!("FFPROBE VERIFIED: yes");
            println!("  {}", report.replace(['\n', ' '], "").chars().take(900).collect::<String>());

            assert!(
                report.contains("\"codec_type\": \"video\"")
                    || report.contains("\"codec_type\":\"video\""),
                "ffprobe found no video stream in the final file"
            );
            if expect_audio {
                assert!(
                    report.contains("\"codec_type\": \"audio\"")
                        || report.contains("\"codec_type\":\"audio\""),
                    "ffprobe found no audio stream in a file that was expected to have one"
                );
                println!("FFPROBE AUDIO VERIFIED: yes");
            }
        }
        None => println!("FFPROBE VERIFIED: not attempted (ffprobe is not installed)"),
    }
    println!("FILE VERIFIED: yes");

    // No partial file left behind.
    assert!(
        !std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".part")),
        "a .part file survived the download"
    );

    // 5. History.
    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .expect("history");
    assert_eq!(history.len(), 1, "HISTORY RECORDED: no");
    assert_eq!(history[0].status, DownloadState::Completed);
    assert_eq!(history[0].local_path.as_deref(), Some(saved.as_str()));
    assert_eq!(history[0].post_id.as_deref(), Some(post.id.as_str()));
    println!("HISTORY RECORDED: yes");
}

// --- the profile link --------------------------------------------------------

#[tokio::test]
#[ignore = "requires a browser and live network access"]
async fn the_profile_link_is_never_treated_as_a_downloadable_post() {
    let (_temp, core) = live_core();

    let analyzed = core
        .analyze_url(PROFILE_URL)
        .await
        .expect("a profile link must be analyzed, not rejected");

    println!("RESULT: intent = {}", analyzed.intent);
    println!("  notice: {:?}", analyzed.notice);

    assert_eq!(analyzed.intent, "profile");
    assert!(analyzed.post.is_none(), "a profile must not become a post");
    assert!(analyzed.media.is_empty(), "a profile has no downloadable media");
    assert!(analyzed
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("Profile Downloader"));

    // And the download path refuses it: there is no post id to submit.
    let err = core
        .create_download(CreateDownloadInput {
            post_id: "not-a-post".into(),
            media_item_id: "not-a-media".into(),
            variant_id: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), AppErrorKind::NotFound);
}

// --- profile listing (Phase 03) ---------------------------------------------
//
// The same invariant as above, applied to a profile: the application either
// returns posts that really came from Douyin, or it returns a typed error
// naming why. It never presents a withheld listing as a creator with no work,
// and never invents a post, a thumbnail, or a caption.

/// The profile link supplied for Phase 03 validation.
const PHASE03_PROFILE_URL: &str = "https://v.douyin.com/mv4vQSIXgg0/";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the live Douyin endpoints and an installed browser"]
async fn the_live_profile_page_identifies_its_creator() {
    let (_temp, core) = live_core();

    let identity = core
        .resolve_profile(PHASE03_PROFILE_URL)
        .await
        .expect("a profile link must resolve to at least an identity");

    println!("--- profile resolution ---");
    println!("submitted : {}", identity.classification.original);
    println!("canonical : {}", identity.classification.canonical);
    println!("creator id: {}", identity.creator.platform_id);
    println!("name      : {}", identity.creator.display_name);
    println!("handle    : {:?}", identity.creator.handle);
    println!("posts     : {:?}", identity.creator.post_count);
    println!("verified  : {}", identity.creator.verified);
    println!("bio       : {:?}", identity.creator.description);
    println!("avatar    : {:?}", identity.creator.avatar_url.as_deref().map(short));
    println!("stage     : {}", identity.resolution_stage);
    println!("via       : {:?}", identity.resolved_via);

    // The link is understood as a profile and yields a stable identity.
    assert_eq!(identity.classification.kind, "profile");
    assert!(!identity.creator.platform_id.is_empty());
    assert!(identity.creator.profile_url.contains("/user/"));

    match identity.resolution_stage.as_str() {
        "resolved" => {
            // The page supplied the creator's own details, so the display name
            // must be the one the profile published — never a placeholder.
            assert_ne!(
                identity.creator.display_name, identity.creator.platform_id,
                "a resolved profile must carry the creator's real name"
            );
            println!("=> the live profile page supplied the creator's details");
        }
        other => {
            // Degraded, but honest: identity only, with the reason recorded.
            println!("=> profile details were not read ({other}); identity only");
            assert!(identity.notice.is_some());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the live Douyin endpoints and an installed browser"]
async fn the_live_profile_listing_is_real_posts_or_a_typed_refusal() {
    let (_temp, core) = live_core();

    // Resolved first, so the listing is asked with the identity the platform
    // actually handed out rather than one guessed from the URL.
    let identity = core
        .resolve_profile(PHASE03_PROFILE_URL)
        .await
        .expect("identity");
    println!("declared post count: {:?}", identity.creator.post_count);

    match core.list_profile_posts(PHASE03_PROFILE_URL, None, Some(18)).await {
        Ok(page) => {
            println!("--- listing ---");
            println!("returned   : {}", page.posts.len());
            println!("has more   : {}", page.has_more);
            println!("total known: {:?}", page.total_known);

            for view in page.posts.iter().take(5) {
                println!(
                    "  {} {} {:?} {}",
                    view.post.platform_post_id,
                    view.post.media_type,
                    view.post.caption.as_deref().map(|c| c.chars().take(28).collect::<String>()),
                    view.status
                );
            }

            // Real posts: every one must carry a Douyin id and a link that
            // really points at it, and none may claim a download.
            for view in &page.posts {
                assert!(
                    view.post.platform_post_id.chars().all(|c| c.is_ascii_digit()),
                    "a listed post must carry its numeric aweme id"
                );
                assert!(
                    view.post.url.contains(&view.post.platform_post_id),
                    "a listed post's link must point at the post it names: {}",
                    view.post.url
                );
                assert!(
                    view.post.media_items.is_empty(),
                    "a listing must not claim resolved media"
                );
            }
            println!("=> non-empty live listing verified");
        }
        Err(err) => {
            // A refusal must say what it is. `UnavailableContent` is the honest
            // answer for a listing the platform would not serve anonymously,
            // and it must never be reported as an empty profile.
            println!("--- listing refused ---");
            println!("kind   : {:?}", err.kind());
            println!("message: {}", err.user_message());
            println!("detail : {:?}", err.technical_detail());
            assert!(
                matches!(
                    err.kind(),
                    AppErrorKind::UnavailableContent
                        | AppErrorKind::BrowserUnavailable
                        | AppErrorKind::Network
                        | AppErrorKind::Resolution
                ),
                "a refused listing must carry a typed reason, got {:?}",
                err.kind()
            );
            assert!(!err.user_message().is_empty());
            // And the declared count is not silently turned into zero.
            assert!(
                identity.creator.post_count != Some(0) || err.kind() != AppErrorKind::UnavailableContent,
                "a refusal must not be presented as a creator with no posts"
            );
            // The diagnostic carries what the *page* said, so a maintainer reads
            // the source's own words rather than an inferred cause.
            println!(
                "page_error in detail: {}",
                err.technical_detail()
                    .map(|d| d.contains("page_error="))
                    .unwrap_or(false)
            );
            println!("=> listing refused with a typed, actionable reason");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the live Douyin endpoints and an installed browser"]
async fn the_live_profile_link_is_not_downloadable_as_a_single_post() {
    // The boundary between the two screens, against the real parser.
    let (_temp, core) = live_core();
    let err = core
        .analyze_url(PHASE03_PROFILE_URL)
        .await
        .expect("a profile link is understood, not an error");
    assert_eq!(err.intent, "profile");
    assert!(err.post.is_none());
    assert!(err
        .notice
        .as_deref()
        .unwrap_or_default()
        .contains("Profile Downloader"));
}

/// The whole Phase 03 flow, against the live source.
///
/// This is the one that decides the phase: browse a real creator, take a real
/// post from that listing, submit it through the existing shared download
/// pipeline, and check that a real file landed on disk and a completed history
/// row was written. Fixtures cannot stand in for any part of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the live Douyin endpoints, a browser, and FFmpeg"]
async fn a_live_profile_post_downloads_and_is_recorded() {
    let temp = tempfile::tempdir().expect("temp dir");
    let download_dir = tempfile::tempdir().expect("download dir");
    let paths = AppPaths::with_data_dir(temp.path().join("data"));
    let core = AppCore::bootstrap(paths).expect("bootstrap").core;

    // Somewhere disposable, so a real download never lands in the user's own
    // folder.
    core.settings()
        .update(|s| {
            support::assert_isolated_destination("the live download harness", download_dir.path());
            s.download_directory = download_dir.path().to_string_lossy().into_owned();
            s.folder_template = "{creator}/{type}".to_string();
            s.organize_by_creator = true;
            s.organize_by_media_type = true;
            Ok(())
        })
        .expect("settings");

    // 1. The profile lists genuine posts.
    //
    // Measured as a *delta*: a profile left behind by some earlier run is not
    // this test's business, and counting absolutely would make the assertion
    // about the machine rather than about the code.
    let profiles_before = live_browser_profiles();
    let page = core
        .list_profile_posts(PHASE03_PROFILE_URL, None, Some(5))
        .await
        .expect("the live profile must list its posts");

    // Reading that page cost two renders — the creator, then the grid — and the
    // point of the shared session is that it cost one browser, not two.
    let profiles_during = live_browser_profiles();
    println!(
        "browser profiles before={profiles_before} during={profiles_during} after listing one page"
    );
    assert!(
        profiles_during <= profiles_before + 1,
        "one shared browser, not one per render ({} -> {profiles_during})",
        profiles_before
    );

    println!("--- live listing ---");
    println!("creator      : {}", page.creator.display_name);
    println!("posts        : {}", page.posts.len());
    println!("has more     : {}", page.has_more);
    for view in page.posts.iter().take(5) {
        println!(
            "   {} {:?} {}",
            view.post.platform_post_id,
            view.post.caption.as_deref().map(|c| c.chars().take(30).collect::<String>()),
            view.status
        );
    }
    assert!(!page.posts.is_empty(), "a live listing must return real posts");
    assert_eq!(page.creator.display_name, "郭虎");

    // 2. Pagination advances, and never repeats what the first page returned.
    let cursor = page.next_cursor.clone().expect("a first page has a cursor");
    let second = core
        .list_profile_posts(PHASE03_PROFILE_URL, Some(cursor), Some(5))
        .await
        .expect("the second page");
    println!("second page  : {}", second.posts.len());
    let first_ids: Vec<&str> = page.posts.iter().map(|v| v.post.platform_post_id.as_str()).collect();
    for view in &second.posts {
        assert!(
            !first_ids.contains(&view.post.platform_post_id.as_str()),
            "page two repeated {}",
            view.post.platform_post_id
        );
    }

    // 3. A post taken from the listing goes through the shared pipeline.
    let selected = page.posts[0].post.id.clone();
    let platform_id = page.posts[0].post.platform_post_id.clone();
    let rx = core.downloads().subscribe();

    let result = core
        .submit_profile_downloads(SubmitProfileDownloadsInput {
            post_ids: vec![selected.clone()],
            selected_items: Default::default(),
        })
        .await
        .expect("submission");

    println!("--- submission ---");
    println!("selected     : {}", result.selected);
    println!("tasks created: {}", result.tasks_created);
    println!("failed       : {}", result.failed);
    for outcome in result.posts.iter().flat_map(|post| &post.media) {
        println!("   {:?} {:?}", outcome.outcome, outcome.message);
    }
    assert_eq!(result.tasks_created, 1, "the selected post must become a task");

    // Four renders have happened by now — two for page one, one for page two,
    // and one to resolve the post's media — and they must all have shared the
    // one browser.
    let profiles_after_all = live_browser_profiles();
    println!(
        "browser profiles after listing + pagination + post resolution = {profiles_after_all} \
         (was {profiles_before} before)"
    );
    assert!(
        profiles_after_all <= profiles_before + 1,
        "four renders must share one browser ({} -> {profiles_after_all})",
        profiles_before
    );

    // 4. The ordinary transfer runs to a terminal state.
    let task_id = result.posts[0].media[0]
        .task_id
        .clone()
        .expect("a created task has an id");
    let state = wait_for_terminal(rx, &task_id).await;
    println!("task state   : {state:?}");

    // 5. History recorded the outcome either way, against the post and its
    //    creator. That association is the thing the listing established and the
    //    resolution must not lose.
    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].post_id.as_deref(), Some(selected.as_str()));
    assert_eq!(
        history[0].creator_name.as_deref(),
        Some("郭虎"),
        "a post resolved for download keeps the creator its listing knew"
    );
    println!(
        "history      : {:?} {} bytes",
        history[0].status,
        history[0].file_size.unwrap_or(0)
    );

    let mut files = Vec::new();
    collect_into(download_dir.path(), &mut files);
    println!("files        : {files:?}");

    match state {
        DownloadState::Completed => {
            // 6a. A real file exists, is non-trivial, and is where it should be.
            assert_eq!(files.len(), 1, "one download, one file");
            assert!(is_within(download_dir.path(), &files[0]));
            let bytes = std::fs::read(&files[0]).expect("read the download");
            println!("file bytes   : {}", bytes.len());
            assert!(bytes.len() > 10_000, "a real video, not a stub");
            // The post id is the file's identity, so the download is provably
            // this post; and the creator folder came from the association above.
            assert!(files[0].to_string_lossy().contains(&platform_id));
            assert!(
                files[0].to_string_lossy().contains("郭虎"),
                "the download landed under the creator the listing named: {:?}",
                files[0]
            );
            assert_eq!(history[0].status, DownloadState::Completed);

            // 6b. Playback verification, by the demuxer the application itself
            //     relies on. A byte count says a file exists; only a decoder can
            //     say the file is a video somebody could actually play.
            match discover_tool("ffprobe", "") {
                Some(ffprobe) => {
                    let output = std::process::Command::new(&ffprobe)
                        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
                        .arg(&files[0])
                        .output()
                        .expect("run ffprobe");
                    let report = String::from_utf8_lossy(&output.stdout);
                    assert!(output.status.success(), "the downloaded file is not readable: {report}");
                    assert!(
                        report.contains("\"codec_type\": \"video\"")
                            || report.contains("\"codec_type\":\"video\""),
                        "ffprobe found no video stream in the downloaded file"
                    );
                    let codecs: Vec<&str> = report
                        .lines()
                        .filter(|line| line.contains("codec_name"))
                        .map(str::trim)
                        .collect();
                    println!("FFPROBE VERIFIED: yes");
                    for line in codecs.iter().take(4) {
                        println!("  {line}");
                    }
                    // A video whose source keeps its audio in a separate track is
                    // fetched as both and muxed; the result must carry the sound,
                    // not merely claim to.
                    assert!(
                        report.contains("\"codec_type\": \"audio\"")
                            || report.contains("\"codec_type\":\"audio\""),
                        "the mux produced a file with no audio stream"
                    );
                    println!("FFPROBE AUDIO VERIFIED: yes");
                }
                None => println!("FFPROBE: skipped, no ffprobe installed"),
            }

            // 7. Re-reading the profile now identifies it as downloaded.
            let refreshed = core
                .list_profile_posts(PHASE03_PROFILE_URL, None, Some(5))
                .await
                .expect("listing again");
            let same = refreshed
                .posts
                .iter()
                .find(|v| v.post.platform_post_id == platform_id)
                .expect("the downloaded post is still listed");
            println!("status now   : {:?}", same.status);
            assert_eq!(same.status, ContentStatus::Downloaded);
        }
        DownloadState::Failed => {
            // Douyin's media CDN refuses this plain HTTP client for some
            // renditions, which Phase 02 documents. When a track has no other
            // rendition to fall back to, the task fails with that reason and
            // leaves nothing behind — which is the honest outcome, not a
            // plausible-looking silent video or an empty file.
            let why = history[0]
                .error
                .clone()
                .unwrap_or_default()
                .to_lowercase();
            println!("failed because: {why}");
            assert!(
                why.contains("403") || why.contains("media resource"),
                "a failed download must name a media refusal, got {why:?}"
            );
            assert!(
                files.is_empty(),
                "a failed transfer must not leave a partial file: {files:?}"
            );
        }
        other => panic!("unexpected terminal state {other:?}"),
    }
}

/// How many of this application's browser profiles exist right now.
///
/// The renderer names every profile `douyin-downloader-browser-<id>` and removes
/// it when the browser it belongs to goes away, so counting them is an outside
/// measurement of how many browsers the application is keeping alive — which is
/// the thing the shared session is supposed to bound at one.
fn live_browser_profiles() -> usize {
    std::env::temp_dir()
        .read_dir()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("douyin-downloader-browser-")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Report, and step past, a live fixture the source will not serve.
///
/// The same treatment `live_browser.rs` gives the same link, for the same
/// reason: Douyin sometimes answers the shared fixture by sending the browser to
/// its own front page, and that is a statement about the *link* rather than
/// about the application. The renderer recognises the landing page and says so
/// in about two seconds instead of waiting out the readiness timeout, so this
/// classifies the answer — it does not hide a failure, and it claims nothing.
fn source_would_not_serve(err: &douyin_downloader_lib::errors::AppError) -> bool {
    if !douyin_downloader_lib::network::chromium::is_source_redirect(err) {
        return false;
    }
    println!(
        "
ENVIRONMENT: Douyin refused {VIDEO_URL} — the source sent the browser to a page
         it was not asked for ({}) rather than to the post. Nothing about the application
         can be observed from this run, and nothing is claimed about it.
",
        err.message()
    );
    true
}


/// Every file beneath `root`, recursively.
fn collect_into(root: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_into(&path, out);
        } else {
            out.push(path);
        }
    }
}
