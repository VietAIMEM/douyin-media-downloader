//! End-to-end command tests.
//!
//! These go through Tauri's real IPC bridge against a mock runtime, so what is
//! exercised is the whole path the React frontend uses: a command name, a JSON
//! body, a serialized result or a serialized *error*. Nothing here is called
//! directly, which is the point — a command that is not registered, or whose
//! return type stopped being serializable, fails here rather than in the UI.

mod support;

use std::sync::Arc;

use douyin_downloader_lib::app_state::AppState;
use douyin_downloader_lib::config::AppPaths;
use douyin_downloader_lib::database::repositories::media as media_repo;
use douyin_downloader_lib::database::repositories::posts as posts_repo;
use douyin_downloader_lib::media::MockMuxer;
use douyin_downloader_lib::models::{DownloadState, MediaItem, MediaType, Platform, Post};
use douyin_downloader_lib::network::renderer::{
    MediaSource, MediaSourceKind, MockPageRenderer, NetworkMedia,
};
use douyin_downloader_lib::network::{HttpClient, MockHttpClient, NoopRenderer, PageRenderer};
use douyin_downloader_lib::resolvers::fixture::{FIXTURE_PROFILE_URL, FIXTURE_VIDEO_URL};
use douyin_downloader_lib::services::AppCore;
use serde_json::{json, Value};
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewWindow, WebviewWindowBuilder};

type MockWebview = WebviewWindow<MockRuntime>;

/// A started application plus its webview, both kept alive for the test's
/// duration (dropping the app would tear the webview down).
struct Harness {
    _temp: tempfile::TempDir,
    _app: App<MockRuntime>,
    webview: MockWebview,
    /// The same core the commands reach through managed state, kept here so a
    /// test can seed or inspect state the commands deliberately do not expose.
    core: Arc<AppCore>,
}

impl Harness {
    /// Boot the real application core over a throwaway data directory and
    /// register the same handlers `lib.rs` does.
    fn start() -> Self {
        Self::start_with_client(None)
    }

    /// Boot the core over a mock HTTP transport, so resolution can be exercised
    /// without the network.
    ///
    /// The page renderer is always substituted with a no-op. Without that the
    /// core would discover the machine's real browser and launch it, which would
    /// make these tests slow and dependent on what is installed.
    fn start_with_client(client: Option<MockHttpClient>) -> Self {
        Self::start_with(client, None)
    }

    /// Boot the core with both the HTTP transport and the page renderer
    /// substituted.
    fn start_with(
        client: Option<MockHttpClient>,
        renderer: Option<Arc<MockPageRenderer>>,
    ) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let paths = AppPaths::with_data_dir(temp.path().to_path_buf());

        let http: Arc<dyn HttpClient> = match client {
            Some(client) => Arc::new(client),
            None => Arc::new(MockHttpClient::new()),
        };
        let page_renderer: Arc<dyn PageRenderer> = match renderer {
            Some(renderer) => renderer,
            None => Arc::new(NoopRenderer),
        };

        let core = Arc::new(
            AppCore::bootstrap_with(
                paths,
                Some(http),
                Some(page_renderer),
                // A mock muxer, so these tests never depend on FFmpeg being present.
                Some(Arc::new(MockMuxer::producing(true, true))),
            )
            .expect("bootstrap")
            .core,
        );

        // Isolated before anything can download. Without this the core falls
        // back to `default_download_directory()`, which on this machine is the
        // user's own `Downloads\Douyin` — and that is exactly how fifteen mock
        // downloads and ten probe files came to be sitting in it.
        let download_dir = support::workspace_subdir(temp.path(), "media", "the command harness");
        support::redirect_downloads_to(&core, &download_dir, "the command harness");

        let app = mock_builder()
            .manage(AppState::from(Arc::clone(&core)))
            .invoke_handler(tauri::generate_handler![
                douyin_downloader_lib::commands::app_info::get_app_info,
                douyin_downloader_lib::commands::database::get_database_status,
                douyin_downloader_lib::commands::downloads::clear_completed_downloads,
                douyin_downloader_lib::commands::downloads::get_download,
                douyin_downloader_lib::commands::downloads::get_download_queue,
                douyin_downloader_lib::commands::downloads::get_download_summary,
                douyin_downloader_lib::commands::downloads::list_downloads,
                douyin_downloader_lib::commands::downloads::pause_download,
                douyin_downloader_lib::commands::downloads::remove_download,
                douyin_downloader_lib::commands::downloads::resume_download,
                douyin_downloader_lib::commands::downloads::retry_failed_downloads,
                douyin_downloader_lib::commands::history::get_history_count,
                douyin_downloader_lib::commands::history::list_history,
                douyin_downloader_lib::commands::library::export_library,
                douyin_downloader_lib::commands::library::list_library_creators,
                douyin_downloader_lib::commands::library::query_library,
                // `reveal_export` needs a real app handle, for the same reason
                // `open_download_path` does; its containment check is covered by
                // the unit tests on `is_within` and the service tests.
                douyin_downloader_lib::commands::profile::list_profile_posts,
                douyin_downloader_lib::commands::profile::resolve_profile,
                douyin_downloader_lib::commands::profile::submit_profile_downloads,
                douyin_downloader_lib::commands::settings::get_settings,
                douyin_downloader_lib::commands::settings::get_settings_options,
                douyin_downloader_lib::commands::settings::reset_settings,
                douyin_downloader_lib::commands::settings::save_settings,
                douyin_downloader_lib::commands::single_url::analyze_url,
                douyin_downloader_lib::commands::single_url::cancel_download,
                douyin_downloader_lib::commands::single_url::create_download_task,
                // `open_download_path` needs a real app handle to hand a path to
                // the OS, which the mock runtime does not provide; it is covered
                // by its own unit tests instead.
                douyin_downloader_lib::commands::single_url::retry_download,
                douyin_downloader_lib::commands::url::classify_url,
                douyin_downloader_lib::commands::url::expand_url,
                douyin_downloader_lib::commands::url::get_test_fixtures,
                douyin_downloader_lib::commands::url::resolve_url_metadata,
            ])
            .build(mock_context(noop_assets()))
            .expect("build mock app");

        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("build webview");

        Self {
            _temp: temp,
            _app: app,
            webview,
            core,
        }
    }

    /// Invoke a command exactly as the frontend does.
    fn call(&self, cmd: &str, args: Value) -> Result<Value, Value> {
        let body = if args.is_null() {
            InvokeBody::default()
        } else {
            InvokeBody::Json(args)
        };

        tauri::test::get_ipc_response(
            &self.webview,
            InvokeRequest {
                cmd: cmd.into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "http://tauri.localhost".parse().unwrap(),
                body,
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|response| response.deserialize::<Value>().expect("response deserializes"))
    }

    /// Invoke a command that is expected to succeed.
    fn ok(&self, cmd: &str, args: Value) -> Value {
        self.call(cmd, args)
            .unwrap_or_else(|err| panic!("{cmd} was rejected: {err}"))
    }

    /// Invoke a command that is expected to fail, returning the error payload.
    fn err(&self, cmd: &str, args: Value) -> Value {
        match self.call(cmd, args) {
            Ok(value) => panic!("{cmd} unexpectedly succeeded: {value}"),
            Err(err) => err,
        }
    }
}

// --- application identity ---------------------------------------------------

/// Every media outcome of every post, flattened.
///
/// The result nests outcomes under the post they belong to, because one post
/// can contribute many items — a carousel is fifteen downloads. The assertions
/// here are about individual items, so this is the view they want.
fn all_outcomes(result: &serde_json::Value) -> Vec<serde_json::Value> {
    result["posts"]
        .as_array()
        .map(|posts| {
            posts
                .iter()
                .flat_map(|post| post["media"].as_array().cloned().unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn get_app_info_round_trips_over_ipc() {
    let harness = Harness::start();
    let info = harness.ok("get_app_info", Value::Null);

    assert_eq!(info["name"], "Douyin Downloader");
    assert_eq!(info["identifier"], "com.douyindownloader.desktop");
    assert_eq!(info["os"], "windows");
    assert_eq!(info["demoMode"], false);
    assert_eq!(info["supportedPlatforms"][0], "douyin");
    assert!(info["version"].as_str().is_some_and(|v| !v.is_empty()));

    // Four capabilities: the URL parser plus profile/post/media resolution.
    let resolvers = info["resolvers"].as_array().expect("resolvers is an array");
    assert_eq!(resolvers.len(), 4);
    assert!(resolvers.iter().any(|r| r["capability"] == "post"));
    assert!(resolvers
        .iter()
        .all(|r| r["platform"] == "douyin" && r["name"].is_string()));
}

#[test]
fn get_database_status_reports_a_healthy_initialized_schema() {
    let harness = Harness::start();
    let status = harness.ok("get_database_status", Value::Null);

    assert_eq!(status["healthy"], true);
    assert_eq!(status["isPersistent"], true);
    assert_eq!(status["schemaVersion"], status["latestSchemaVersion"]);
    assert!(status["schemaVersion"].as_i64().unwrap_or(0) >= 1);
    assert_eq!(status["foreignKeys"], true);

    let migrations = status["appliedMigrations"].as_array().expect("migrations");
    assert!(!migrations.is_empty());
    assert!(migrations[0]["description"].is_string());
    assert!(migrations[0]["appliedAt"].is_string());

    let tables: Vec<&str> = status["tableCounts"]
        .as_array()
        .expect("table counts")
        .iter()
        .map(|entry| entry["table"].as_str().unwrap_or_default())
        .collect();
    for table in ["creators", "posts", "media_items", "downloads", "download_history", "settings"] {
        assert!(tables.contains(&table), "{table} missing from {tables:?}");
    }
}

// --- settings ---------------------------------------------------------------

#[test]
fn settings_survive_a_save_and_read_back_through_ipc() {
    let harness = Harness::start();

    let initial = harness.ok("get_settings", Value::Null);
    // The frontend's camelCase contract: a renamed field breaks here.
    assert!(initial["downloadDirectory"].is_string());
    assert!(initial["maxConcurrentDownloads"].is_u64());
    assert!(initial["filenameTemplate"].is_string());
    assert!(initial["folderTemplate"].is_string());
    assert!(initial["conflictPolicy"].is_string());
    assert!(initial["theme"].is_string());
    assert!(initial["logLevel"].is_string());
    assert!(initial["organizeByCreator"].is_boolean());
    assert!(initial["requestTimeoutSeconds"].is_u64());

    let mut edited = initial.clone();
    edited["maxConcurrentDownloads"] = json!(6);
    edited["theme"] = json!("dark");

    let stored = harness.ok("save_settings", json!({ "settings": edited }));
    assert_eq!(stored["maxConcurrentDownloads"], 6);
    assert_eq!(stored["theme"], "dark");

    // Reading it back proves the write reached SQLite, not just the cache.
    let reread = harness.ok("get_settings", Value::Null);
    assert_eq!(reread["maxConcurrentDownloads"], 6);
    assert_eq!(reread["theme"], "dark");

    // And a reset puts the defaults back.
    let reset = harness.ok("reset_settings", Value::Null);
    assert_eq!(reset["maxConcurrentDownloads"], initial["maxConcurrentDownloads"]);
    assert_eq!(reset["theme"], "system");
}

#[test]
fn an_invalid_settings_payload_is_rejected_with_a_structured_error() {
    let harness = Harness::start();

    let mut invalid = harness.ok("get_settings", Value::Null);
    invalid["maxConcurrentDownloads"] = json!(0);

    let error = harness.err("save_settings", json!({ "settings": invalid }));
    assert_eq!(error["kind"], "validation");
    assert_eq!(error["code"], "validation_error");
    assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));

    // The rejected value must not have been stored.
    let current = harness.ok("get_settings", Value::Null);
    assert_ne!(current["maxConcurrentDownloads"], 0);
}

#[test]
fn get_settings_options_supplies_the_form_metadata() {
    let harness = Harness::start();
    let options = harness.ok("get_settings_options", Value::Null);

    for key in ["conflictPolicies", "themes", "logLevels"] {
        let choices = options[key].as_array().unwrap_or_else(|| panic!("{key} missing"));
        assert!(!choices.is_empty());
        assert!(choices.iter().all(|c| c["value"].is_string() && c["label"].is_string()));
    }

    // Placeholders are sent brace-wrapped so the form can show exactly what to
    // type into a template.
    let placeholders = options["filenamePlaceholders"].as_array().expect("placeholders");
    assert!(placeholders.iter().any(|p| p == "{creator}"));
    assert!(placeholders.iter().any(|p| p == "{id}"));

    assert!(options["defaultDownloadDirectory"].is_string());
    assert!(options["maxConcurrentLimit"].as_u64().unwrap_or(0) >= 1);
    assert!(options["timeoutSecondsMin"].as_u64().unwrap_or(0) >= 1);
    assert!(
        options["timeoutSecondsMax"].as_u64().unwrap_or(0)
            > options["timeoutSecondsMin"].as_u64().unwrap_or(0)
    );
}

// --- URL inspection ---------------------------------------------------------

#[test]
fn classify_url_accepts_the_shipped_test_links() {
    let harness = Harness::start();

    for url in [FIXTURE_PROFILE_URL, FIXTURE_VIDEO_URL] {
        let classified = harness.ok("classify_url", json!({ "url": url }));
        assert_eq!(classified["platform"], "douyin");
        assert_eq!(classified["kind"], "short_link");
        assert_eq!(classified["isShortLink"], true);
        assert_eq!(classified["requiresExpansion"], true);
        assert_eq!(classified["original"], url);
        assert_eq!(classified["canonical"], url);
    }
}

#[test]
fn classify_url_rejects_an_empty_link_with_a_validation_error() {
    let harness = Harness::start();

    let error = harness.err("classify_url", json!({ "url": "   " }));
    assert_eq!(error["kind"], "validation");
    assert_eq!(error["code"], "validation_error");
    assert_eq!(error["message"], "Paste a Douyin link to continue.");
}

#[test]
fn classify_url_reports_a_foreign_link_as_unsupported() {
    let harness = Harness::start();

    let error = harness.err("classify_url", json!({ "url": "https://example.com/video/1" }));
    assert_eq!(error["kind"], "unsupported_content");
    assert_eq!(error["message"], "Only Douyin links are supported.");
}

#[test]
fn get_test_fixtures_describes_the_links_the_project_ships() {
    let harness = Harness::start();
    let fixtures = harness.ok("get_test_fixtures", Value::Null);

    let fixtures = fixtures.as_array().expect("array of fixtures");
    assert_eq!(fixtures.len(), 2);

    let urls: Vec<&str> = fixtures.iter().map(|f| f["url"].as_str().unwrap()).collect();
    assert!(urls.contains(&FIXTURE_PROFILE_URL));
    assert!(urls.contains(&FIXTURE_VIDEO_URL));
    // The expected kind is derived by running the parser, not hard-coded.
    assert!(fixtures.iter().all(|f| f["expectedKind"] == "short_link"));
    // The screen each one belongs on is declared, because two short links are
    // indistinguishable until the redirect is followed.
    let intents: Vec<&str> = fixtures.iter().map(|f| f["intent"].as_str().unwrap()).collect();
    assert!(intents.contains(&"profile"));
    assert!(intents.contains(&"post"));
}

// --- downloads and history --------------------------------------------------

#[test]
fn the_download_queue_and_history_start_empty_but_real() {
    let harness = Harness::start();

    let summary = harness.ok("get_download_summary", Value::Null);
    assert_eq!(summary["total"], 0);
    assert_eq!(summary["completed"], 0);

    let downloads = harness.ok("list_downloads", Value::Null);
    assert_eq!(downloads.as_array().expect("array").len(), 0);

    let history = harness.ok("list_history", json!({ "limit": 50 }));
    assert_eq!(history.as_array().expect("array").len(), 0);

    let count = harness.ok("get_history_count", Value::Null);
    assert_eq!(count, 0);
}

// --- failure modes ----------------------------------------------------------

#[test]
fn an_unregistered_command_fails_rather_than_silently_succeeding() {
    let harness = Harness::start();
    let error = harness.err("a_command_that_does_not_exist", Value::Null);
    assert!(!error.is_null());
}

#[test]
fn missing_arguments_are_rejected_as_an_error_payload() {
    let harness = Harness::start();
    // `url` is required; the frontend must not be able to call the command
    // without it and get a success.
    let error = harness.err("classify_url", json!({}));
    assert!(!error.is_null());
}

// --- single URL analysis over IPC -------------------------------------------

/// A share page shaped like the one Douyin serves to a mobile agent.
///
/// Integration tests are a separate crate, so they carry their own copy rather
/// than reaching into the library's unit-test fixtures.
fn share_page_with_one_video() -> String {
    let aweme = r#"{
        "aweme_id": "7281234567890123456",
        "desc": "A single clip",
        "create_time": 1760000000,
        "author": { "sec_uid": "MS4wLjABAAAAauthor", "nickname": "Alice", "unique_id": "alice" },
        "video": {
            "duration": 8000, "width": 1080, "height": 1920,
            "play_addr": {
                "url_list": ["https://v3-cdn.douyin.com/aweme/v1/play/?video_id=clean"],
                "width": 1080, "height": 1920, "data_size": 1000
            },
            "download_addr": {
                "url_list": ["https://v3-cdn.douyin.com/aweme/v1/playwm/?video_id=clean"]
            },
            "cover": { "url_list": ["https://p3-cdn.douyin.com/cover.jpeg"] }
        },
        "images": null
    }"#;
    format!(
        "<html><script>window._ROUTER_DATA = {{\"loaderData\":{{\"video_(id)/page\":\
         {{\"videoInfoRes\":{{\"item_list\":[{aweme}]}}}}}}}};</script></html>"
    )
}

fn video_harness() -> Harness {
    Harness::start_with_client(Some(
        MockHttpClient::new()
            .with_redirect(
                FIXTURE_VIDEO_URL,
                "https://www.iesdouyin.com/share/video/7281234567890123456/",
            )
            .with_body(
                "https://www.iesdouyin.com/share/video/7281234567890123456/",
                share_page_with_one_video(),
            ),
    ))
}

#[test]
fn analyze_url_returns_a_normalized_post_with_media() {
    let harness = video_harness();
    let analyzed = harness.ok("analyze_url", json!({ "url": FIXTURE_VIDEO_URL }));

    assert_eq!(analyzed["intent"], "post");
    assert_eq!(analyzed["resolutionStage"], "resolved");
    assert!(analyzed["notice"].is_null());

    let post = &analyzed["post"];
    assert_eq!(post["platformPostId"], "7281234567890123456");
    assert_eq!(post["caption"], "A single clip");
    // The link the user pasted is preserved, while the classification describes
    // what the link *resolved to* — the short link was followed.
    assert_eq!(analyzed["classification"]["original"], FIXTURE_VIDEO_URL);
    assert_eq!(analyzed["classification"]["kind"], "post");
    assert_eq!(analyzed["classification"]["isShortLink"], false);
    assert!(analyzed["classification"]["canonical"]
        .as_str()
        .unwrap_or_default()
        .contains("/video/"));

    let media = analyzed["media"].as_array().expect("media array");
    let video = media
        .iter()
        .find(|m| m["mediaType"] == "video")
        .expect("a video item");
    assert_eq!(video["extension"], "mp4");
    let variants = video["variants"].as_array().expect("variants");
    assert!(variants.iter().any(|v| v["isOriginal"] == true));
    assert!(variants.iter().any(|v| v["sourceUrl"]
        .as_str()
        .unwrap_or_default()
        .contains("playwm")));

    // The creator travels with the analysis.
    assert_eq!(analyzed["creator"]["displayName"], "Alice");
}

#[test]
fn analyze_url_treats_a_profile_link_as_a_profile_not_a_post() {
    let harness = Harness::start_with_client(Some(MockHttpClient::new().with_redirect(
        FIXTURE_PROFILE_URL,
        "https://www.iesdouyin.com/share/user/MS4wLjABAAAAexample",
    )));

    let analyzed = harness.ok("analyze_url", json!({ "url": FIXTURE_PROFILE_URL }));

    assert_eq!(analyzed["intent"], "profile");
    assert!(analyzed["post"].is_null(), "a profile must not become a post");
    assert_eq!(analyzed["media"].as_array().map(Vec::len), Some(0));
    assert!(analyzed["notice"]
        .as_str()
        .unwrap_or_default()
        .contains("Profile Downloader"));
    assert_eq!(analyzed["classification"]["kind"], "profile");
    assert_eq!(analyzed["classification"]["original"], FIXTURE_PROFILE_URL);
}

#[test]
fn analyze_url_rejects_a_foreign_link_with_a_typed_error() {
    let harness = Harness::start();
    let error = harness.err("analyze_url", json!({ "url": "https://example.com/video/1" }));
    assert_eq!(error["kind"], "unsupported_content");
}

#[test]
fn analyze_url_reports_unavailable_content_rather_than_inventing_a_post() {
    // The redirect resolves, but the page carries no post payload.
    let harness = Harness::start_with_client(Some(
        MockHttpClient::new()
            .with_redirect(
                FIXTURE_VIDEO_URL,
                "https://www.iesdouyin.com/share/video/7281234567890123456/",
            )
            .with_body(
                "https://www.iesdouyin.com/share/video/7281234567890123456/",
                "<html><body>an anti-bot shell with no data</body></html>",
            ),
    ));

    let error = harness.err("analyze_url", json!({ "url": FIXTURE_VIDEO_URL }));
    assert_eq!(error["kind"], "resolution");
    assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));
}

#[test]
fn create_download_task_rejects_ids_that_were_never_analyzed() {    let harness = Harness::start();
    let error = harness.err(
        "create_download_task",
        json!({ "input": { "postId": "missing", "mediaItemId": "missing" } }),
    );
    assert_eq!(error["kind"], "not_found");
}

#[test]
fn downloads_can_be_cancelled_over_ipc() {
    let harness = video_harness();
    // Analyze first so there is a persisted post to reference.
    let analyzed = harness.ok("analyze_url", json!({ "url": FIXTURE_VIDEO_URL }));
    let post_id = analyzed["post"]["id"].as_str().expect("post id").to_string();
    let media_id = analyzed["media"][0]["id"].as_str().expect("media id").to_string();

    // Creating the task queues a real transfer; cancelling it must succeed and
    // be reflected in the task list.
    let created = harness.ok(
        "create_download_task",
        json!({ "input": { "postId": post_id, "mediaItemId": media_id } }),
    );
    assert_eq!(created["state"], "queued");
    assert_eq!(created["extension"], "mp4");

    let task_id = created["id"].as_str().expect("task id").to_string();
    let cancelled = harness.ok("cancel_download", json!({ "taskId": task_id }));
    assert_eq!(cancelled["state"], "cancelled");

    // And it lands in history, because a cancelled download is still history.
    let history = harness.ok("list_history", json!({ "limit": 50 }));
    let entries = history.as_array().expect("history array");
    assert!(entries.iter().any(|e| e["status"] == "cancelled"));
}

#[test]
fn a_post_analyzed_twice_keeps_one_identity_and_stays_downloadable() {
    // The failure this pins down was found by using the installed application,
    // not by a test: analyzing a link the program had already seen returned an
    // id the database did not have. The stored row keeps its original identity
    // across analyses, while the freshly resolved post carried a newly
    // generated one — so every later command that keyed on the analysis, the
    // Download button first among them, answered "analyze the link again".
    // Everything looked right: the analysis succeeded, the post was stored, and
    // the preview rendered.
    //
    // It only happened on the *second* analysis. A fixture suite in which every
    // post is new on every run cannot see it, which is why this test analyzes
    // the same link twice rather than once.
    let harness = video_harness();

    let first = harness.ok("analyze_url", json!({ "url": FIXTURE_VIDEO_URL }));
    let first_id = first["post"]["id"].as_str().expect("post id").to_string();

    let second = harness.ok("analyze_url", json!({ "url": FIXTURE_VIDEO_URL }));
    let second_id = second["post"]["id"].as_str().expect("post id").to_string();

    assert_eq!(
        second_id, first_id,
        "the second analysis named a different post than the one that was stored"
    );

    // And the ids the second analysis hands out are the ones that work.
    let media_id = second["media"][0]["id"].as_str().expect("media id").to_string();
    let created = harness.ok(
        "create_download_task",
        json!({ "input": { "postId": second_id, "mediaItemId": media_id } }),
    );
    assert_eq!(created["state"], "queued");
}

// --- browser-assisted resolution over IPC -----------------------------------

/// The share page Douyin serves when it will not server-render the post: the
/// marker is present, the post is not.
fn config_only_page() -> String {
    "<html><script>window._ROUTER_DATA = {\"loaderData\":{\"video_layout\":null,\
     \"video_(id)/page\":{\"itemId\":\"7281234567890123456\",\"renderInSSR\":1}}};\
     </script></html>"
        .to_string()
}

#[test]
fn analyze_url_falls_back_to_a_browser_when_the_pages_hold_no_post() {
    let renderer = Arc::new(
        MockPageRenderer::returning(vec![
            MediaSource::test(
                "https://cdn.example/media-video-avc1/?t=1",
                MediaSourceKind::Video,
            ),
            MediaSource::test(
                "https://cdn.example/media-audio-und-mp4a/?t=1",
                MediaSourceKind::Audio,
            ),
        ])
        .with_title("A caption - 抖音")
        .with_final_url("https://www.douyin.com/video/7281234567890123456"),
    );

    let client = MockHttpClient::new().with_body(
        "https://www.iesdouyin.com/share/video/7281234567890123456/",
        config_only_page(),
    );
    let harness = Harness::start_with(Some(client), Some(Arc::clone(&renderer)));

    let analyzed = harness.ok(
        "analyze_url",
        json!({ "url": "https://www.douyin.com/video/7281234567890123456" }),
    );

    assert_eq!(analyzed["intent"], "post");
    // The frontend is told which path resolved it, rather than having to guess.
    assert_eq!(analyzed["resolvedVia"], "browser");
    // The caption comes from the page title with the site suffix removed.
    assert_eq!(analyzed["post"]["caption"], "A caption");
    // Both tracks are represented, and the layout is stated.
    let media = analyzed["media"].as_array().expect("media array");
    assert_eq!(media.len(), 2);
    let video = media.iter().find(|m| m["mediaType"] == "video").expect("video");
    let audio = media.iter().find(|m| m["mediaType"] == "audio").expect("audio");
    // The video's audio is a separate rendition, so the video item is marked
    // `audio_video`; that is what makes `create_download_task` fetch and mux
    // both tracks below.
    assert_eq!(video["metadata"]["track_layout"], "audio_video");
    assert_eq!(audio["metadata"]["track_layout"], "audio_only");
    assert_eq!(renderer.calls(), 1);

    // And the resolved media can be turned into a real download task.
    let post_id = analyzed["post"]["id"].as_str().unwrap().to_string();
    let media_id = video["id"].as_str().unwrap().to_string();
    let created = harness.ok(
        "create_download_task",
        json!({ "input": { "postId": post_id, "mediaItemId": media_id } }),
    );
    assert_eq!(created["state"], "queued");
    assert_eq!(created["extension"], "mp4");
}

/// The corrected classification, seen across the IPC boundary.
///
/// A page that was watched but never fetched a track has not been shown to lack
/// audio, so the frontend must be told `unknown` — which it renders as "audio not
/// checked" — rather than `video_only`, which claims the source has no sound.
#[test]
fn analyze_url_reports_unknown_audio_when_no_track_was_requested() {
    let renderer = Arc::new(
        MockPageRenderer::returning(vec![MediaSource::test(
            "https://cdn.example/media-video-avc1/?t=1",
            MediaSourceKind::Video,
        )])
        .with_title("A caption - 抖音")
        .with_final_url("https://www.douyin.com/video/7281234567890123456"),
    );

    let client = MockHttpClient::new().with_body(
        "https://www.iesdouyin.com/share/video/7281234567890123456/",
        config_only_page(),
    );
    let harness = Harness::start_with(Some(client), Some(Arc::clone(&renderer)));

    let analyzed = harness.ok(
        "analyze_url",
        json!({ "url": "https://www.douyin.com/video/7281234567890123456" }),
    );

    assert_eq!(analyzed["resolvedVia"], "browser");
    let media = analyzed["media"].as_array().expect("media array");
    assert_eq!(media.len(), 1);
    assert_eq!(media[0]["mediaType"], "video");
    assert_eq!(media[0]["metadata"]["track_layout"], "unknown");
}

/// The positive case still crosses the boundary as a real finding.
#[test]
fn analyze_url_reports_video_only_when_a_track_was_requested_without_audio() {
    let renderer = Arc::new(
        MockPageRenderer::returning(vec![MediaSource::test(
            "https://cdn.example/media-video-avc1/?t=1",
            MediaSourceKind::Video,
        )])
        .with_network_media(vec![NetworkMedia::test(
            "https://cdn.example/media-video-avc1/?t=2",
            "video/mp4",
        )])
        .with_title("A caption - 抖音")
        .with_final_url("https://www.douyin.com/video/7281234567890123456"),
    );

    let client = MockHttpClient::new().with_body(
        "https://www.iesdouyin.com/share/video/7281234567890123456/",
        config_only_page(),
    );
    let harness = Harness::start_with(Some(client), Some(Arc::clone(&renderer)));

    let analyzed = harness.ok(
        "analyze_url",
        json!({ "url": "https://www.douyin.com/video/7281234567890123456" }),
    );

    let media = analyzed["media"].as_array().expect("media array");
    assert_eq!(media.len(), 1, "both renditions belong to one video item");
    assert_eq!(media[0]["metadata"]["track_layout"], "video_only");
}

#[test]
fn analyze_url_does_not_start_a_browser_when_the_page_has_the_post() {
    let renderer = Arc::new(
        MockPageRenderer::returning(vec![MediaSource::test(
            "https://cdn.example/should-not-be-used",
            MediaSourceKind::Video,
        )])
        .with_title("unused"),
    );

    let client = MockHttpClient::new().with_body(
        "https://www.iesdouyin.com/share/video/7281234567890123456/",
        share_page_with_one_video(),
    );
    let harness = Harness::start_with(Some(client), Some(Arc::clone(&renderer)));

    let analyzed = harness.ok(
        "analyze_url",
        json!({ "url": "https://www.douyin.com/video/7281234567890123456" }),
    );

    assert_eq!(analyzed["resolvedVia"], "http");
    assert_eq!(
        renderer.calls(),
        0,
        "a page that already carried the post must not cost a browser launch"
    );
}

/// The application core, for a test that needs to seed or inspect state the
/// commands deliberately do not expose.
impl Harness {
    fn core(&self) -> &AppCore {
        &self.core
    }
}

/// Record a download task for `post_id`, pointing at `path`.
///
/// Seeds the state directly because the real transfer worker is attached in
/// these tests and races the assertions: a mock transport cannot serve media,
/// so a task created through the queue may be running or already failed by the
/// time the next call is made. Seeding makes the state under test exact.
fn seed_download(
    harness: &Harness,
    post_id: &str,
    state: DownloadState,
    path: Option<&std::path::Path>,
) {
    use douyin_downloader_lib::database::repositories::{
        downloads as downloads_repo, media as media_repo, posts as posts_repo,
    };
    use douyin_downloader_lib::models::{DownloadRequest, DownloadTask, MediaItem, MediaType};

    harness
        .core()
        .database()
        .with_conn(|conn| {
            let post = posts_repo::get(conn, post_id)?.expect("post is stored");

            let mut item = MediaItem::new(
                post.id.clone(),
                MediaType::Video,
                "https://cdn.example/seed.mp4",
            );
            item.post_id = post.id.clone();
            media_repo::replace_items_for_post(conn, &post.id, std::slice::from_ref(&item))?;

            let mut task = DownloadTask::new(&DownloadRequest::new(
                "https://cdn.example/seed.mp4",
                MediaType::Video,
            ));
            task.post_id = Some(post.id.clone());
            task.media_item_id = Some(item.id.clone());
            task.state = state;
            task.destination_path = path.map(|p| p.to_string_lossy().into_owned());
            if task.is_terminal() {
                task.completed_at = Some(douyin_downloader_lib::models::now());
            }
            downloads_repo::insert(conn, &task)?;
            Ok(())
        })
        .expect("seed a download");
}

// --- profile workflow -------------------------------------------------------
//
// The profile screen's three actions, driven over the same IPC bridge the
// frontend uses. The creator page is supplied by a mock renderer, so these are
// deterministic and offline; what the platform really serves is covered
// separately by the live suite.

const PROFILE_SEC_UID: &str = "MS4wLjABAAAAexample";
const LISTED_VIDEO_ID: &str = "7281234567890123456";
const LISTED_VIDEO_ID_2: &str = "7281234567890123457";

/// A rendered profile page carrying a creator and two post cards.
fn profile_page(posts: Value, post_count: Value) -> Value {
    json!({
        "creator": {
            "name": "郭虎",
            "handle": "42299195146",
            "signature": "导演",
            "avatarUrl": "https://p3.douyinpic.com/avatar.jpeg",
            "verified": true,
            "postCount": post_count
        },
        "posts": posts
    })
}

fn listed_card(id: &str) -> Value {
    json!({
        "id": id,
        "kind": "video",
        "url": format!("https://www.douyin.com/video/{id}"),
        "caption": format!("clip {id}"),
        "thumbnailUrl": "https://p3.douyinpic.com/t.jpeg",
        "hasImage": true
    })
}

/// A profile URL that needs no expansion, so no redirect has to be stubbed.
fn profile_url() -> String {
    format!("https://www.douyin.com/user/{PROFILE_SEC_UID}")
}

/// A core whose profile pages carry `payload`, and whose share page serves the
/// first listed post so its media can be resolved.
fn profile_harness(payload: Value) -> Harness {
    let renderer = Arc::new(MockPageRenderer::returning(Vec::new()).with_payload(payload));
    let client = MockHttpClient::new().with_body(
        format!("https://www.iesdouyin.com/share/video/{LISTED_VIDEO_ID}/"),
        share_page_with_one_video(),
    );
    Harness::start_with(Some(client), Some(renderer))
}

fn default_profile_harness() -> Harness {
    profile_harness(profile_page(
        json!([listed_card(LISTED_VIDEO_ID), listed_card(LISTED_VIDEO_ID_2)]),
        json!(2),
    ))
}

#[test]
fn resolve_profile_returns_the_creator_over_ipc() {
    let harness = default_profile_harness();
    let identity = harness.ok("resolve_profile", json!({ "url": profile_url() }));

    // The camelCase contract the frontend types depend on.
    let creator = &identity["creator"];
    assert_eq!(creator["platformId"], PROFILE_SEC_UID);
    assert_eq!(creator["displayName"], "郭虎");
    assert_eq!(creator["handle"], "@42299195146");
    assert_eq!(creator["description"], "导演");
    assert_eq!(creator["postCount"], 2);
    assert_eq!(creator["verified"], true);
    assert!(creator["avatarUrl"].as_str().unwrap_or_default().starts_with("https://"));
    // A stable local id the screen can key on.
    assert!(creator["id"].as_str().is_some_and(|id| !id.is_empty()));

    assert_eq!(identity["resolutionStage"], "resolved");
    assert_eq!(identity["resolvedVia"], "browser");
    assert!(identity["notice"].is_null());
    assert_eq!(identity["classification"]["kind"], "profile");
}

#[test]
fn resolve_profile_reports_identity_only_when_no_browser_can_be_used() {
    // Without a renderer the link still names who it is about; the screen is
    // told the details were not read rather than being shown invented ones.
    let harness = Harness::start_with(
        Some(MockHttpClient::new()),
        Some(Arc::new(MockPageRenderer::unavailable())),
    );
    let identity = harness.ok("resolve_profile", json!({ "url": profile_url() }));

    assert_eq!(identity["resolutionStage"], "url_only");
    assert_eq!(identity["creator"]["displayName"], PROFILE_SEC_UID);
    assert!(identity["notice"].as_str().unwrap_or_default().contains("identity"));
}

#[test]
fn resolve_profile_rejects_a_post_link_with_a_validation_error() {
    let harness = Harness::start();
    let err = harness.err(
        "resolve_profile",
        json!({ "url": "https://www.douyin.com/video/7281234567890123456" }),
    );
    assert_eq!(err["kind"], "validation");
    assert!(err["message"].as_str().unwrap_or_default().contains("Single URL"));
}

#[test]
fn resolve_profile_reports_a_foreign_link_as_unsupported() {
    let harness = Harness::start();
    let err = harness.err("resolve_profile", json!({ "url": "https://example.com/user/1" }));
    assert_eq!(err["kind"], "unsupported_content");
}

#[test]
fn list_profile_posts_returns_normalized_posts_with_their_local_state() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );

    assert_eq!(page["creator"]["displayName"], "郭虎");
    assert_eq!(page["hasMore"], false);
    assert!(page["nextCursor"].is_null());
    assert_eq!(page["totalKnown"], 2);
    assert_eq!(page["loadedTotal"], 2);

    let posts = page["posts"].as_array().expect("posts");
    assert_eq!(posts.len(), 2);
    let first = &posts[0];
    assert_eq!(first["post"]["platformPostId"], LISTED_VIDEO_ID);
    assert_eq!(first["post"]["mediaType"], "video");
    assert_eq!(first["post"]["caption"], format!("clip {LISTED_VIDEO_ID}"));
    // Discovered, not downloaded: a listing makes no claim about local state.
    assert_eq!(first["status"], "new");
    assert!(first["downloadState"].is_null());
    assert!(first["post"]["mediaItems"].as_array().expect("media").is_empty());
    // A local id the selection is keyed by, distinct from the platform id.
    assert!(first["post"]["id"].as_str().is_some_and(|id| !id.is_empty()));
}

#[test]
fn list_profile_posts_paginates_without_repeating_a_post() {
    let harness = default_profile_harness();

    let first = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 1 }),
    );
    assert_eq!(first["posts"].as_array().expect("posts").len(), 1);
    assert_eq!(first["hasMore"], true);
    let cursor = first["nextCursor"].as_str().expect("a cursor").to_string();

    let second = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": cursor, "limit": 1 }),
    );
    let posts = second["posts"].as_array().expect("posts");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["post"]["platformPostId"], LISTED_VIDEO_ID_2);
    assert_eq!(second["hasMore"], false);
}

#[test]
fn a_refused_listing_is_reported_rather_than_shown_as_an_empty_profile() {
    // The profile says it has two posts and the page lists none: that is the
    // platform withholding the listing, not a creator with nothing published.
    let harness = profile_harness(profile_page(json!([]), json!(2)));
    let err = harness.err(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );

    assert_eq!(err["kind"], "unavailable_content");
    assert!(err["message"]
        .as_str()
        .unwrap_or_default()
        .contains("did not return"));
}

#[test]
fn list_profile_posts_rejects_an_invalid_cursor() {
    let harness = default_profile_harness();
    let err = harness.err(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": "not-a-number", "limit": 18 }),
    );
    assert_eq!(err["kind"], "validation");
}

#[test]
fn submit_profile_downloads_creates_tasks_through_the_existing_pipeline() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();

    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": [post_id] } }),
    );

    assert_eq!(result["selected"], 1);
    assert_eq!(result["resolved"], 1);
    assert_eq!(result["tasksCreated"], 1);
    assert_eq!(result["skipped"], 0);
    assert_eq!(result["failed"], 0);

    let outcomes = all_outcomes(&result);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"], "taskCreated");
    assert_eq!(outcomes[0]["postId"], post_id);
    assert!(outcomes[0]["taskId"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(outcomes[0]["title"].as_str().is_some_and(|t| !t.is_empty()));

    // The task is real, and it went through the shared queue.
    let downloads = harness.ok("list_downloads", Value::Null);
    let tasks = downloads.as_array().expect("downloads");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], outcomes[0]["taskId"]);
    assert_eq!(tasks[0]["postId"], post_id);

    // And the listing now knows the post has a download behind it. Which state
    // it is in depends on the transfer worker, which really runs here and
    // cannot fetch from a mock transport — so the claim is that local state is
    // attached at all, not what a race produced.
    let refreshed = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    assert!(
        refreshed["posts"][0]["downloadState"].is_string(),
        "the listing must reflect the task that was just created"
    );
    assert_ne!(refreshed["posts"][0]["status"], "new");
}

#[test]
fn a_post_already_in_flight_is_shown_as_downloading() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();
    seed_download(&harness, &post_id, DownloadState::Downloading, None);

    let refreshed = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    assert_eq!(refreshed["posts"][0]["status"], "downloading");
    assert_eq!(refreshed["posts"][0]["downloadState"], "downloading");
    assert!(refreshed["posts"][0]["activeTaskId"].is_string());
}

#[test]
fn a_post_whose_download_failed_is_shown_as_failed() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();
    seed_download(&harness, &post_id, DownloadState::Failed, None);

    let refreshed = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    assert_eq!(refreshed["posts"][0]["status"], "failed");
    assert!(refreshed["posts"][0]["note"]
        .as_str()
        .unwrap_or_default()
        .contains("retried"));
}

#[test]
fn submit_profile_downloads_keeps_going_when_one_post_cannot_be_used() {
    // The second listed post has no stubbed source, so its media cannot be
    // resolved. That must fail on its own and leave the first one alone.
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let posts = page["posts"].as_array().expect("posts");
    let ids: Vec<String> = posts
        .iter()
        .map(|entry| entry["post"]["id"].as_str().expect("id").to_string())
        .collect();

    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": ids } }),
    );

    assert_eq!(result["selected"], 2);
    assert_eq!(result["tasksCreated"], 1);
    assert_eq!(result["failed"], 1);
    // Every selected post lands in exactly one bucket.
    assert_eq!(
        result["selected"].as_i64(),
        Some(
            result["tasksCreated"].as_i64().unwrap_or(0)
                + result["skipped"].as_i64().unwrap_or(0)
                + result["failed"].as_i64().unwrap_or(0)
                + result["unsupported"].as_i64().unwrap_or(0)
        ),
        "the summary must account for the whole selection: {result}"
    );

    let outcomes = all_outcomes(&result);
    let created = outcomes
        .iter()
        .find(|o| o["outcome"] == "taskCreated")
        .expect("the usable post was still submitted");
    assert_eq!(created["platformPostId"], LISTED_VIDEO_ID);
    let failed = outcomes
        .iter()
        .find(|o| o["outcome"] == "failed")
        .expect("the unusable post is reported");
    assert_eq!(failed["platformPostId"], LISTED_VIDEO_ID_2);
    // The failure carries something the user can act on.
    assert!(failed["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert!(failed["taskId"].is_null());
}

/// A carousel post with `count` pictures, stored and ready to be submitted.
///
/// Inserted rather than resolved: the branch under test is what happens once a
/// post's media is already known — a carousel queues every picture, not just the
/// first — and driving the resolver there would test the resolver instead.
fn store_carousel(harness: &Harness, count: usize) -> (String, Vec<String>) {
    let mut post = Post::new(
        Platform::Douyin,
        "7629996475679029850",
        MediaType::Carousel,
        "https://www.douyin.com/note/7629996475679029850",
    );
    post.caption = Some("a carousel".into());
    post.media_items = (0..count)
        .map(|index| {
            MediaItem::new(
                post.id.clone(),
                MediaType::Image,
                format!("https://p3-cdn.douyinpic.com/slide-{index}.jpg"),
            )
        })
        .collect();

    // `upsert` writes the post row and nothing else, so the pictures are
    // persisted beside it — the same two steps the resolver's own persistence
    // takes.
    let post_id = harness
        .core()
        .database()
        .with_conn(|conn| {
            let id = posts_repo::upsert(conn, &post)?;
            media_repo::replace_items_for_post(conn, &id, &post.media_items)?;
            Ok(id)
        })
        .expect("the carousel is stored");
    let media_ids = harness
        .core()
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .expect("read back")
        .expect("the post is there")
        .media_items
        .iter()
        .map(|item| item.id.clone())
        .collect();
    (post_id, media_ids)
}

#[test]
fn a_profile_carousel_queues_every_picture_not_just_the_first() {
    // The reported defect: one item was chosen per post, so selecting a
    // fifteen-picture carousel downloaded one picture and called the post
    // handled. What is asserted here is that every picture of the post becomes
    // its own task, and that the post still produces exactly *one* result.
    let harness = default_profile_harness();
    let (post_id, media_ids) = store_carousel(&harness, 3);
    assert_eq!(media_ids.len(), 3, "three pictures were stored");

    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": [post_id] } }),
    );

    assert_eq!(result["selected"], 1, "one post was chosen");
    assert_eq!(
        result["tasksCreated"], 3,
        "every picture of the carousel was queued, not just the first: {result}"
    );

    let posts = result["posts"].as_array().expect("posts");
    assert_eq!(posts.len(), 1, "one logical result for the one selected post");
    assert_eq!(posts[0]["media"].as_array().expect("media").len(), 3);

    // One task per picture, each naming a different one of them.
    let queue = harness.ok("get_download_queue", Value::Null);
    let queued: Vec<String> = queue["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter_map(|task| task["mediaItemId"].as_str().map(str::to_string))
        .collect();
    for media_id in &media_ids {
        assert!(
            queued.contains(media_id),
            "no task was queued for picture {media_id}: {queued:?}"
        );
    }
}

#[test]
fn a_narrowed_carousel_queues_only_the_pictures_chosen() {
    // The point of the chooser: four pictures found, two wanted. Queuing all
    // four would be disobedient; queuing the two out of the post's order would
    // renumber them, because a slide's filename follows its place in the post.
    let harness = default_profile_harness();
    let (post_id, media_ids) = store_carousel(&harness, 4);

    let mut selected = serde_json::Map::new();
    selected.insert(post_id.clone(), json!([media_ids[3], media_ids[1]]));

    let result = harness.ok(
        "submit_profile_downloads",
        json!({
            "input": {
                "postIds": [post_id.clone()],
                "selectedItems": Value::Object(selected),
            }
        }),
    );

    assert_eq!(result["selected"], 1);
    assert_eq!(result["tasksCreated"], 2, "only the pictures chosen: {result}");

    // Which pictures, read from the tasks: `mediaItemId` is the association the
    // store, the History screen and a per-slide retry all work from. The order
    // they are queued in is the post's own — `wanted` is built by walking the
    // post's items rather than the list the caller sent — which is what keeps a
    // slide's number stable when a checkbox list is reordered.
    let queue = harness.ok("get_download_queue", Value::Null);
    let mut queued: Vec<&str> = queue["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter_map(|task| task["mediaItemId"].as_str())
        .collect();
    queued.sort_unstable();
    let mut expected = vec![media_ids[1].as_str(), media_ids[3].as_str()];
    expected.sort_unstable();
    assert_eq!(queued, expected, "exactly the pictures chosen were queued: {result}");
}

#[test]
fn a_carousel_emptied_by_the_chooser_is_not_downloaded_in_full() {
    // The dangerous reading of an empty selection is "no opinion, so take
    // everything". It is an opinion, and it is "none of them".
    let harness = default_profile_harness();
    let (post_id, _media_ids) = store_carousel(&harness, 3);

    let mut selected = serde_json::Map::new();
    selected.insert(post_id.clone(), json!([]));

    let result = harness.ok(
        "submit_profile_downloads",
        json!({
            "input": {
                "postIds": [post_id.clone()],
                "selectedItems": Value::Object(selected),
            }
        }),
    );

    assert_eq!(
        result["tasksCreated"], 0,
        "nothing was chosen, so nothing may be queued: {result}"
    );
}

#[test]
fn submit_profile_downloads_buckets_a_post_with_nothing_downloadable() {
    // A stored post whose link is not one this build can resolve has nothing
    // to download. That is not a failure of the batch, and not a task either —
    // it is its own outcome, and the post beside it is still submitted.
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let posts = page["posts"].as_array().expect("posts");
    let ids: Vec<String> = posts
        .iter()
        .map(|entry| entry["post"]["id"].as_str().expect("id").to_string())
        .collect();

    harness
        .core()
        .database()
        .with_conn(|conn| {
            conn.execute(
                "UPDATE posts SET url = 'https://example.com/video/7281234567890123457' WHERE id = ?1",
                [ids[1].clone()],
            )?;
            Ok(())
        })
        .expect("point the second post somewhere unresolvable");

    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": ids } }),
    );

    assert_eq!(result["selected"], 2);
    assert_eq!(result["tasksCreated"], 1, "the usable post is still submitted");
    assert_eq!(result["unsupported"], 1);
    assert_eq!(result["failed"], 0);
    assert_eq!(
        result["selected"].as_i64(),
        Some(
            result["tasksCreated"].as_i64().unwrap_or(0)
                + result["skipped"].as_i64().unwrap_or(0)
                + result["failed"].as_i64().unwrap_or(0)
                + result["unsupported"].as_i64().unwrap_or(0)
        ),
        "the summary must account for the whole selection: {result}"
    );

    let entry = all_outcomes(&result)
        .into_iter()
        .find(|o| o["outcome"] == "unsupported")
        .expect("the unresolvable post is reported as unsupported");
    assert!(entry["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert!(entry["taskId"].is_null());
}

#[test]
fn a_failed_next_page_leaves_the_posts_already_loaded_alone() {
    // Each service-level listing costs two renders — the creator is resolved
    // before the grid is read — and an empty grid is looked at again, twice, so
    // a genuinely refused page two is rendered empty three times. A source that
    // refuses is asked the bounded number of times and then believed.
    let page_one = profile_page(
        json!([listed_card(LISTED_VIDEO_ID), listed_card(LISTED_VIDEO_ID_2)]),
        json!(2),
    );
    // The second page is refused by the source, the way a real one is.
    let refused = profile_page(json!([]), json!(2));

    let renderer = Arc::new(
        MockPageRenderer::returning(Vec::new())
            .with_payload(page_one.clone())
            .with_payload_sequence(vec![
                page_one.clone(), // page one → resolve the creator
                page_one.clone(), // page one → read the grid
                page_one.clone(), // page two → resolve the creator
                refused.clone(),  // page two → read the grid: refused
                refused.clone(),  // page two → the second look: refused again
                refused,          // page two → the third look: refused again
            ]),
    );
    let harness = Harness::start_with(Some(MockHttpClient::new()), Some(renderer));

    let first = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 1 }),
    );
    assert_eq!(first["posts"].as_array().expect("posts").len(), 1);
    assert_eq!(first["hasMore"], true);
    assert_eq!(
        first["loadedTotal"], 1,
        "only the page that was asked for is stored"
    );
    let cursor = first["nextCursor"].as_str().expect("a cursor").to_string();

    // The next page is refused — and that is reported, not hidden.
    let err = harness.err(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": cursor, "limit": 1 }),
    );
    assert_eq!(err["kind"], "unavailable_content");

    // What was already loaded is untouched: still stored, still listed.
    let creator_id = first["creator"]["id"].as_str().expect("creator id").to_string();
    let stored = harness
        .core()
        .database()
        .with_conn(|conn| posts_repo::count_by_creator(conn, &creator_id))
        .expect("count");
    assert_eq!(stored, 1, "a refused page must not erase what was loaded");

    let again = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    assert_eq!(
        again["posts"].as_array().expect("posts").len(),
        2,
        "the posts remain available after the failure"
    );
}

#[test]
fn submit_profile_downloads_refuses_an_empty_selection() {
    let harness = Harness::start();
    let err = harness.err(
        "submit_profile_downloads",
        json!({ "input": { "postIds": [] } }),
    );
    assert_eq!(err["kind"], "validation");
    assert!(err["message"].as_str().unwrap_or_default().contains("at least one"));
}

#[test]
fn submit_profile_downloads_reports_a_post_that_was_never_analyzed() {
    let harness = Harness::start();
    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": ["not-a-real-id"] } }),
    );

    assert_eq!(result["selected"], 1);
    assert_eq!(result["tasksCreated"], 0);
    assert_eq!(result["failed"], 1);
    assert_eq!(result["posts"][0]["media"][0]["outcome"], "failed");
}

#[test]
fn submit_profile_downloads_does_not_queue_a_second_task_for_work_already_in_flight() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();

    // A transfer for this media is already under way (or queued, or paused).
    seed_download(&harness, &post_id, DownloadState::Downloading, None);

    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": [post_id] } }),
    );
    assert_eq!(result["tasksCreated"], 0);
    assert_eq!(result["skipped"], 1);
    assert_eq!(result["posts"][0]["media"][0]["outcome"], "alreadyDownloading");

    // Still exactly the one task that was already there.
    let tasks = harness.ok("list_downloads", Value::Null);
    assert_eq!(tasks.as_array().expect("downloads").len(), 1);
}

#[test]
fn submit_profile_downloads_does_not_queue_a_second_completed_task() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();

    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("clip.mp4");
    std::fs::write(&file, b"already here").expect("write");
    seed_download(&harness, &post_id, DownloadState::Completed, Some(&file));

    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": [post_id] } }),
    );
    assert_eq!(result["tasksCreated"], 0);
    assert_eq!(result["skipped"], 1);
    assert_eq!(result["posts"][0]["media"][0]["outcome"], "alreadyDownloaded");

    let tasks = harness.ok("list_downloads", Value::Null);
    assert_eq!(tasks.as_array().expect("downloads").len(), 1);
}

#[test]
fn a_post_that_was_already_downloaded_is_identified_from_local_data() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();

    // Stand in for a completed download whose file is still on disk.
    let dir = tempfile::tempdir().expect("temp dir");
    let file = dir.path().join("clip.mp4");
    std::fs::write(&file, b"already here").expect("write");
    seed_download(&harness, &post_id, DownloadState::Completed, Some(&file));

    let refreshed = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    assert_eq!(refreshed["posts"][0]["status"], "downloaded");
    assert_eq!(refreshed["posts"][0]["downloadState"], "completed");

    // And submitting it again is passed over rather than re-fetched.
    let result = harness.ok(
        "submit_profile_downloads",
        json!({ "input": { "postIds": [post_id] } }),
    );
    assert_eq!(result["tasksCreated"], 0);
    assert_eq!(result["skipped"], 1);
    assert_eq!(result["posts"][0]["media"][0]["outcome"], "alreadyDownloaded");
}

#[test]
fn a_download_whose_file_is_gone_is_offered_again_rather_than_called_downloaded() {
    let harness = default_profile_harness();
    let page = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    let post_id = page["posts"][0]["post"]["id"]
        .as_str()
        .expect("post id")
        .to_string();

    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("deleted.mp4");
    seed_download(&harness, &post_id, DownloadState::Completed, Some(&missing));

    let refreshed = harness.ok(
        "list_profile_posts",
        json!({ "url": profile_url(), "cursor": null, "limit": 18 }),
    );
    assert_eq!(
        refreshed["posts"][0]["status"], "ready",
        "a row is not a file, so this is not a download"
    );
    assert!(refreshed["posts"][0]["note"]
        .as_str()
        .unwrap_or_default()
        .contains("no longer on disk"));
}

// --- the download queue over IPC --------------------------------------------
//
// Every queue operation the Downloads screen offers, driven through the real
// command bridge. The task is created the way the application creates one — by
// analyzing a link and creating a download from it — so what is exercised is
// the path the UI actually takes, not a shortcut past it.

/// The media URL the stubbed share page resolves to.
const MEDIA_URL: &str = "https://v3-cdn.douyin.com/aweme/v1/play/?video_id=clean";

/// A harness whose media transfer takes a moment, so a task can be caught
/// while it is running.
fn slow_video_harness() -> Harness {
    Harness::start_with_client(Some(
        MockHttpClient::new()
            .with_body(
                "https://www.iesdouyin.com/share/video/7281234567890123456/",
                share_page_with_one_video(),
            )
            .with_download(MEDIA_URL, vec![7u8; 4096])
            .with_delay(std::time::Duration::from_millis(400)),
    ))
}

/// Create one download through the application's own path and return its id.
fn create_one_download(harness: &Harness) -> String {
    let analyzed = harness.ok(
        "analyze_url",
        json!({ "url": "https://www.douyin.com/video/7281234567890123456" }),
    );
    let post_id = analyzed["post"]["id"].as_str().expect("post id").to_string();
    let media_id = analyzed["media"][0]["id"]
        .as_str()
        .expect("media id")
        .to_string();

    let task = harness.ok(
        "create_download_task",
        json!({ "input": { "postId": post_id, "mediaItemId": media_id, "variantId": null } }),
    );
    task["id"].as_str().expect("task id").to_string()
}

#[test]
fn the_queue_snapshot_carries_the_tasks_and_their_aggregate() {
    let harness = slow_video_harness();
    let task_id = create_one_download(&harness);

    let queue = harness.ok("get_download_queue", Value::Null);
    let tasks = queue["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], task_id.as_str());

    // The aggregate is over the same list, so the two cannot disagree.
    assert_eq!(queue["summary"]["total"], 1);
    assert!(queue["summary"]["activeDownloadedBytes"].is_u64());
    assert!(queue["summary"]["completedBytes"].is_u64());
    assert!(queue["summary"]["activeWithoutSize"].is_u64());
}

#[test]
fn a_task_can_be_fetched_on_its_own() {
    let harness = slow_video_harness();
    let task_id = create_one_download(&harness);

    let task = harness.ok("get_download", json!({ "taskId": task_id }));
    assert_eq!(task["id"], task_id.as_str());
    assert_eq!(task["mediaType"], "video");

    // An id that is not in the queue is `null`, not an error: "gone" is a
    // normal answer to "what is this now".
    let missing = harness.ok("get_download", json!({ "taskId": "not-a-task" }));
    assert!(missing.is_null());
}

#[test]
fn pausing_and_resuming_over_ipc_follows_the_real_task_state() {
    let harness = slow_video_harness();
    let task_id = create_one_download(&harness);

    let paused = harness.ok("pause_download", json!({ "taskId": task_id }));
    assert_eq!(paused["state"], "paused");

    let resumed = harness.ok("resume_download", json!({ "taskId": task_id }));
    assert!(
        matches!(resumed["state"].as_str(), Some("queued") | Some("downloading")),
        "a resumed task is queued or already running, got {}",
        resumed["state"]
    );
}

#[test]
fn an_impossible_transition_is_a_typed_error_not_a_state_change() {
    let harness = slow_video_harness();
    let task_id = create_one_download(&harness);
    harness.ok("cancel_download", json!({ "taskId": task_id }));

    // A cancelled task has no resume to give.
    let err = harness.err("resume_download", json!({ "taskId": task_id }));
    assert_eq!(err["kind"], "validation");
    assert!(!err["message"].as_str().unwrap_or_default().is_empty());

    // And the task is still cancelled: a rejected command changes nothing.
    let task = harness.ok("get_download", json!({ "taskId": task_id }));
    assert_eq!(task["state"], "cancelled");
}

#[test]
fn removing_an_entry_over_ipc_keeps_its_history() {
    let harness = slow_video_harness();
    let task_id = create_one_download(&harness);
    harness.ok("cancel_download", json!({ "taskId": task_id }));

    let before = harness.ok("get_history_count", Value::Null);
    assert_eq!(before, 1, "a cancelled download is still history");

    assert_eq!(harness.ok("remove_download", json!({ "taskId": task_id })), true);
    assert!(harness.ok("get_download", json!({ "taskId": task_id })).is_null());

    // The queue entry is gone; what it recorded is not.
    assert_eq!(harness.ok("get_history_count", Value::Null), 1);
    // Removing it again reports that there was nothing to remove.
    assert_eq!(harness.ok("remove_download", json!({ "taskId": task_id })), false);
}

#[test]
fn failing_downloads_can_be_retried_in_bulk_over_ipc() {
    // No media is served, so the download fails — permanently, so it stays put.
    let harness = Harness::start_with_client(Some(MockHttpClient::new().with_body(
        "https://www.iesdouyin.com/share/video/7281234567890123456/",
        share_page_with_one_video(),
    )));
    let task_id = create_one_download(&harness);

    // Wait for it to reach its terminal state before looking at it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let task = harness.ok("get_download", json!({ "taskId": task_id }));
        if task["state"] == "failed" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the download never failed");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(harness.ok("get_download_summary", Value::Null)["failed"], 1);

    let retried = harness.ok("retry_failed_downloads", Value::Null);
    let retried = retried.as_array().expect("re-queued tasks");
    assert_eq!(retried.len(), 1, "the failed task was re-queued");
    assert_eq!(retried[0]["id"], task_id.as_str());
    assert_eq!(retried[0]["attempts"], 0, "a retry starts its own counting");
}

#[test]
fn clearing_completed_entries_over_ipc_reports_what_it_removed() {
    let harness = slow_video_harness();
    let task_id = create_one_download(&harness);
    harness.ok("cancel_download", json!({ "taskId": task_id }));

    // Nothing is completed, so there is nothing to clear.
    assert_eq!(harness.ok("clear_completed_downloads", Value::Null), 0);
    // The cancelled entry is not "completed", so clearing leaves it alone.
    assert!(!harness.ok("get_download", json!({ "taskId": task_id })).is_null());
    assert_eq!(harness.ok("get_history_count", Value::Null), 1);
}

#[test]
fn a_queue_command_for_an_unknown_task_is_a_typed_error() {
    let harness = Harness::start();
    for command in ["pause_download", "resume_download", "cancel_download", "retry_download"] {
        let err = harness.err(command, json!({ "taskId": "no-such-task" }));
        assert_eq!(err["kind"], "not_found", "{command} should report not-found");
    }
}

// --- library ----------------------------------------------------------------
//
// The Library reads what the other workflows wrote, so these tests seed a post
// and a download directly and then drive the three commands the screen uses.
// The query itself — reconciliation, filters, paging, export — is covered in
// depth by `tests/library.rs`; what is checked here is that it survives the IPC
// boundary with the shape the frontend expects.

/// Point the download folder at the harness's own temporary directory.
///
/// Without this the export tests would write into the machine's real Downloads
/// folder, which is a side effect no test should have.
fn redirect_downloads(harness: &Harness) -> std::path::PathBuf {
    // The harness has already done this in `start_with`, so this only reports
    // where the test's own download directory is. Kept as a named call because
    // the tests that use it read better for saying so.
    let root = std::path::PathBuf::from(harness.core().settings().current().download_directory);
    support::assert_isolated_destination("the export tests", &root);
    root
}

/// A stored post with a caption, for the library to find.
fn seed_library_post(
    harness: &Harness,
    platform_post_id: &str,
    caption: &str,
    media_type: douyin_downloader_lib::models::MediaType,
) -> String {
    use douyin_downloader_lib::database::repositories::posts as posts_repo;
    use douyin_downloader_lib::models::{Platform, Post};

    let mut post = Post::new(
        Platform::Douyin,
        platform_post_id,
        media_type,
        format!("https://www.douyin.com/video/{platform_post_id}"),
    );
    post.caption = Some(caption.to_string());
    // Dated, so a publication-date filter has something to compare against.
    post.published_at = Some(douyin_downloader_lib::models::now());
    harness
        .core()
        .database()
        .with_conn(|conn| posts_repo::upsert(conn, &post))
        .expect("seed a post")
}

#[test]
fn query_library_round_trips_over_ipc() {
    let harness = Harness::start();
    let root = redirect_downloads(&harness);
    let post_id = seed_library_post(
        &harness,
        "7300000000000000001",
        "Sunset over the sea",
        douyin_downloader_lib::models::MediaType::Video,
    );
    let file = root.join("sunset.mp4");
    std::fs::write(&file, b"bytes").expect("write");
    seed_download(&harness, &post_id, DownloadState::Completed, Some(&file));

    let page = harness.ok("query_library", json!({ "query": { "view": "all" } }));

    assert_eq!(page["total"], 1);
    assert_eq!(page["counts"]["all"], 1);
    assert_eq!(page["counts"]["videos"], 1);
    assert_eq!(page["counts"]["downloaded"], 1);
    assert_eq!(page["counts"]["new"], 0);
    assert_eq!(page["limit"], 50);
    assert_eq!(page["offset"], 0);
    assert!(page["scanned"].as_u64().unwrap() >= 1);

    let entry = &page["entries"][0];
    assert_eq!(entry["post"]["caption"], "Sunset over the sea");
    assert_eq!(entry["post"]["platformPostId"], "7300000000000000001");
    assert_eq!(entry["status"], "downloaded");
    assert_eq!(entry["downloadState"], "completed");
    // Three answers rather than a boolean, and the word the window already uses
    // for the same condition on the Downloads screen.
    assert_eq!(entry["fileState"], "present");
    assert_eq!(entry["fileStateReason"], serde_json::Value::Null);
    assert_eq!(entry["mediaItemCount"], 1);
    assert_eq!(entry["downloadedItemCount"], 1);
}

#[test]
fn the_library_accepts_the_filters_and_search_the_frontend_sends() {
    let harness = Harness::start();
    redirect_downloads(&harness);
    seed_library_post(
        &harness,
        "7300000000000000002",
        "A quiet harbour",
        douyin_downloader_lib::models::MediaType::Video,
    );
    seed_library_post(
        &harness,
        "7300000000000000003",
        "Something else entirely",
        douyin_downloader_lib::models::MediaType::Image,
    );

    let page = harness.ok(
        "query_library",
        json!({
            "query": {
                "search": "harbour",
                "sort": "title_asc",
                "limit": 10,
                "filter": {
                    "mediaTypes": ["video"],
                    "statuses": ["new"],
                    "publishedAfter": "2026-01-01",
                    "minDurationSeconds": 5,
                    "minWidth": 640
                }
            }
        }),
    );

    assert_eq!(page["total"], 1);
    assert_eq!(page["entries"][0]["post"]["caption"], "A quiet harbour");
    assert_eq!(page["entries"][0]["status"], "new");
}

#[test]
fn a_library_query_with_an_unparseable_date_is_a_typed_validation_error() {
    let harness = Harness::start();
    let err = harness.err(
        "query_library",
        json!({ "query": { "filter": { "publishedAfter": "11/10/2026" } } }),
    );
    assert_eq!(
        err["kind"], "validation",
        "a bad date must be refused, not silently ignored: {err}"
    );
}

#[test]
fn list_library_creators_returns_the_creators_that_have_posts() {
    use douyin_downloader_lib::database::repositories::{
        creators as creators_repo, posts as posts_repo,
    };
    use douyin_downloader_lib::models::{Creator, MediaType, Platform, Post};

    let harness = Harness::start();
    let creator = Creator::new(
        Platform::Douyin,
        "MS4wLjABAAAAlibrary",
        "Sunset Studio",
        "https://www.douyin.com/user/MS4wLjABAAAAlibrary",
    );
    harness
        .core()
        .database()
        .with_conn(|conn| {
            let id = creators_repo::upsert(conn, &creator)?;
            let mut post = Post::new(
                Platform::Douyin,
                "7300000000000000004",
                MediaType::Video,
                "https://www.douyin.com/video/7300000000000000004",
            );
            post.creator_id = Some(id);
            posts_repo::upsert(conn, &post)?;
            Ok(())
        })
        .expect("seed a creator with a post");

    let creators = harness.ok("list_library_creators", Value::Null);
    let creators = creators.as_array().expect("an array");
    assert_eq!(creators.len(), 1);
    assert_eq!(creators[0]["name"], "Sunset Studio");
    assert!(!creators[0]["id"].as_str().unwrap().is_empty());
}

#[test]
fn export_library_writes_a_file_and_says_where_it_went() {
    let harness = Harness::start();
    let root = redirect_downloads(&harness);
    seed_library_post(
        &harness,
        "7300000000000000005",
        "Exported caption",
        douyin_downloader_lib::models::MediaType::Video,
    );

    let result = harness.ok(
        "export_library",
        json!({ "query": { "view": "all" }, "format": "json", "scope": "matching" }),
    );

    assert_eq!(result["records"], 1);
    assert_eq!(result["format"], "json");
    assert_eq!(result["scope"], "matching");
    assert!(result["bytes"].as_u64().unwrap() > 0);

    let path = std::path::PathBuf::from(result["path"].as_str().expect("a path"));
    assert!(path.is_file(), "the export must exist at the path it reported");
    // Inside the folder this application is allowed to write in. Both sides are
    // canonicalized because Windows hands back a `\?\`-prefixed form that a
    // plain path does not literally start with.
    let canonical_root = std::fs::canonicalize(&root).expect("canonical root");
    let canonical_path = std::fs::canonicalize(&path).expect("canonical export");
    assert!(
        canonical_path.starts_with(&canonical_root),
        "{canonical_path:?} must be inside {canonical_root:?}"
    );

    let body = std::fs::read_to_string(&path).expect("read the export");
    let rows: Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(rows[0]["caption"], "Exported caption");
}

/// The command harness must never hand a download to the machine's own folder.
///
/// This is the test that would have caught the incident. `Harness::start` boots
/// the real application over a throwaway data directory; the download directory
/// is a *different* setting, and when it is left alone it resolves to the
/// machine's `Downloads\Douyin`. That is where fifteen mock downloads and ten
/// write-probe files were found, sitting beside real downloads.
#[test]
fn the_command_harness_downloads_only_into_its_own_workspace() {
    let harness = Harness::start();
    let workspace = harness._temp.path();

    let download_dir = std::path::PathBuf::from(
        harness.core().settings().current().download_directory,
    );

    assert!(
        download_dir.starts_with(workspace),
        "the harness is downloading into {download_dir:?}, which is not inside its own \
         workspace {workspace:?}"
    );
    // And the shared guard agrees, which is what every other harness asserts.
    support::assert_isolated_destination("the command harness", &download_dir);

    // The two directories a test could confuse are genuinely different here, so
    // the check above is not passing by accident.
    let (real_download, real_data) = support::real_paths();
    assert_ne!(download_dir, real_download);
    assert_ne!(workspace, real_data);
}

