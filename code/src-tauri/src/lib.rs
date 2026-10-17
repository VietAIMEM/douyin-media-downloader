//! Douyin Downloader — application core and Tauri entry point.
//!
//! # Layering
//!
//! ```text
//! React UI
//!   │  (only through Tauri commands)
//!   ▼
//! commands/            thin adapters: state in, wire format out
//!   ▼
//! services::AppCore    composition root — the only place things are wired
//!   ├── resolvers/     platform knowledge (Douyin) behind 3 interfaces
//!   ├── downloads/     download task lifecycle
//!   ├── storage/       where files land, and under what name
//!   ├── settings/      validated, persisted preferences
//!   ├── database/      SQLite access + migrations
//!   ├── network/       HTTP seam (reqwest lives only here)
//!   └── errors/        one error type for the whole core
//! ```
//!
//! Dependencies point downward only. No module below `commands/` knows Tauri
//! exists, which is why the whole core is testable without a webview, and no
//! module above `resolvers/` knows what a Douyin response looks like.
//!
//! The crate is a library (`douyin_downloader_lib`) with a thin binary on top,
//! so integration tests can drive the real application core directly.

pub mod app_state;
pub mod commands;
pub mod config;
pub mod database;
pub mod downloads;
pub mod errors;
pub mod logging;
pub mod media;
pub mod models;
pub mod network;
pub mod process;
pub mod resolvers;
pub mod services;
pub mod settings;
pub mod startup;
pub mod storage;
pub mod thumbnails;

/// Rules the library's own tests follow so they cannot write into the user's
/// files. Test-only: the product has no use for it.
#[cfg(test)]
pub mod test_isolation;

use parking_lot::Mutex;

use app_state::AppState;
use config::AppPaths;
use errors::AppError;
use services::{AppCore, APP_NAME};

/// Tauri event name carrying download lifecycle updates to the frontend.
///
/// One stream for the whole application: queue, progress, completion, and
/// failure all arrive here, so the UI has a single subscription to manage.
pub const DOWNLOAD_EVENT_NAME: &str = "download://event";

/// Start the desktop application.
///
/// Startup order is deliberate: paths → database → settings → logging →
/// network → resolvers → downloads → storage. Settings must be readable before
/// logging is installed (the level comes from them), and pre-existing
/// interrupted downloads are requeued before the UI can observe the queue.
pub fn run() {
    // Installed first, because the failure it explains happens before anything
    // else here can run: Tauri creates the window inside its own event loop and
    // panics if it cannot, so the error never reaches this function. Without
    // this, a locked WebView2 profile is reported as a panic naming a path in
    // `.cargo\registry`. See `startup` for what was measured and why nothing is
    // killed automatically. Every other panic keeps its default reporting.
    startup::install_panic_hook();

    let paths = AppPaths::resolve().unwrap_or_else(|err| startup_failure(&err));

    let bootstrap = AppCore::bootstrap(paths).unwrap_or_else(|err| startup_failure(&err));
    let services::Bootstrap {
        core,
        logging_guard,
    } = bootstrap;

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // The native folder picker behind the Settings screen's "Browse…"
        // button. The dialog runs in the webview's own process, so the chosen
        // path still travels back through `save_settings` and is validated by
        // the backend like any other value.
        .plugin(tauri_plugin_dialog::init())
        // In-app updates. The plugin does the parts that must not be
        // reimplemented — fetching the release manifest, comparing versions, and
        // verifying a downloaded package against the public key compiled into
        // this build — and refuses anything that is not signed by the matching
        // private key. What the application decides is *when* to look and
        // whether to install, which is the Settings screen's job.
        //
        // Its endpoints and key come from the bundle configuration, which ships
        // without a feed: an unconfigured updater fails the check with a clear
        // message rather than reaching for a default location. See the release
        // notes for what the release owner has to supply.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Restarting after an update has been installed. The updater can
        // relaunch on its own, but only this plugin can do it on request, and
        // the update is finished by a restart the user can see.
        .plugin(tauri_plugin_process::init())
        .manage(AppState::new(core))
        .invoke_handler(tauri::generate_handler![
            commands::app_info::get_app_info,
            commands::dashboard::browse_media_folder,
            commands::dashboard::browse_media_folders,
            commands::dashboard::media_thumbnail,
            commands::dashboard::open_media_file,
            commands::database::get_database_status,
            commands::downloads::clear_completed_downloads,
            commands::downloads::get_download,
            commands::downloads::get_download_queue,
            commands::downloads::get_download_summary,
            commands::downloads::list_downloads,
            commands::downloads::pause_download,
            commands::downloads::remove_download,
            commands::downloads::resume_download,
            commands::downloads::retry_failed_downloads,
            commands::downloads::scan_downloaded_files,
            commands::history::get_history_count,
            commands::history::list_history,
            commands::library::export_library,
            commands::library::list_library_creators,
            commands::library::query_library,
            commands::library::reveal_export,
            commands::library::save_post_text,
            commands::profile::list_profile_posts,
            commands::profile::resolve_profile,
            commands::profile::submit_profile_downloads,
            commands::login::begin_login,
            commands::login::clear_login_session,
            commands::login::get_login_status,
            commands::settings::get_settings,
            commands::settings::get_settings_options,
            commands::settings::reset_settings,
            commands::settings::save_settings,
            commands::single_url::analyze_url,
            commands::single_url::cancel_download,
            commands::single_url::create_download_task,
            commands::single_url::open_download_path,
            commands::single_url::retry_download,
            commands::url::classify_url,
            commands::url::expand_url,
            commands::url::get_test_fixtures,
            commands::url::resolve_url_metadata,
        ])
        .setup(|app| {
            // The window may load media from the download folder and nowhere
            // else. Granted here so a picture in the Dashboard works on the
            // first visit, and re-granted whenever the folder is browsed, so a
            // changed setting takes effect without a restart.
            commands::dashboard::allow_the_download_folder(app.handle());
            spawn_download_event_bridge(app.handle());
            spawn_download_scheduler(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|err| {
            eprintln!("{APP_NAME} could not create its window: {err}");
            std::process::exit(1);
        });

    // The file log is written on a background thread; the guard flushes it on
    // drop. Releasing it as the app exits is what guarantees the final lines
    // reach disk.
    let guard = Mutex::new(Some(logging_guard));
    app.run(move |app, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            // Close a browser this application started, before the process goes
            // away. Waiting for `Drop` covers an ordinary exit and is what the
            // renderer already does; this covers the exits that do not run
            // destructors — the updater restarts the process directly — where
            // "there is a chrome.exe in Task Manager I cannot account for" is
            // exactly what the user is left with. Only this application's own
            // browser is closed: it is reached through the session the renderer
            // owns, and killing is by that process's id and its children.
            use tauri::Manager;
            app.state::<AppState>().core().shutdown_browser();
            *guard.lock() = None;
        }
    });
}

/// Report a fatal startup problem and exit.
///
/// Startup failures happen before a window exists, so there is nothing to show
/// a UI in. The message goes to stderr (visible in `tauri dev` and in a
/// console-launched release build) and the process exits non-zero.
fn startup_failure(err: &AppError) -> ! {
    eprintln!("{APP_NAME} could not start: {err}");
    if let Some(detail) = err.technical_detail() {
        eprintln!("  detail: {detail}");
    }
    std::process::exit(1);
}

/// Forward download lifecycle events to the frontend.
///
/// The download layer already publishes an ordered event stream; this is the
/// one place it crosses into the webview. Doing it here rather than having the
/// UI poll means progress reaches the screen as it happens, without the
/// frontend guessing an interval or the backend knowing about the UI.
///
/// Progress frames are already coalesced by the transfer worker, so what
/// crosses IPC is a bounded stream, not one message per chunk.
fn spawn_download_event_bridge(handle: &tauri::AppHandle) {
    use tauri::{Emitter, Manager};

    let downloads = handle.state::<AppState>().core().downloads();
    let handle = handle.clone();

    tauri::async_runtime::spawn(async move {
        let mut receiver = downloads.subscribe();
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    // A failed emit means no window is listening yet; the event
                    // is not worth retrying, and the next one will arrive.
                    let _ = handle.emit(DOWNLOAD_EVENT_NAME, &event);
                }
                // The subscriber fell behind: progress frames were dropped, but
                // the stream is still alive and the next frame is accurate.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(target: "downloads", skipped, "download event stream lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// Hand the persisted queue back to the download scheduler.
///
/// Queue state survives a restart; the worker pool does not. This runs once the
/// application is up — which is the first moment there is a runtime to start a
/// worker on — so a task that was queued when the application closed is picked
/// up rather than sitting in the list forever.
fn spawn_download_scheduler(handle: &tauri::AppHandle) {
    use tauri::Manager;

    let downloads = handle.state::<AppState>().core().downloads();

    tauri::async_runtime::spawn(async move {
        match downloads.schedule_pending().await {
            Ok(scheduled) if scheduled > 0 => {
                tracing::info!(target: "downloads", scheduled, "queued downloads rescheduled");
            }
            Ok(_) => {}
            Err(err) => {
                // Work left queued is not a reason to fail the window; it will
                // be picked up by the next start.
                tracing::warn!(
                    target: "downloads",
                    error = %err.message(),
                    "could not reschedule queued downloads"
                );
            }
        }
    });
}
