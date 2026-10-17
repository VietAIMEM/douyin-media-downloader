//! End-to-end download pipeline over a real socket and a real filesystem.
//!
//! The unit tests drive the transfer against a mock client; this suite drives
//! the *real* one — `ReqwestHttpClient`, `LocalStorageManager`, SQLite, and the
//! transfer worker — against a throwaway HTTP server on loopback. What it
//! proves is the part that cannot be mocked away: bytes actually leave a socket,
//! land in the right file, under the right directory, and are recorded.
//!
//! Nothing here depends on Douyin. The server is local and deterministic, so the
//! suite is meaningful offline.

mod support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use douyin_downloader_lib::config::AppPaths;
use douyin_downloader_lib::database::repositories::{
    creators as creators_repo, downloads as downloads_repo, history as history_repo,
    media as media_repo, posts as posts_repo,
};
use douyin_downloader_lib::models::{
    ContentStatus, Creator, DownloadEvent, DownloadRequest, DownloadState, DownloadTask, MediaItem,
    MediaType, Platform, Post,
};
use douyin_downloader_lib::network::renderer::MockPageRenderer;
use douyin_downloader_lib::network::{DestinationPolicy, HttpClient, NoopRenderer, PageRenderer, ReqwestHttpClient};
use douyin_downloader_lib::services::profile::SubmitProfileDownloadsInput;
use douyin_downloader_lib::services::{AppCore, CreateDownloadInput};
use douyin_downloader_lib::storage::filenames::is_within;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve `body` on a loopback port, one request per connection.
async fn serve_bytes(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let _ = socket.read(&mut scratch).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://{address}/media.mp4")
}

/// Serve `total` bytes in small, delayed chunks, so a transfer can be caught
/// mid-flight.
async fn serve_slowly(total: usize, chunk: usize, delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let _ = socket.read(&mut scratch).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nETag: \"slow-v1\"\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                );
                if socket.write_all(header.as_bytes()).await.is_err() {
                    return;
                }
                let block = vec![0x5Au8; chunk];
                let mut sent = 0;
                while sent < total {
                    if socket.write_all(&block).await.is_err() {
                        return;
                    }
                    let _ = socket.flush().await;
                    sent += block.len();
                    tokio::time::sleep(delay).await;
                }
            });
        }
    });

    format!("http://{address}/slow.mp4")
}

/// Boot the core with the real HTTP client and a temp download directory.
///
/// The client is built with [`DestinationPolicy::Any`] because this suite serves
/// its media from a server it started on loopback, which the shipped policy
/// (`PublicOnly`) correctly refuses. Nothing outside a test can select that
/// policy — see `network::policy`.
fn core_with_download_dir(download_dir: &Path) -> (tempfile::TempDir, AppCore) {
    support::assert_isolated_destination("the download-pipeline harness", download_dir);
    let temp = tempfile::tempdir().expect("temp dir");
    let paths = AppPaths::with_data_dir(temp.path().join("data"));
    let http = ReqwestHttpClient::with_policy(30, DestinationPolicy::Any)
        .expect("build a loopback-capable client");
    let core = AppCore::bootstrap_with(
        paths,
        Some(Arc::new(http)),
        Some(Arc::new(NoopRenderer)),
        // A mock prober. These tests serve synthetic byte patterns rather than
        // real containers, so a real `ffprobe` would correctly report that they
        // hold no media and every download here would fail for a reason that has
        // nothing to do with what is under test. That the probe *is* applied to
        // single-track downloads — and that a real one rejects a non-media file
        // — is covered in `downloads::transfer`'s own tests.
        Some(Arc::new(douyin_downloader_lib::media::MockMuxer::producing(true, false))),
    )
    .expect("bootstrap")
    .core;
    core.settings()
        .update(|s| {
            support::assert_isolated_destination("the pipeline test", download_dir);
            s.download_directory = download_dir.to_string_lossy().into_owned();
            s.folder_template = "{creator}/{type}".to_string();
            s.organize_by_creator = true;
            s.organize_by_media_type = true;
            Ok(())
        })
        .expect("settings");
    (temp, core)
}

/// Persist a creator, post, and one media item pointing at `source_url`.
fn seed_post(core: &AppCore, source_url: &str, extension: &str) -> String {
    let mut creator = Creator::new(
        Platform::Douyin,
        "test-creator",
        "Alice",
        "https://www.douyin.com/user/test-creator",
    );
    creator.handle = Some("alice".into());

    let mut post = Post::new(
        Platform::Douyin,
        "7281234567890123456",
        MediaType::Video,
        "https://www.douyin.com/video/7281234567890123456",
    );
    post.caption = Some("A test clip".into());

    let mut item = MediaItem::new("pending", MediaType::Video, source_url);
    item.extension = Some(extension.to_string());
    item.mime_type = Some("video/mp4".to_string());

    core.database()
        .with_conn(|conn| {
            let creator_id = creators_repo::upsert(conn, &creator)?;
            post.creator_id = Some(creator_id);
            let post_id = posts_repo::upsert(conn, &post)?;
            item.post_id = post_id.clone();
            media_repo::replace_items_for_post(conn, &post_id, &[item])?;
            Ok(post_id)
        })
        .expect("seed")
}

/// Every file beneath `root`, recursively. Directories are not counted.
fn collect_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(collect_files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Wait for a terminal event for `task_id`.
async fn wait_for_terminal(
    mut rx: tokio::sync::broadcast::Receiver<DownloadEvent>,
    task_id: &str,
) -> DownloadState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "transfer did not finish in time");
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
            // An event about some other task, or one that does not settle this.
            Ok(Ok(_)) => continue,
            Err(_) => panic!("timed out waiting for a terminal state"),
            // A lagged or closed receiver used to be swallowed by a bare
            // `Ok(_)`, which turned "I missed the event" into "keep waiting" and
            // finally into a timeout that said nothing about why.
            Ok(Err(err)) => panic!("the event stream failed: {err}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_transfer_writes_the_file_records_history_and_completes() {
    // A payload with a recognisable MP4 ftyp box at the start, so the saved file
    // is structurally plausible rather than just "some bytes".
    let mut payload = Vec::from(*b"\x00\x00\x00\x20ftypisom");
    payload.extend(std::iter::repeat(0x11u8).take(64 * 1024));

    let url = serve_bytes(payload.clone()).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = core_with_download_dir(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();

    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");

    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    // --- the task row -------------------------------------------------------
    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, DownloadState::Completed);
    assert_eq!(stored.attempts, 1);

    let saved = stored.destination_path.expect("a saved path");
    let saved_path = Path::new(&saved);

    // --- the file -----------------------------------------------------------
    assert!(saved_path.exists(), "the file must exist at {saved}");
    let bytes = std::fs::read(saved_path).expect("read back");
    assert!(!bytes.is_empty(), "the file must not be empty");
    assert_eq!(bytes, payload, "the file must be exactly what was served");
    assert_eq!(bytes.len(), 64 * 1024 + 12);

    // The extension matches the resolved media.
    assert!(saved.ends_with(".mp4"), "got {saved}");
    // The folder template was honoured.
    assert!(saved.contains("Alice"), "creator folder missing from {saved}");
    assert!(saved.contains("Videos"), "media-type folder missing from {saved}");
    // And the file is inside the configured download directory.
    assert!(is_within(download_dir.path(), saved_path), "escaped {saved}");
    // No partial file was left behind.
    let leftovers: Vec<_> = std::fs::read_dir(saved_path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "a partial file was left behind");

    // --- history ------------------------------------------------------------
    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "exactly one history row");
    let entry = &history[0];
    assert_eq!(entry.status, DownloadState::Completed);
    assert_eq!(entry.task_id.as_deref(), Some(task.id.as_str()));
    assert_eq!(entry.post_id.as_deref(), Some(post_id.as_str()));
    assert_eq!(entry.local_path.as_deref(), Some(saved.as_str()));
    assert_eq!(entry.file_size, Some(bytes.len() as u64));
    assert_eq!(entry.creator_name.as_deref(), Some("Alice"));
    assert!(entry.was_successful());

    // The summary the Downloads screen reads agrees.
    let summary = core.downloads().summary().await.unwrap();
    assert_eq!(summary.completed, 1);
    assert_eq!(summary.total, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_mid_transfer_leaves_an_identifiable_partial_that_resume_continues() {
    // The case that pins the whole design together: pause mid-body, then
    // resume. The partial written by the *transfer itself* — not staged by the
    // test — must carry its identity at header time, or the resume gate would
    // discard the user's progress and restart from zero, defeating the pause.
    let url = serve_slowly(4 * 1024 * 1024, 16 * 1024, Duration::from_millis(10)).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();

    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");

    // Let some bytes land, then pause.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let paused = core.downloads().pause(&task.id).await.expect("pause");
    assert_eq!(paused.state, DownloadState::Paused);

    // Wait for the transfer to actually stop: the state is Paused already, but
    // the worker unwinds asynchronously and writes its metadata on the way out.
    let settled_at = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let summary = core.downloads().summary().await.unwrap();
        if summary.downloading == 0 && summary.queued == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < settled_at,
            "the paused transfer never settled: {summary:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The partial exists — the user's progress survived the pause.
    let settings = core.settings().current();
    let plan = core
        .storage()
        .plan_destination(
            &douyin_downloader_lib::storage::NamingContext {
                post_id: Some(post_id.clone()),
                extension: Some("mp4".into()),
                ..Default::default()
            },
            &settings,
        )
        .expect("plan");
    let stamp = &task.id[..task.id.len().min(8)];
    let partial = std::path::Path::new(&plan.full_path)
        .parent()
        .unwrap()
        .join(format!(".{}.{stamp}.video.part", plan.file_name));

    let bytes_on_disk = std::fs::read(&partial).expect("the partial survived the pause");
    assert!(!bytes_on_disk.is_empty(), "some bytes landed before the pause");

    // And it carries its identity — the whole point of writing it at header
    // time rather than at completion.
    let metadata =
        douyin_downloader_lib::downloads::partial::PartialMetadata::read(&partial)
            .expect("the partial's identity was recorded");
    assert!(
        metadata.authorizes(bytes_on_disk.len() as u64),
        "the metadata authorizes the partial it describes"
    );

    // Resume: the transfer continues from the partial rather than restarting.
    core.downloads().resume(&task.id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let final_bytes = std::fs::read(&saved).expect("read the final file");
    // 4 MiB served from one static server: the file is exactly the resource.
    assert_eq!(final_bytes.len(), 4 * 1024 * 1024);
    assert!(
        !collect_files(download_dir.path())
            .into_iter()
            .any(|path| path.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false)),
        "no partial survives completion"
    );

    // Exactly one history row: a resume is a continuation, not a second
    // download.
    let count = core
        .database()
        .with_conn(|conn| {
            use douyin_downloader_lib::database::repositories::history as history_repo;
            history_repo::count(conn)
        })
        .unwrap();
    assert_eq!(count, 1, "one task, one history row");
}

/// A redirect server: answers every request with `status` pointing at
/// `target`, unless the path is the final one, which answers with `body`.
async fn serve_redirect(status: u16, target: String, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let target = target.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let read = socket.read(&mut scratch).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&scratch[..read]).to_string();
                // The first line holds the path.
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();

                if path == "/final" {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                } else {
                    let header = format!(
                        "HTTP/1.1 {status} Moved\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                }
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://{address}/start")
}

/// The redirect statuses a real origin uses, each of which the client must
/// follow (or refuse at the policy) rather than treat as the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_redirect_status_is_followed_to_the_resource() {
    let body = vec![7u8; 256];
    // A real destination in a throwaway directory. (`NUL` would be a device on
    // a bare path, but `NUL.meta.json` is an ordinary file — and a relative one
    // at that, which dropped a sidecar into the crate root.)
    let scratch = tempfile::tempdir().expect("scratch dir");
    for status in [301u16, 302, 303, 307, 308] {
        // The hop points at a second server, which serves the resource. Both
        // are loopback, which is why the client under test must be built with
        // the permissive policy.
        let final_server = serve_bytes(body.clone()).await;
        let redirect_server = serve_redirect(status, final_server.clone(), body.clone()).await;

        let client = ReqwestHttpClient::with_policy(30, DestinationPolicy::Any)
            .expect("loopback client");
        let destination = scratch.path().join(format!("redirect-{status}.part"));
        let outcome = client
            .download(
                &douyin_downloader_lib::network::TransferRequest::new(
                    redirect_server.as_str(),
                    &destination,
                ),
                &douyin_downloader_lib::network::NoopProgress,
            )
            .await
            .unwrap_or_else(|err| panic!("status {status} must be followed: {err}"));

        assert_eq!(outcome.status, 200, "status {status} reached the resource");
        assert_eq!(outcome.bytes_written, body.len() as u64);
    }
}

/// A redirect that leaves the permitted set is stopped, typed, and not retried.
/// The policy under test here is `PublicOnly` — the shipped one — against a
/// redirect pointing at loopback, which is exactly what a rebinding redirect
/// would do.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redirect_to_a_forbidden_destination_is_refused_without_retrying() {
    // The real test of the *redirect* rule: a policy-refused hop surfaces as a
    // non-retryable error naming the destination, not as an empty body or a
    // retryable connect failure. `PublicOnly` refuses 127.0.0.1, so a client
    // with the shipped policy asked to follow any redirect chain that lands on
    // an explicit loopback IP must fail.
    let client = ReqwestHttpClient::new(30).expect("shipped client");
    let scratch = tempfile::tempdir().expect("scratch dir");
    let destination = scratch.path().join("refused.part");
    // The initial URL is already loopback, so the refusal fires on the first
    // check — the same typed error the redirect policy produces on a later hop.
    let err = client
        .download(
            &douyin_downloader_lib::network::TransferRequest::new(
                "http://127.0.0.1:9/media.mp4",
                &destination,
            ),
            &douyin_downloader_lib::network::NoopProgress,
        )
        .await
        .expect_err("a loopback destination is refused");

    assert_eq!(err.kind(), douyin_downloader_lib::errors::AppErrorKind::Network);
    assert!(!err.is_retryable(), "refusing is a decision, not a failure");
    assert!(
        err.user_message().contains("not a public destination"),
        "the message names what happened: {}",
        err.user_message()
    );
    // And nothing sensitive rode along.
    let detail = err.technical_detail().unwrap_or_default();
    assert!(!detail.contains("token"), "no token in the detail");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_real_transfer_stops_it_and_leaves_no_file() {
    // ~8 MB dribbled out slowly: long enough to cancel deliberately.
    let url = serve_slowly(8 * 1024 * 1024, 16 * 1024, Duration::from_millis(20)).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = core_with_download_dir(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();

    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id,
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");

    // Let some bytes arrive, then cancel.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let cancelled = core.downloads().cancel(&task.id).await.expect("cancel");
    assert_eq!(cancelled.state, DownloadState::Cancelled);

    let state = wait_for_terminal(rx, &task.id).await;
    assert_eq!(state, DownloadState::Cancelled);

    // Nothing reached the destination directory. Directories may exist — the
    // storage layer creates them before starting the transfer — but no *file*
    // may survive.
    //
    // The state change is published by the manager as soon as the stop is
    // requested; the transfer notices it at its next chunk boundary and removes
    // its partial file then. So the guarantee is "cleaned up", not "cleaned up
    // before the state event", and this waits for it rather than racing it.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut files = collect_files(download_dir.path());
    while !files.is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        files = collect_files(download_dir.path());
    }
    assert!(
        files.is_empty(),
        "a cancelled transfer must not leave files behind: {files:?}"
    );

    let entry = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(entry.len(), 1);
    assert_eq!(entry[0].status, DownloadState::Cancelled);
    assert!(entry[0].local_path.is_none());
}

/// A 404 must be recorded as a permanent failure, not retried three times.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_resource_fails_once_and_is_recorded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let _ = socket.read(&mut scratch).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = socket.flush().await;
            });
        }
    });

    let url = format!("http://{address}/gone.mp4");
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = core_with_download_dir(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();

    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id,
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");

    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Failed);

    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    assert_eq!(stored.state, DownloadState::Failed);
    assert_eq!(stored.attempts, 1, "a 404 is permanent and must not be retried");
    assert!(stored.destination_path.is_none());

    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, DownloadState::Failed);
    assert!(history[0].error.is_some());
}

// --- the profile workflow, end to end --------------------------------------
//
// The chain Phase 03 is judged on:
//
//     profile page ─▶ normalized posts ─▶ persisted locally
//          │
//          └─▶ selection ─▶ download task ─▶ real bytes ─▶ file + history
//
// The profile page is supplied by a mock renderer (what Douyin really serves is
// covered by the live suite and by the IPC suite), while the HTTP client, the
// storage manager, SQLite, and the transfer worker are the real ones — so what
// this proves is that a profile selection really does travel the same pipeline
// the Single URL screen uses, and that its outcome is recorded.

const PROFILE_SEC_UID_E2E: &str = "MS4wLjABAAAAe2e";

fn e2e_profile_payload() -> serde_json::Value {
    serde_json::json!({
        "creator": {
            "name": "郭虎",
            "handle": "42299195146",
            "signature": "导演",
            "avatarUrl": "https://p3.douyinpic.com/avatar.jpeg",
            "verified": true,
            "postCount": 2
        },
        "posts": [
            {
                "id": "7091303675863321870",
                "kind": "video",
                "url": "https://www.douyin.com/video/7091303675863321870",
                "caption": "第一条",
                "thumbnailUrl": "https://p3.douyinpic.com/a.jpeg",
                "hasImage": true
            },
            {
                "id": "7473536022312144155",
                "kind": "note",
                "url": "https://www.douyin.com/note/7473536022312144155",
                "caption": "第二条",
                "thumbnailUrl": "https://p3.douyinpic.com/b.jpeg",
                "hasImage": true
            }
        ]
    })
}

/// Boot the core with the *real* HTTP client — so a transfer really happens —
/// and a renderer that serves a fixed profile page, so the listing is
/// deterministic.
fn profile_core(download_dir: &Path, payload: serde_json::Value) -> (tempfile::TempDir, AppCore) {
    let temp = tempfile::tempdir().expect("temp dir");
    let paths = AppPaths::with_data_dir(temp.path().join("data"));
    let renderer: Arc<dyn PageRenderer> =
        Arc::new(MockPageRenderer::returning(Vec::new()).with_payload(payload));

    let core = AppCore::bootstrap_with(
        paths,
        // Permissive, for the same reason as `core_with_download_dir`: the media
        // these tests download comes from a loopback server.
        Some(Arc::new(
            ReqwestHttpClient::with_policy(30, DestinationPolicy::Any)
                .expect("build a loopback-capable client"),
        )),
        Some(renderer),
        Some(Arc::new(douyin_downloader_lib::media::MockMuxer::producing(true, true))),
    )
    .expect("bootstrap")
    .core;

    core.settings()
        .update(|s| {
            support::assert_isolated_destination("the pipeline test", download_dir);
            s.download_directory = download_dir.to_string_lossy().into_owned();
            s.folder_template = "{creator}/{type}".to_string();
            s.organize_by_creator = true;
            s.organize_by_media_type = true;
            Ok(())
        })
        .expect("settings");

    (temp, core)
}

/// Attach a resolved media item to a stored post, as the media-resolution step
/// would once the post's source has been read.
fn attach_media(core: &AppCore, post_id: &str, source_url: &str) {
    core.database()
        .with_conn(|conn| {
            let post = posts_repo::get(conn, post_id)?.expect("post is stored");
            let mut item = MediaItem::new(post.id.clone(), MediaType::Video, source_url);
            item.extension = Some("mp4".into());
            item.mime_type = Some("video/mp4".into());
            item.post_id = post.id.clone();
            media_repo::replace_items_for_post(conn, &post.id, std::slice::from_ref(&item))?;
            // The post is now resolved, so the listing's provisional `new`
            // status is no longer the most that is known about it.
            posts_repo::set_status(conn, &post.id, ContentStatus::Ready)?;
            Ok(())
        })
        .expect("attach media");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_profile_selection_downloads_through_the_shared_pipeline_and_is_recorded() {
    let mut payload_bytes = Vec::from(*b"\x00\x00\x00\x20ftypisom");
    payload_bytes.extend(std::iter::repeat(0x22u8).take(48 * 1024));
    let media_url = serve_bytes(payload_bytes.clone()).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = profile_core(download_dir.path(), e2e_profile_payload());

    // 1. The profile is browsed: the creator is identified and the posts are
    //    normalized and stored locally.
    let page = core
        .list_profile_posts(&format!("https://www.douyin.com/user/{PROFILE_SEC_UID_E2E}"), None, Some(18))
        .await
        .expect("listing");
    assert_eq!(page.creator.display_name, "郭虎");
    assert_eq!(page.creator.post_count, Some(2));
    assert_eq!(page.posts.len(), 2);
    assert_eq!(page.posts[0].post.platform_post_id, "7091303675863321870");
    assert_eq!(page.posts[0].status, ContentStatus::New);

    // Nothing is downloadable about a post straight from a listing.
    assert!(page.posts[0].post.media_items.is_empty());

    // 2. The selected post's media is resolved and stored — the same step the
    //    Single URL screen performs, exercised here against a real server.
    let post_id = page.posts[0].post.id.clone();
    attach_media(&core, &post_id, &media_url);

    // 3. The user selects one post and submits the batch.
    let rx = core.downloads().subscribe();
    let result = core
        .submit_profile_downloads(SubmitProfileDownloadsInput {
            post_ids: vec![post_id.clone()],
            selected_items: Default::default(),
        })
        .await
        .expect("submission");

    assert_eq!(result.selected, 1);
    assert_eq!(result.tasks_created, 1);
    assert_eq!(result.failed, 0);
    let task_id = result.posts[0].media[0]
        .task_id
        .clone()
        .expect("a task was created");

    // 4. The existing pipeline moves the bytes.
    assert_eq!(
        wait_for_terminal(rx, &task_id).await,
        DownloadState::Completed,
        "the profile workflow must drive the ordinary transfer"
    );

    // 5. The file landed under {creator}/{type} and holds exactly what was served.
    let files = collect_files(download_dir.path());
    assert_eq!(files.len(), 1, "one download, one file");
    assert!(is_within(download_dir.path(), &files[0]));
    assert_eq!(std::fs::read(&files[0]).expect("read file"), payload_bytes);
    let relative = files[0]
        .strip_prefix(download_dir.path())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    assert_eq!(relative, "郭虎/Videos/7091303675863321870.mp4");

    // 6. History recorded it against the creator and the post.
    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, DownloadState::Completed);
    assert_eq!(history[0].post_id.as_deref(), Some(post_id.as_str()));
    assert_eq!(history[0].creator_name.as_deref(), Some("郭虎"));
    assert!(history[0].local_path.is_some());

    // 7. Revisiting the profile identifies the post as already downloaded, and
    //    re-submitting it does not fetch it a second time.
    let refreshed = core
        .list_profile_posts(&format!("https://www.douyin.com/user/{PROFILE_SEC_UID_E2E}"), None, Some(18))
        .await
        .expect("second listing");
    assert_eq!(refreshed.posts[0].status, ContentStatus::Downloaded);
    assert_eq!(refreshed.posts[0].download_state, Some(DownloadState::Completed));

    let again = core
        .submit_profile_downloads(SubmitProfileDownloadsInput {
            post_ids: vec![post_id.clone()],
            selected_items: Default::default(),
        })
        .await
        .expect("second submission");
    assert_eq!(again.tasks_created, 0);
    assert_eq!(again.skipped, 1);
    assert_eq!(again.posts[0].media[0].outcome, "alreadyDownloaded");
    assert_eq!(
        core.downloads().list().await.expect("list").len(),
        1,
        "an already-downloaded post must not gain a second task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_local_file_makes_a_post_downloadable_again() {
    let mut payload_bytes = Vec::from(*b"\x00\x00\x00\x20ftypisom");
    payload_bytes.extend(std::iter::repeat(0x33u8).take(16 * 1024));
    let media_url = serve_bytes(payload_bytes).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = profile_core(download_dir.path(), e2e_profile_payload());

    let page = core
        .list_profile_posts(&format!("https://www.douyin.com/user/{PROFILE_SEC_UID_E2E}"), None, Some(18))
        .await
        .expect("listing");
    let post_id = page.posts[0].post.id.clone();
    attach_media(&core, &post_id, &media_url);

    // A completed download whose file is gone: a row is not a file.
    core.database()
        .with_conn(|conn| {
            let mut task = DownloadTask::new(&DownloadRequest::new(&media_url, MediaType::Video));
            let post = posts_repo::get(conn, &post_id)?.expect("post");
            task.post_id = Some(post_id.clone());
            task.media_item_id = Some(post.media_items[0].id.clone());
            task.state = DownloadState::Completed;
            task.destination_path = Some(
                download_dir
                    .path()
                    .join("gone.mp4")
                    .to_string_lossy()
                    .into_owned(),
            );
            downloads_repo::insert(conn, &task)
        })
        .expect("seed");

    let stale = core
        .list_profile_posts(&format!("https://www.douyin.com/user/{PROFILE_SEC_UID_E2E}"), None, Some(18))
        .await
        .expect("listing");
    assert_eq!(
        stale.posts[0].status,
        ContentStatus::Ready,
        "a completed row whose file is gone must not read as downloaded"
    );

    let rx = core.downloads().subscribe();
    let result = core
        .submit_profile_downloads(SubmitProfileDownloadsInput {
            post_ids: vec![post_id],
            selected_items: Default::default(),
        })
        .await
        .expect("submission");
    assert_eq!(result.tasks_created, 1);
    let task_id = result.posts[0].media[0].task_id.clone().expect("task");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    let files = collect_files(download_dir.path());
    assert!(
        files.iter().any(|path| path.ends_with("7091303675863321870.mp4")),
        "the re-fetch must produce the file: {files:?}"
    );
}

// --- resume integrity, over a real socket -------------------------------------
//
// The unit tests prove the rules; these prove them against a real
// `ReqwestHttpClient` talking to a real server on loopback, with the file's
// bytes checked at the end. The point that cannot be tested with a mock: the
// `If-Range` header actually going out on the wire, and the client's own
// validator comparison actually running.

/// How long a resume-test server waits before answering.
///
/// Not decoration: these tests create a task and pause it immediately, so that
/// they can stage the partial the transfer would have left. Against a server
/// that answers instantly the transfer can finish first, and the test then has
/// no pausable task to stage anything for — a race in the *test*, not in the
/// product. A delay wide enough that no loopback transfer can complete inside it
/// removes it.
const ANSWER_DELAY: Duration = Duration::from_millis(300);

/// A server that answers `Range` requests with real `206` semantics and honours
/// `If-Range` — or ignores it, when asked to.
///
/// `versions` maps an ETag to a body. The current version is the last one; the
/// server reports its ETag on every response. When `honour_if_range` is false,
/// the server answers `206` no matter what the condition said, which is the
/// origin behaviour the client's belt-and-braces comparison exists to catch.
async fn serve_versioned_resource(
    versions: Vec<(&'static str, Vec<u8>)>,
    honour_if_range: bool,
) -> String {
    use std::sync::Mutex;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let current = Arc::new(Mutex::new(versions.len() - 1));
    let bodies: Arc<Vec<(String, Vec<u8>)>> = Arc::new(
        versions
            .into_iter()
            .map(|(etag, body)| (etag.to_string(), body))
            .collect(),
    );

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let current = Arc::clone(&current);
            let bodies = Arc::clone(&bodies);
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let read = socket.read(&mut scratch).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&scratch[..read]).to_string();

                let offset = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split_once('='))
                    .and_then(|(_, rest)| rest.trim().trim_end_matches('-').parse::<usize>().ok())
                    .unwrap_or(0);
                let if_range = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("if-range:"))
                    .map(|line| line.split_once(':').unwrap().1.trim().to_string());

                let index = *current.lock().unwrap();
                let (etag, body) = &bodies[index];
                let condition_ok = if_range.as_deref() != Some(etag.as_str());
                let partial_ok = honour_if_range || if_range.is_none();

                let (status, slice) = if offset > 0 && offset < body.len() && partial_ok {
                    // The condition is only consulted by a server that honours
                    // it; a `206` to an `If-Range` request is the server's own
                    // statement that the entity is unchanged.
                    if honour_if_range && !condition_ok {
                        ("200 OK", &body[..])
                    } else {
                        (
                            "206 Partial Content",
                            &body[offset..],
                        )
                    }
                } else {
                    ("200 OK", &body[..])
                };

                tokio::time::sleep(ANSWER_DELAY).await;
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\nETag: {etag}\r\n\
                     Content-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(slice).await;
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://{address}/media.mp4")
}

/// The loopback-capable core, reused from the helpers above.
fn resume_core(download_dir: &Path) -> (tempfile::TempDir, AppCore) {
    let temp = tempfile::tempdir().expect("temp dir");
    let core = core_over(&temp.path().join("data"), download_dir);
    (temp, core)
}

/// Boot a core over a data directory the caller owns.
///
/// Named separately so a test can stop one core and start another over the same
/// database and the same download folder — which is what a restart is, and the
/// only way to find out whether the state that crosses it is enough.
fn core_over(data_dir: &Path, download_dir: &Path) -> AppCore {
    support::assert_isolated_destination("the restart harness", download_dir);
    let paths = AppPaths::with_data_dir(data_dir.to_path_buf());
    let http = ReqwestHttpClient::with_policy(30, DestinationPolicy::Any)
        .expect("build a loopback-capable client");
    let core = AppCore::bootstrap_with(
        paths,
        Some(Arc::new(http)),
        Some(Arc::new(NoopRenderer)),
        Some(Arc::new(douyin_downloader_lib::media::MockMuxer::producing(true, false))),
    )
    .expect("bootstrap")
    .core;
    core.settings()
        .update(|s| {
            support::assert_isolated_destination("the pipeline test", download_dir);
            s.download_directory = download_dir.to_string_lossy().into_owned();
            s.organize_by_creator = false;
            s.organize_by_media_type = false;
            s.folder_template = String::new();
            s.filename_template = "clip".to_string();
            Ok(())
        })
        .expect("settings");
    core
}

/// A version-1 partial plus the metadata the transfer wrote for it.
/// Stage a partial at the *worker's* own name for `task_id`, so the resume
/// gate in `attempt_once` actually finds it.
///
/// The worker names its partials `.{file}.{first-8-of-task-id}.video.part`; a
/// partial staged anywhere else is invisible to it, which is why the task must
/// exist before this runs.
fn worker_partial_path(core: &AppCore, task_id: &str) -> std::path::PathBuf {
    let settings = core.settings().current();
    let plan = core
        .storage()
        .plan_destination(
            &douyin_downloader_lib::storage::NamingContext {
                post_id: Some("staging".into()),
                extension: Some("mp4".into()),
                ..Default::default()
            },
            &settings,
        )
        .expect("plan");
    let stamp = &task_id[..task_id.len().min(8)];
    std::path::Path::new(&plan.full_path)
        .parent()
        .unwrap()
        .join(format!(".{}.{stamp}.video.part", plan.file_name))
}

async fn stage_partial(
    core: &AppCore,
    task_id: &str,
    head: &[u8],
    total: u64,
    etag: &str,
) -> std::path::PathBuf {
    let partial = worker_partial_path(core, task_id);
    std::fs::create_dir_all(partial.parent().unwrap()).expect("directory");
    std::fs::write(&partial, head).expect("write partial");
    douyin_downloader_lib::downloads::partial::PartialMetadata {
        validator: Some(douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: etag.to_string(),
        }),
        total_bytes: Some(total),
        offset_bytes: head.len() as u64,
    }
    .write(&partial)
    .await
    .expect("write metadata");
    partial
}

#[tokio::test]
async fn a_matching_resource_resumes_and_the_bytes_are_one_version() {
    let head = vec![1u8; 128];
    let tail = vec![2u8; 128];
    let body = [head.clone(), tail.clone()].concat();

    let url = serve_versioned_resource(vec![("v1", body.clone())], true).await;
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();
    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");
    pause_and_settle(&core, &task.id).await;

    // The v1 partial, staged at the worker's own name with the metadata the
    // transfer itself would have written.
    let partial = stage_partial(&core, &task.id, &head, 256, "\"v1\"").await;

    // Resuming: the client reads the metadata, sends If-Range, gets a 206,
    // and appends.
    core.downloads().resume(&task.id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    // The file is version 1 in full, in order. Not a splice: every byte is
    // checked.
    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let bytes = std::fs::read(&saved).expect("read the final file");
    assert_eq!(bytes, body, "the file is exactly the one version");

    // The only dot-file that may remain is the one this test staged at a name
    // the worker does not own. Everything the *worker* created — its own
    // partials and their metadata — must be gone.
    let staged = partial.file_name().unwrap().to_string_lossy().into_owned();
    let leftovers: Vec<_> = collect_files(download_dir.path())
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with('.') && name != staged && name != format!("{staged}.meta.json")
                })
                .unwrap_or(false)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "the worker left its own partials behind: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_changed_resource_is_downloaded_fresh_and_never_spliced() {
    // Version 1's head, then the resource becomes version 2 — same length,
    // different bytes. A splice would have v1's head and v2's tail.
    let v1_head = vec![1u8; 128];
    let v1 = [[1u8; 128].as_slice(), [2u8; 128].as_slice()].concat();
    let v2 = [vec![9u8; 128], vec![8u8; 128]].concat();

    let url = serve_versioned_resource(vec![("v1", v1.clone()), ("v2", v2.clone())], true).await;
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();
    // Created and immediately paused, so the v1 partial can be staged at the
    // worker's own name before the transfer runs. This is also the state a user
    // pausing mid-download produces, which is the case under test.
    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");
    pause_and_settle(&core, &task.id).await;

    let staged_name = stage_partial(&core, &task.id, &v1_head, 256, "\"v1\"").await;

    // Resuming hands the task back to the scheduler. The stored validator is
    // v1; the server now serves v2, whose If-Range condition fails, so the
    // transfer must restart from zero.
    core.downloads().resume(&task.id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let bytes = std::fs::read(&saved).expect("read the final file");
    assert_eq!(
        bytes, v2,
        "the final file is version 2 in full — no v1 byte survived"
    );
    let leftovers: Vec<_> = collect_files(download_dir.path())
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with('.')
                        && name != staged_name.to_string_lossy()
                        && name != format!("{}.meta.json", staged_name.to_string_lossy())
                })
                .unwrap_or(false)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "the worker left its own partials behind: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_server_that_ignores_if_range_is_caught_by_the_client_validator() {
    // The server answers 206 to everything, conditions be damned. The client
    // compares the response ETag against the stored one and refuses to splice.
    let v1_head = vec![1u8; 128];
    let v1 = [[1u8; 128].as_slice(), [2u8; 128].as_slice()].concat();
    let v2 = vec![9u8; 256];

    let url = serve_versioned_resource(vec![("v1", v1.clone()), ("v2", v2.clone())], false).await;
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();
    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");
    pause_and_settle(&core, &task.id).await;

    let staged_name = stage_partial(&core, &task.id, &v1_head, 256, "\"v1\"").await;

    // The server ignores If-Range and answers 206 to everything; the client's
    // own validator comparison is what refuses the v1 partial here.
    core.downloads().resume(&task.id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let bytes = std::fs::read(&saved).expect("read the final file");
    assert_eq!(
        bytes, v2,
        "the final file is version 2 alone; the v1 partial was discarded"
    );
    let leftovers: Vec<_> = collect_files(download_dir.path())
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with('.')
                        && name != staged_name.to_string_lossy()
                        && name != format!("{}.meta.json", staged_name.to_string_lossy())
                })
                .unwrap_or(false)
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "the worker left its own partials behind: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_partial_without_metadata_is_discarded_rather_than_resumed() {
    // The metadata sidecar is what authorizes a resume. A partial whose
    // description is missing — a crash between the two writes, a hand-deleted
    // file — must not be offered: the bytes could be anything.
    let v1_head = vec![1u8; 128];
    let v1 = [[1u8; 128].as_slice(), [2u8; 128].as_slice()].concat();

    let url = serve_versioned_resource(vec![("v1", v1.clone())], true).await;
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    let partial = worker_partial_path(
        &core,
        // A task id whose stamp matches nothing staged — this test stages a
        // partial with no metadata at a *plausible* worker name, which is the
        // point: the file exists, the identity does not.
        "00000000-0000-0000-0000-000000000000",
    );
    std::fs::create_dir_all(partial.parent().unwrap()).expect("directory");
    std::fs::write(&partial, &v1_head).expect("write a partial with no metadata");

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();
    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");
    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let bytes = std::fs::read(&saved).expect("read the final file");
    assert_eq!(bytes, v1, "downloaded from zero, once, in full");
}

// --- the library, over a download that really happened ------------------------

/// A completed real transfer is what the Library reports as downloaded.
///
/// This is the loop Phase 05 exists to close: a task runs, bytes land in a file,
/// and the Library — which reads the *task row* and the *file system*, never the
/// transfer — says the post is downloaded. Nothing here is seeded beyond the
/// post and its media item; the download itself is real.
#[tokio::test]
async fn a_real_download_reads_back_as_downloaded_in_the_library() {
    use douyin_downloader_lib::models::{LibrarySort, LibraryView};
    use douyin_downloader_lib::services::LibraryQuery;

    let url = serve_bytes(Vec::from(*b"\x00\x00\x00\x20ftypisom")).await;
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = core_with_download_dir(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");

    // Before the download: discovered, and nothing more.
    let before = core
        .query_library(LibraryQuery {
            view: LibraryView::New,
            ..Default::default()
        })
        .expect("query");
    assert_eq!(before.total, 1);
    assert_eq!(before.entries[0].status, ContentStatus::New);
    assert_eq!(before.counts.downloaded, 0);

    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();

    let rx = core.downloads().subscribe();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.clone(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");
    assert_eq!(wait_for_terminal(rx, &task.id).await, DownloadState::Completed);

    // After: the Library agrees, and names the file that is actually there.
    let after = core
        .query_library(LibraryQuery {
            sort: LibrarySort::DownloadedFirst,
            ..Default::default()
        })
        .expect("query");
    assert_eq!(after.total, 1, "one post, not one row per download");
    let entry = &after.entries[0];
    assert_eq!(entry.status, ContentStatus::Downloaded);
    assert_eq!(entry.download_state, Some(DownloadState::Completed));
    assert_eq!(entry.file_state.as_deref(), Some("present"));
    assert_eq!(entry.downloaded_item_count, 1);
    assert_eq!(entry.media_item_count, 1);
    assert!(!entry.is_partially_downloaded());
    assert!(entry.downloaded_at.is_some(), "the library knows when it finished");

    let reported = entry.local_path.as_deref().expect("a local path");
    let stored = core.downloads().get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        Some(reported),
        stored.destination_path.as_deref(),
        "the Library reports the path the transfer recorded"
    );
    assert!(Path::new(reported).is_file(), "and that path is a real file");

    assert_eq!(after.counts.downloaded, 1);
    assert_eq!(after.counts.new, 0);

    // --- and when that file is deleted -------------------------------------
    //
    // The task row is untouched — it is a record of what happened. What changes
    // is the post's status, because nothing is on disk any more.
    std::fs::remove_file(reported).expect("delete the downloaded file");

    let gone = core
        .query_library(LibraryQuery::default())
        .expect("query");
    assert_eq!(gone.entries[0].status, ContentStatus::Ready);
    assert_eq!(gone.entries[0].file_state.as_deref(), Some("missing"));
    assert!(
        gone.entries[0]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("no longer on disk"),
        "and says why"
    );
    assert_eq!(gone.counts.downloaded, 0);
    assert_eq!(gone.counts.new, 1, "a post with no file is downloadable again");

    // The history of the download survives; only the post's status changed.
    assert_eq!(
        core.database()
            .with_conn(|conn| history_repo::count(conn))
            .unwrap(),
        1
    );
    assert_eq!(
        core.downloads().get(&task.id).await.unwrap().unwrap().state,
        DownloadState::Completed
    );
}

// --- the pause/claim race, at the level a user sees it -------------------------

/// Pausing a task the moment it is created must pause it — every time.
///
/// The race this guards is narrow and real: the worker records an attempt count
/// as it claims a task, and it used to do that as a read-modify-write that
/// carried the task's *state* back to the row. A `pause` landing between the
/// read and the write was therefore undone, the row went back to `Downloading`,
/// and the transfer the pause had stopped reported the task as `Cancelled`
/// rather than `Paused` — a terminal state the user never asked for, on a task
/// they had every reason to expect could be resumed.
///
/// Reproduced here by doing it repeatedly against a server that keeps the
/// transfer busy, so every pause has something in flight to interrupt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_a_task_right_after_creating_it_always_pauses_it() {
    let url = serve_slowly(4 * 1024 * 1024, 16 * 1024, Duration::from_millis(20)).await;
    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = core_with_download_dir(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, &post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();

    let mut paused = Vec::new();
    for round in 1..=12 {
        let task = core
            .create_download(CreateDownloadInput {
                post_id: post_id.clone(),
                media_item_id: media_id.clone(),
                variant_id: None,
            })
            .await
            .expect("task created");
        core.downloads().pause(&task.id).await.expect("pause");
        paused.push(task.id);
        let _ = round;
    }

    // Give the workers a moment to do whatever they were about to do.
    tokio::time::sleep(Duration::from_millis(500)).await;

    for id in &paused {
        let task = core.downloads().get(id).await.unwrap().unwrap();
        assert_eq!(
            task.state,
            DownloadState::Paused,
            "a paused task must stay paused, not become {:?}",
            task.state
        );
    }

    // And none of them is history: a pause is not an outcome.
    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 50, 0))
        .unwrap();
    assert!(
        history.is_empty(),
        "a paused task must not be recorded as a finished one: {history:?}"
    );
}

// --- resume validation: what the client does with a response it cannot use -----//
// The tests above prove the *good* resume and the *changed resource*. These
// prove the responses a source can send that are neither: a range it refuses to
// honour, a range it claims to honour but describes wrongly, and a description
// that cannot be read at all. Every one of them must end with the file holding
// exactly one version of the resource — never a splice, never a truncation.

/// How a loopback origin answers a `Range` request.
#[derive(Clone, Copy, Debug)]
enum RangeBehaviour {
    /// Ignores `Range` entirely: the whole body, `200`.
    IgnoresRange,
    /// Rejects any range with `416 Range Not Satisfiable`.
    RejectsRange,
    /// Answers `206`, but describes a range that starts somewhere else.
    WrongStart,
    /// Answers `206` for the right offset, but states a different whole-resource
    /// size than the partial was written under.
    ///
    /// The case the validator cannot always speak to: a source that offers a
    /// validator on the first response and none on the ranged one has said
    /// nothing at all about identity, and the total is then the only statement
    /// left to contradict it.
    WrongTotal,
    /// Answers `206` with a `Content-Range` that cannot be parsed.
    UnparseableContentRange,
    /// Answers `206` for exactly the offset and total that were asked for, and
    /// **names no validator at all**.
    ///
    /// The case the offset and the total cannot speak to. A resource that was
    /// re-encoded at the same length matches both perfectly, so a server that
    /// answers a range without saying *which* representation it is answering
    /// about leaves nothing on which to found a continuation — and the tail it
    /// hands back here is the current version's, so appending it to a partial
    /// from the previous one would splice.
    UnlabelledTail,
}

/// A fixture server, plus what it was actually asked for.
///
/// The counts are not decoration. Whether the client *offered* the partial is
/// the difference between a policy that refused it and one that never got as far
/// as asking, and the final bytes cannot tell those two apart — so several tests
/// below are only meaningful alongside them.
struct CountingServer {
    url: String,
    requests: Arc<AtomicUsize>,
    ranged: Arc<AtomicUsize>,
}

impl CountingServer {
    fn counting(url: String) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let ranged = Arc::new(AtomicUsize::new(0));
        (
            Self {
                url,
                requests: Arc::clone(&requests),
                ranged: Arc::clone(&ranged),
            },
            requests,
            ranged,
        )
    }

    /// How many requests arrived at all.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// How many of them offered a byte range.
    fn ranged(&self) -> usize {
        self.ranged.load(Ordering::SeqCst)
    }
}

/// Serve `body`, answering a ranged request according to `behaviour`.
async fn serve_range_behaviour(body: Vec<u8>, behaviour: RangeBehaviour) -> CountingServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let body = Arc::new(body);
    let (server, requests, ranged_counter) =
        CountingServer::counting(format!("http://{address}/media.mp4"));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = Arc::clone(&body);
            let requests = Arc::clone(&requests);
            let ranged_counter = Arc::clone(&ranged_counter);
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let read = socket.read(&mut scratch).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&scratch[..read]).to_string();
                requests.fetch_add(1, Ordering::SeqCst);
                let ranged = request
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("range:"));
                if ranged {
                    ranged_counter.fetch_add(1, Ordering::SeqCst);
                }
                let requested = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split_once('='))
                    .and_then(|(_, rest)| rest.trim().trim_end_matches('-').parse::<usize>().ok())
                    .unwrap_or(0);

                // What this behaviour answers with: the status line, the
                // `Content-Range` (empty for none), the body bytes, and whether a
                // validator is offered at all.
                let (status, content_range, slice, send_etag): (&str, String, Vec<u8>, bool) =
                    match behaviour {
                        RangeBehaviour::IgnoresRange => ("200 OK", String::new(), body.to_vec(), true),
                        RangeBehaviour::RejectsRange if ranged => (
                            "416 Range Not Satisfiable",
                            "bytes */256".to_string(),
                            Vec::new(),
                            true,
                        ),
                        RangeBehaviour::WrongStart if ranged => (
                            "206 Partial Content",
                            format!("bytes 0-{}/{}", body.len() - 1, body.len()),
                            body.to_vec(),
                            true,
                        ),
                        // The right offset, a plausible `Content-Range`, and a
                        // whole-resource size that is not the one the partial was
                        // written under — from a source that offers no validator
                        // here, so the size is the only statement contradicting
                        // it. The tail is a *different* version's bytes: appending
                        // it would splice, and the test's byte comparison is what
                        // catches that.
                        RangeBehaviour::WrongTotal if ranged && requested < body.len() => (
                            "206 Partial Content",
                            format!(
                                "bytes {requested}-{}/{}",
                                body.len() - 1,
                                body.len() + 1024
                            ),
                            vec![0xAAu8; body.len() - requested],
                            false,
                        ),
                        RangeBehaviour::UnparseableContentRange if ranged => (
                            "206 Partial Content",
                            "not a content range at all".to_string(),
                            body[body.len() / 2..].to_vec(),
                            true,
                        ),
                        // Exactly the range that was asked for, the right total,
                        // and no validator of any kind. The offset and the size
                        // agree with the partial; nothing says the bytes are from
                        // the same version, and they are not.
                        RangeBehaviour::UnlabelledTail if ranged && requested < body.len() => (
                            "206 Partial Content",
                            format!("bytes {requested}-{}/{}", body.len() - 1, body.len()),
                            body[requested..].to_vec(),
                            false,
                        ),
                        // Not ranged, or not the case under test: the whole body.
                        _ => ("200 OK", String::new(), body.to_vec(), true),
                    };

                tokio::time::sleep(ANSWER_DELAY).await;
                let mut header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\n\
                     Content-Length: {}\r\nAccept-Ranges: bytes\r\n",
                    slice.len()
                );
                if send_etag {
                    header.push_str("ETag: \"v1\"\r\n");
                }
                if !content_range.is_empty() {
                    header.push_str(&format!("Content-Range: {content_range}\r\n"));
                }
                header.push_str("Connection: close\r\n\r\n");

                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&slice).await;
                let _ = socket.flush().await;
            });
        }
    });

    server
}

/// Serve `body` with real `Range` support, dribbling the remainder out slowly.
///
/// A resumed transfer has to be catchable *in flight* for a cancellation test to
/// mean anything, and a server that answers a `206` all at once gives no window
/// to cancel in. The validator is a plain strong `ETag: "v1"`, so a partial
/// written under it resumes cleanly until the test stops it.
async fn serve_range_slowly(body: Vec<u8>, chunk: usize, delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let body = Arc::new(body);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let read = socket.read(&mut scratch).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&scratch[..read]).to_string();

                let offset = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split_once('='))
                    .and_then(|(_, rest)| rest.trim().trim_end_matches('-').parse::<usize>().ok())
                    .unwrap_or(0);
                let offset = offset.min(body.len());
                let slice = &body[offset..];

                let mut header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nETag: \"v1\"\r\n\
                     Content-Length: {}\r\nAccept-Ranges: bytes\r\n\
                     Content-Range: bytes {offset}-{}/{}\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    body.len() - 1,
                    body.len()
                );
                if offset == 0 {
                    header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nETag: \"v1\"\r\n\
                         Content-Length: {}\r\nAccept-Ranges: bytes\r\n\
                         Connection: close\r\n\r\n",
                        slice.len()
                    );
                }

                let _ = socket.write_all(header.as_bytes()).await;
                for piece in slice.chunks(chunk.max(1)) {
                    if socket.write_all(piece).await.is_err() {
                        return;
                    }
                    let _ = socket.flush().await;
                    tokio::time::sleep(delay).await;
                }
            });
        }
    });

    format!("http://{address}/media.mp4")
}

/// Serve `body` with a `Last-Modified` validator and no `ETag`, honouring
/// `If-Range` against it.
///
/// The weaker half of the validator pair: a timestamp is all some origins
/// offer, and a resume has to work with it — through the same `If-Range`
/// exchange, so the *server* decides whether the range is still valid.
async fn serve_last_modified(
    body: Vec<u8>,
    modified: &'static str,
    date: Option<&'static str>,
) -> CountingServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let body = Arc::new(body);
    let (server, requests, ranged_counter) =
        CountingServer::counting(format!("http://{address}/media.mp4"));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = Arc::clone(&body);
            let requests = Arc::clone(&requests);
            let ranged_counter = Arc::clone(&ranged_counter);
            tokio::spawn(async move {
                let mut scratch = [0u8; 4096];
                let read = socket.read(&mut scratch).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&scratch[..read]).to_string();
                requests.fetch_add(1, Ordering::SeqCst);

                let offset = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|line| line.split_once('='))
                    .and_then(|(_, rest)| rest.trim().trim_end_matches('-').parse::<usize>().ok())
                    .unwrap_or(0);
                if offset > 0 {
                    ranged_counter.fetch_add(1, Ordering::SeqCst);
                }
                let if_range = request
                    .lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("if-range:"))
                    .map(|line| line.split_once(':').unwrap().1.trim().to_string());

                let condition_ok = if_range.as_deref() == Some(modified);
                let (status, slice) = if offset > 0 && offset < body.len() && condition_ok {
                    ("206 Partial Content", &body[offset..])
                } else {
                    ("200 OK", &body[..])
                };

                tokio::time::sleep(ANSWER_DELAY).await;
                let mut header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: video/mp4\r\n\
                     Last-Modified: {modified}\r\nContent-Length: {}\r\n\
                     Accept-Ranges: bytes\r\n",
                    slice.len()
                );
                // `date` is what makes the `Last-Modified` deducible as a strong
                // validator, and `None` is a response that offers none — which
                // is the case a client has to refuse.
                if let Some(date) = date {
                    header.push_str(&format!("Date: {date}\r\n"));
                }
                if status.starts_with("206") {
                    header.push_str(&format!(
                        "Content-Range: bytes {}-{}/{}\r\n",
                        offset,
                        body.len() - 1,
                        body.len()
                    ));
                }
                header.push_str("Connection: close\r\n\r\n");

                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(slice).await;
                let _ = socket.flush().await;
            });
        }
    });

    server
}

/// Stage a partial at the worker's own name with an explicit validator.
///
/// The sidecar is written **before** the partial, which is the order the
/// transfer itself uses, and the order is load-bearing. A worker that reads the
/// pair while they are being staged must never see a partial with no sidecar:
/// that is the one state the policy reads as "these bytes belong to nothing",
/// and it *removes* the file rather than ignoring it. Writing the file first
/// leaves exactly that window open, and under load a paused attempt still
/// unwinding walks straight into it — which looked, from the outside, like a
/// resume test that mysteriously never offered its partial.
async fn stage_partial_with(
    core: &AppCore,
    task_id: &str,
    head: &[u8],
    total: u64,
    validator: douyin_downloader_lib::downloads::partial::ResourceValidator,
) -> std::path::PathBuf {
    let partial = worker_partial_path(core, task_id);
    std::fs::create_dir_all(partial.parent().unwrap()).expect("directory");
    douyin_downloader_lib::downloads::partial::PartialMetadata {
        validator: Some(validator),
        total_bytes: Some(total),
        offset_bytes: head.len() as u64,
    }
    .write(&partial)
    .await
    .expect("write metadata");
    std::fs::write(&partial, head).expect("write partial");
    partial
}

/// Pause a task and wait until its transfer has stopped writing.
///
/// The wait is the whole point, and it is what makes the staging that follows
/// deterministic instead of lucky.
///
/// A paused task is not yet a *stopped* one. The state is written first and the
/// transfer goes on until it notices, which it does inside `write_stream` —
/// right after it has written its own sidecar and created the partial file. By
/// then the pause is long set, so that same first check stops it **before a
/// single byte of body is written**. The sidecar appearing is therefore the
/// signal that the worker has nothing left to write, and staging over the pair
/// after that cannot be overtaken.
///
/// Staging without waiting is what makes a resume test mysteriously never offer
/// its partial: the worker's write lands hundreds of milliseconds later, on top
/// of the staged pair, and the sidecar left behind describes something the test
/// never intended.
async fn pause_and_settle(core: &AppCore, task_id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

    // Wait for the worker to have *claimed* the task before pausing it. Pausing
    // a task that is still queued is a different scenario — the attempt never
    // runs, nothing is ever written, and the wait below would hang on a sidecar
    // that is never coming. Downloading is the proof that an attempt is in
    // flight.
    loop {
        let stored = core.downloads().get(task_id).await.unwrap().unwrap();
        if stored.state == DownloadState::Downloading {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the task {task_id} was never claimed by a worker; state was {:?}",
            stored.state
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    core.downloads().pause(task_id).await.expect("pause");

    loop {
        let stored = core.downloads().get(task_id).await.unwrap().unwrap();
        if stored.state == DownloadState::Paused {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the task {task_id} never paused; state was {:?}",
            stored.state
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let partial = worker_partial_path(core, task_id);
    let mut name = partial.file_name().unwrap().to_string_lossy().into_owned();
    name.push_str(".meta.json");
    let sidecar = partial.with_file_name(name);

    while !sidecar.is_file() {
        // Say what happened to the attempt, not merely that a file is missing.
        // A transfer that died has a state and an error that explain why; a
        // transfer that is still running has neither, and the difference is the
        // whole diagnosis.
        let stored = core.downloads().get(task_id).await.unwrap().unwrap();
        assert!(
            !stored.is_terminal(),
            "the transfer ended as {:?} instead of reaching its first write: {}",
            stored.state,
            stored.error.as_deref().unwrap_or("no error recorded")
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "the transfer never recorded a sidecar, so there is nothing to stage              over at {} (state {:?})",
            sidecar.display(),
            stored.state
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A download for `post_id`, paused and settled, ready to be staged over.
async fn paused_task(core: &AppCore, post_id: &str) -> String {
    let media_id = core
        .database()
        .with_conn(|conn| posts_repo::get(conn, post_id))
        .unwrap()
        .unwrap()
        .media_items[0]
        .id
        .clone();
    let task = core
        .create_download(CreateDownloadInput {
            post_id: post_id.to_string(),
            media_item_id: media_id,
            variant_id: None,
        })
        .await
        .expect("task created");
    pause_and_settle(core, &task.id).await;
    task.id
}

/// Wait for a task to reach a terminal state, and say what it was stuck in.
///
/// Polls the row rather than the event stream, which is what a restart test
/// needs: there are two cores over one database, and the question is what the
/// database says, not what one of them announced.
async fn await_terminal(core: &AppCore, task_id: &str, patience: Duration) -> DownloadState {
    let deadline = std::time::Instant::now() + patience;
    loop {
        let task = core.downloads().get(task_id).await.unwrap().unwrap();
        if task.is_terminal() {
            return task.state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the task never reached a terminal state; after {patience:?} it was {:?} \
             with {} attempt(s)",
            task.state,
            task.attempts
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Stage what a *previous run of the application* left on disk, and return the
/// task id it left behind. Runs on its own runtime, and that runtime is dropped
/// before this returns — which is what a process ending does.
///
/// Using a separate runtime rather than merely dropping the `AppCore` is the
/// point. A core's transfer worker is a spawned task holding its own `Arc`, so
/// dropping the core does not stop it: it goes on unwinding, still holding a
/// cancellation handle for the task. A second worker that then claims that task
/// can have it marked cancelled by the first one's unwind — not on the way out
/// of its own attempt, but on top of someone else's. That is an artefact of two
/// workers over one database, and tearing the runtime down removes it, leaving
/// What the *previous run of the application* left on disk, and the task id it
/// belonged to.
///
/// The first core is dropped here, and everything that decided anything about
/// the transfer went with it: its scheduler, its handles, its worker. What is
/// left for the next core is exactly what a restart leaves — the database and
/// the two files.
///
/// Nothing has to be torn down more forcefully than that, because
/// [`pause_and_settle`] has already waited for the worker to reach the one
/// point where it stops writing: the sidecar it records before it notices the
/// pause. After that it touches no file, so staging over the pair cannot be
/// overtaken and dropping the core cannot lose a write.
async fn previous_process_left_behind(
    data_dir: &Path,
    downloads: &Path,
    url: &str,
    head: &[u8],
    total: u64,
) -> String {
    let core = core_over(data_dir, downloads);
    let post_id = seed_post(&core, url, "mp4");
    let task_id = paused_task(&core, &post_id).await;
    stage_partial_with(
        &core,
        &task_id,
        head,
        total,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;
    task_id
}

/// Any dot-file left under the download root.
///
/// The worker's own partials and their sidecars all start with one, so this is
/// what "nothing was left behind" means.
fn worker_leftovers(root: &Path) -> Vec<std::path::PathBuf> {
    collect_files(root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with('.'))
                .unwrap_or(false)
        })
        .collect()
}

/// One resource, one version, whatever the source did with the range.
///
/// Runs the whole staging dance and asserts the three things that matter: the
/// partial was actually *offered* (or the test would be passing because nothing
/// was ever attempted), the task completes, and the file on disk is *exactly*
/// the resource — every byte, in order. A splice, a duplicated prefix, and a
/// truncated tail all fail here.
async fn assert_resume_ends_with_one_version(server: CountingServer, body: &[u8], label: &str) {
    let url = server.url.clone();

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    // Half the resource already on disk, described as version 1.
    stage_partial_with(
        &core,
        &task_id,
        &body[..body.len() / 2],
        body.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(
        wait_for_terminal(rx, &task_id).await,
        DownloadState::Completed,
        "{label}: the task must still complete"
    );

    assert!(
        server.ranged() >= 1,
        "{label}: the client never offered the partial ({} request(s) arrived, none \
         ranged), so nothing about resume behaviour was tested",
        server.requests()
    );

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let bytes = std::fs::read(&saved).expect("read the final file");
    assert_eq!(bytes, body, "{label}: the file must be exactly one version");

    let leftovers = worker_leftovers(download_dir.path());
    assert!(leftovers.is_empty(), "{label}: leftovers {leftovers:?}");

    // One outcome, recorded once. A safe restart that re-downloads is still one
    // download: the history says what the task produced, not how many attempts
    // it took to produce it.
    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "{label}: one task means one history row");
    assert_eq!(history[0].status, DownloadState::Completed, "{label}");
    assert_eq!(
        history[0].local_path.as_deref(),
        Some(saved.as_str()),
        "{label}: and it points at the file that landed"
    );
}

#[tokio::test]
async fn a_source_that_ignores_range_is_restarted_not_appended() {
    // The whole body comes back to a ranged request. Appending would duplicate
    // everything before the offset, so the partial is discarded and the file
    // rewritten.
    let body = [vec![1u8; 128], vec![2u8; 128]].concat();
    let server = serve_range_behaviour(body.clone(), RangeBehaviour::IgnoresRange).await;
    assert_resume_ends_with_one_version(server, &body, "a source that ignores Range").await;
}

#[tokio::test]
async fn a_416_is_recovered_by_restarting_from_zero() {
    let body = [vec![3u8; 128], vec![4u8; 128]].concat();
    let server = serve_range_behaviour(body.clone(), RangeBehaviour::RejectsRange).await;
    assert_resume_ends_with_one_version(server, &body, "a 416 answer").await;
}

#[tokio::test]
async fn a_content_range_for_another_offset_is_not_appended_at() {
    // A `206` whose own `Content-Range` says it starts somewhere other than the
    // offset that was asked for. Trusting it would write the wrong bytes into
    // the file.
    let body = [vec![5u8; 128], vec![6u8; 128]].concat();
    let server = serve_range_behaviour(body.clone(), RangeBehaviour::WrongStart).await;
    assert_resume_ends_with_one_version(server, &body, "a mismatched Content-Range").await;
}

#[tokio::test]
async fn an_unparseable_content_range_is_not_trusted() {
    let body = [vec![7u8; 128], vec![8u8; 128]].concat();
    let server = serve_range_behaviour(body.clone(), RangeBehaviour::UnparseableContentRange).await;
    assert_resume_ends_with_one_version(server, &body, "an unreadable Content-Range").await;
}

#[tokio::test]
async fn a_last_modified_resume_continues_from_the_offset() {
    // The weaker validator, and the one a CDN without ETags leaves you. It has
    // to work — through the same `If-Range` exchange, so the server decides —
    // and the response's `Date` is what makes the date strong enough to be
    // offered at all.
    const MODIFIED: &str = "Wed, 21 Oct 2026 07:28:00 GMT";
    const DATE: &str = "Wed, 21 Oct 2026 07:28:01 GMT";
    let body = [vec![1u8; 128], vec![2u8; 128]].concat();
    let server = serve_last_modified(body.clone(), MODIFIED, Some(DATE)).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &server.url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &body[..128],
        body.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::LastModified {
            value: MODIFIED.to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    assert!(
        server.ranged() >= 1,
        "the date must have been strong enough to offer the partial at all"
    );

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, body, "the timestamp validator carried the resume");
}

#[tokio::test]
async fn a_last_modified_that_moved_restarts_instead_of_splicing() {
    // The resource was re-encoded between the pause and the resume, and only a
    // timestamp describes it. Same length, different bytes.
    let v1 = [vec![1u8; 128], vec![2u8; 128]].concat();
    let v2 = [vec![9u8; 128], vec![8u8; 128]].concat();
    let server = serve_last_modified(
        v2.clone(),
        "Thu, 22 Oct 2026 09:15:00 GMT",
        Some("Thu, 22 Oct 2026 09:15:01 GMT"),
    )
    .await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &server.url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &v1[..128],
        v1.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::LastModified {
            value: "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    assert!(
        server.ranged() >= 1,
        "the partial was offered, and refused — which is the case under test"
    );

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, v2, "the file is the new version alone");
    assert!(!bytes.starts_with(&v1[..128]), "no stale byte survived");
}

#[tokio::test]
async fn a_206_that_names_no_validator_is_not_appended_to() {
    // The gap this closes. The offset is exactly right, the total is exactly
    // right, the response is a well-formed `206 Partial Content` — and it names
    // no validator at all, so nothing in it says *which* representation it is
    // continuing. An unchanged size and offset are not evidence of identity: a
    // re-encode at the same length matches both exactly.
    //
    // The server is honest about the bytes and silent about the identity: the
    // tail it hands back is the *current* version's, so appending it to the
    // partial would produce v1's head followed by v2's tail.
    let v1_head = vec![1u8; 128];
    let v2 = [vec![9u8; 128], vec![8u8; 128]].concat();
    let server =
        serve_range_behaviour(v2.clone(), RangeBehaviour::UnlabelledTail).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &server.url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &v1_head,
        v2.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    assert!(
        server.ranged() >= 1,
        "the stored tag was strong, so the partial must have been offered — otherwise \
         this test would be passing because nothing was attempted"
    );

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, v2, "a `206` naming no validator must not be appended to");
    assert!(
        !bytes.starts_with(&v1_head),
        "not one byte of the partial may survive a restart"
    );
}

#[tokio::test]
async fn an_unlabelled_range_cannot_bypass_the_check_with_a_matching_size() {
    // The same hole, stated the way it actually bites: a server that ignores
    // `If-Range` and answers the range from whatever it is serving *now*. The
    // total is unchanged, so the total check cannot catch it, and the response
    // carries no validator, so the validator check has nothing to work with.
    // What must happen is a restart — the partial is not trustworthy, and the
    // only safe move is to fetch the resource whole.
    let v1_head = vec![5u8; 64];
    let v2 = [vec![7u8; 64], vec![6u8; 64]].concat();
    let server =
        serve_range_behaviour(v2.clone(), RangeBehaviour::UnlabelledTail).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &server.url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &v1_head,
        // The size the partial was written under is the size the source still
        // reports, so nothing about the *shape* of the response is wrong.
        v2.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);
    assert!(server.ranged() >= 1);

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    let bytes = std::fs::read(&saved).unwrap();
    assert_eq!(bytes, v2, "the file is one version, never a mixture");
    assert!(
        !bytes.starts_with(&v1_head),
        "a matching total must not let a different version's bytes through"
    );

    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "one outcome, recorded once");
    assert_eq!(history[0].status, DownloadState::Completed);
    assert_eq!(history[0].local_path.as_deref(), Some(saved.as_str()));
}

#[tokio::test]
async fn a_last_modified_the_response_cannot_confirm_is_not_appended_to() {
    // A date with no `Date` beside it is implicitly weak under RFC 9110
    // §8.8.2.2, so the response cannot confirm anything about identity.
    //
    // The strength test is applied to *every* response, and it is the response
    // at hand that has to pass it: the sidecar carries a value, not a verdict,
    // because the deduction rests on the `Date` of the response being examined.
    // So the partial is offered — the client has a value worth asking about —
    // and then the answer is refused for want of confirmation. What matters is
    // that the bytes are never appended to.
    let v1_head = vec![1u8; 128];
    let v2 = [vec![9u8; 128], vec![8u8; 128]].concat();
    const MODIFIED: &str = "Wed, 21 Oct 2026 07:28:00 GMT";
    let server = serve_last_modified(v2.clone(), MODIFIED, None).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &server.url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &v1_head,
        v2.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::LastModified {
            value: MODIFIED.to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    assert!(
        server.ranged() >= 1,
        "the partial was offered, so the refusal below is the response being \
         refused rather than the request never being made"
    );

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, v2, "the transfer restarts rather than risk a splice");
    assert!(!bytes.starts_with(&v1_head));
}

#[tokio::test]
async fn cancelling_a_safe_restart_leaves_no_completed_history() {
    // The restart path has to be as stoppable as the append path. The stored
    // validator is one the server will not confirm, so the ranged request is
    // refused and the transfer starts over — and it is that second, full
    // download that gets cancelled.
    let body = [vec![3u8; 256 * 1024], vec![4u8; 256 * 1024]].concat();
    let url = serve_range_slowly(body.clone(), 8 * 1024, Duration::from_millis(30)).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    // The server serves `ETag: "v1"`; a partial claiming `"v2"` cannot be
    // continued by it, so the transfer must restart from zero.
    stage_partial_with(
        &core,
        &task_id,
        &body[..256 * 1024],
        body.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v2\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let running = core.downloads().get(&task_id).await.unwrap().unwrap();
    assert!(
        !running.is_terminal(),
        "the restarted transfer finished before it could be cancelled; state was {:?}",
        running.state
    );

    let cancelled = core.downloads().cancel(&task_id).await.expect("cancel");
    assert_eq!(cancelled.state, DownloadState::Cancelled);
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Cancelled);

    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "one outcome");
    assert_eq!(
        history[0].status,
        DownloadState::Cancelled,
        "a cancelled restart is not a completed download"
    );
    assert!(history[0].local_path.is_none());

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    assert!(stored.destination_path.is_none(), "nothing was saved");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut leftovers = worker_leftovers(download_dir.path());
    while !leftovers.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        leftovers = worker_leftovers(download_dir.path());
    }
    assert!(
        leftovers.is_empty(),
        "a cancelled restart must not leave its partial: {leftovers:?}"
    );
}

#[tokio::test]
async fn malformed_partial_metadata_discards_the_partial() {
    // The sidecar is unreadable JSON. It describes nothing, so the bytes beside
    // it cannot be attributed to any resource and must not be continued.
    let body = [vec![1u8; 128], vec![2u8; 128]].concat();
    let url = serve_versioned_resource(vec![("v1", body.clone())], true).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    let partial = stage_partial_with(
        &core,
        &task_id,
        &body[..128],
        body.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;
    // Corrupt the sidecar, leaving the partial itself in place.
    let metadata = partial.with_file_name(format!(
        "{}.meta.json",
        partial.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&metadata, b"{ this is not json").expect("corrupt the metadata");

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, body, "an unattributable partial is replaced, not continued");

    let leftovers = worker_leftovers(download_dir.path());
    assert!(
        leftovers.is_empty(),
        "the unusable partial must be removed: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_response_that_describes_a_different_size_is_not_appended_to() {
    // The right offset, a plausible `Content-Range`, and a whole-resource size
    // that is not the one the partial was written under — from a source that
    // offers no validator on this response. The total is then the only statement
    // left, and it says this is a different resource.
    //
    // The tail this server hands back is a *different* version's bytes, so an
    // append would splice and the final byte comparison is what catches it.
    let body = [vec![1u8; 128], vec![2u8; 128]].concat();
    let server = serve_range_behaviour(body.clone(), RangeBehaviour::WrongTotal).await;
    assert_resume_ends_with_one_version(server, &body, "a contradictory total").await;
}

#[tokio::test]
async fn a_weak_validator_does_not_authorize_a_resume() {
    // A weak `ETag` is defined to compare equal for representations that are
    // merely *semantically* equivalent, so two byte-different encodings can
    // share one — and `If-Range` is defined not to accept one at all.
    //
    // The server here is the worst case and the realistic one: it serves the
    // same weak tag for both versions and honours `If-Range` faithfully. A
    // resume taken on the strength of that tag would append v2's tail to v1's
    // head and produce a file that is neither.
    let v1_head = vec![1u8; 128];
    let v2 = [vec![9u8; 128], vec![8u8; 128]].concat();
    let url = serve_versioned_resource(vec![("W/\"same\"", v2.clone())], true).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &v1_head,
        v2.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "W/\"same\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, v2, "a weak tag must not carry a resume");
    assert!(
        !bytes.starts_with(&v1_head),
        "not one byte of the partial survived"
    );
}

#[tokio::test]
async fn a_sidecar_that_disagrees_with_the_file_length_is_discarded() {
    // The sidecar describes a 128-byte resource; the file beside it holds 200
    // bytes. A file longer than the resource it claims is drift no validator can
    // explain away — the two have stopped describing the same thing — so it is
    // removed rather than continued.
    let body = [vec![3u8; 128], vec![4u8; 128]].concat();
    let url = serve_versioned_resource(vec![("v1", body.clone())], true).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    // 200 bytes staged under a sidecar that claims the resource is 128.
    stage_partial_with(
        &core,
        &task_id,
        &body[..200],
        128,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(bytes, body, "the file is the whole resource, once");

    let leftovers = worker_leftovers(download_dir.path());
    assert!(
        leftovers.is_empty(),
        "the disagreeing partial must be removed: {leftovers:?}"
    );
}

#[tokio::test]
async fn cancelling_during_a_resume_leaves_no_completed_history() {
    // A continuation that is cancelled is a cancelled task, not a finished one:
    // the bytes it had written are discarded with it and nothing is recorded as
    // a completed download.
    //
    // The response is dribbled out so the cancel has something to interrupt —
    // half the resource is already on disk, and the other half arrives in pieces
    // over about a second.
    let body = [vec![5u8; 256 * 1024], vec![6u8; 256 * 1024]].concat();
    let url = serve_range_slowly(body.clone(), 8 * 1024, Duration::from_millis(30)).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &body[..256 * 1024],
        body.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The transfer has to still be running, or the test would be asserting
    // something about a download that had already finished.
    let running = core.downloads().get(&task_id).await.unwrap().unwrap();
    assert!(
        !running.is_terminal(),
        "the resumed transfer finished before it could be cancelled, so nothing was \
         interrupted; state was {:?}",
        running.state
    );

    let cancelled = core.downloads().cancel(&task_id).await.expect("cancel");
    assert_eq!(cancelled.state, DownloadState::Cancelled);
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Cancelled);

    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "one outcome");
    assert_eq!(
        history[0].status,
        DownloadState::Cancelled,
        "a cancelled resume is not a completed download"
    );
    assert!(history[0].local_path.is_none());

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    assert!(stored.destination_path.is_none(), "nothing was saved");

    // The partial is removed by the cancellation, not kept for a resume that was
    // never asked for.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut leftovers = worker_leftovers(download_dir.path());
    while !leftovers.is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        leftovers = worker_leftovers(download_dir.path());
    }
    assert!(
        leftovers.is_empty(),
        "a cancelled resume must not leave its partial: {leftovers:?}"
    );
}

#[tokio::test]
async fn a_restart_revalidates_the_partial_the_same_way() {
    // The partial and its sidecar are the only things that cross a restart, so
    // the rules have to live in them. A second process over the same database —
    // with no memory of the first — must reach the same verdict about a partial
    // that no longer matches the resource.
    let v1 = [vec![1u8; 128], vec![2u8; 128]].concat();
    let v2 = [vec![9u8; 128], vec![8u8; 128]].concat();
    let url = serve_versioned_resource(vec![("v1", v1.clone()), ("v2", v2.clone())], true).await;

    let root = tempfile::tempdir().expect("root");
    let data_dir = root.path().join("data");
    let downloads = root.path().join("downloads");
    std::fs::create_dir_all(&downloads).expect("downloads dir");

    // The first process: create the task, pause it, leave a v1 partial behind
    // exactly as a paused transfer would have, and then end.
    let task_id =
        previous_process_left_behind(&data_dir, &downloads, &url, &v1[..128], v1.len() as u64)
            .await;

    // The second process. The scheduler, the handles, and every in-memory
    // decision are gone; the database and the files are not.
    let core = core_over(&data_dir, &downloads);
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(
        await_terminal(&core, &task_id, Duration::from_secs(30)).await,
        DownloadState::Completed
    );

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let bytes = std::fs::read(stored.destination_path.expect("a saved path")).unwrap();
    assert_eq!(
        bytes, v2,
        "the resource is v2 now; the stored v1 validator must have been refused"
    );
    assert!(!bytes.starts_with(&v1[..128]), "no byte of v1 survived");

    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "a restart does not duplicate the outcome");
    assert_eq!(history[0].status, DownloadState::Completed);
}

#[tokio::test]
async fn a_resume_that_restarts_records_one_outcome() {
    // The other half of the same rule: when the source refuses the range and the
    // transfer starts over, the task still produces exactly one history row —
    // the restart is how the file was obtained, not a second download.
    let v1 = [vec![1u8; 128], vec![2u8; 128]].concat();
    let v2 = [vec![7u8; 128], vec![8u8; 128]].concat();
    let url = serve_versioned_resource(vec![("v1", v1.clone()), ("v2", v2.clone())], true).await;

    let download_dir = tempfile::tempdir().expect("download dir");
    let (_temp, core) = resume_core(download_dir.path());
    let post_id = seed_post(&core, &url, "mp4");
    let task_id = paused_task(&core, &post_id).await;

    stage_partial_with(
        &core,
        &task_id,
        &v1[..128],
        v1.len() as u64,
        douyin_downloader_lib::downloads::partial::ResourceValidator::ETag {
            value: "\"v1\"".to_string(),
        },
    )
    .await;

    let rx = core.downloads().subscribe();
    core.downloads().resume(&task_id).await.expect("resume");
    assert_eq!(wait_for_terminal(rx, &task_id).await, DownloadState::Completed);

    let stored = core.downloads().get(&task_id).await.unwrap().unwrap();
    let saved = stored.destination_path.expect("a saved path");
    assert_eq!(std::fs::read(&saved).unwrap(), v2, "the file is one version");

    let history = core
        .database()
        .with_conn(|conn| history_repo::list(conn, 10, 0))
        .unwrap();
    assert_eq!(history.len(), 1, "a restart must not duplicate the history");
    assert_eq!(history[0].status, DownloadState::Completed);
    assert_eq!(history[0].local_path.as_deref(), Some(saved.as_str()));
}

