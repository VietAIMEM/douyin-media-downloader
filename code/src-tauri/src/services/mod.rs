//! Application services — the composition root.
//!
//! [`AppCore`] is where the layers are wired together: it opens the database,
//! builds the settings service, installs logging, constructs the resolver
//! registry, and hands out the download and storage managers. Every other
//! module receives its dependencies through [`AppCore`] rather than creating
//! them, which keeps construction order (and the reason for it) in one place.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use crate::config::AppPaths;
use crate::database::Database;
use crate::downloads::{DownloadManager, QueueDownloadManager};
use crate::errors::AppResult;
use crate::logging::{self, LoggingConfig, LoggingGuard};
use crate::media::{FfmpegMuxer, MediaMuxer, NoopMuxer};
use crate::models::{Platform, Timestamp};
use crate::network::{ChromiumRenderer, HttpClient, NoopRenderer, PageRenderer, ReqwestHttpClient};
use crate::resolvers::{
    DouyinResolver, DouyinUrlParser, FixtureResolver, ResolverInfo, ResolverRegistry,
};
use crate::settings::SettingsService;
use crate::storage::{LocalStorageManager, StorageManager};

pub mod browser;
pub mod dashboard;
pub mod files;
pub mod library;
pub mod profile;
pub mod single_url;
pub mod text;

pub use browser::SettingGatedRenderer;
pub use library::{ExportFormat, ExportResult, ExportScope, LibraryQuery, Work};
pub use profile::{
    ProfileDownloadResult, ProfileIdentity, ProfilePostView, ProfilePostsPage,
    SubmitProfileDownloadsInput,
};
pub use single_url::{AnalyzedUrl, CreateDownloadInput, UrlClassification};

/// Product name shown to the user.
pub const APP_NAME: &str = "Douyin Downloader";
/// Bundle identifier, mirrored in `tauri.conf.json`.
pub const APP_IDENTIFIER: &str = "com.douyindownloader.desktop";

/// Set this to `1` to register the offline fixture resolver ahead of the live
/// Douyin resolver. Used for UI development without network access.
pub const DEMO_ENV_VAR: &str = "DOUYIN_DOWNLOADER_DEMO";

/// The wired-up application.
pub struct AppCore {
    paths: AppPaths,
    database: Database,
    settings: Arc<SettingsService>,
    resolvers: ResolverRegistry,
    downloads: Arc<dyn DownloadManager>,
    storage: Arc<dyn StorageManager>,
    /// Combines a video track with a separately served audio track.
    muxer: Arc<dyn MediaMuxer>,
    /// Makes the small preview pictures the Dashboard draws.
    ///
    /// Held here rather than built per request so the FFmpeg lookup happens
    /// once, and so every caller shares one cache directory.
    thumbnails: crate::thumbnails::Thumbnailer,
    /// The browser-assisted renderer, held here as well as in the registry so
    /// the exit path can close whatever browser it is keeping alive. The
    /// registry hands out resolvers, not the resources behind them.
    renderer: Arc<dyn PageRenderer>,
    /// The same renderer as a browser, when there is one.
    ///
    /// The sign-in screen needs operations the `PageRenderer` interface has no
    /// business knowing about — opening a window the user types into, asking
    /// the profile what it is holding, closing it before the session is
    /// cleared. `None` on a machine with no browser, which is exactly when
    /// there is nothing to sign in with.
    browser: Option<Arc<ChromiumRenderer>>,
    http: Arc<dyn HttpClient>,
    demo_mode: bool,
    started_at: Timestamp,
}

/// Result of [`AppCore::bootstrap`].
///
/// The logging guard is returned separately because it must be owned by
/// `main`/`run` for the whole process lifetime; storing it inside `AppCore`
/// would work but would make the ownership requirement less obvious.
pub struct Bootstrap {
    pub core: AppCore,
    pub logging_guard: LoggingGuard,
}

impl AppCore {
    /// Build the application from resolved paths.
    ///
    /// Order matters: the database must exist before settings can be read, and
    /// settings determine the log level and network timeout.
    pub fn bootstrap(paths: AppPaths) -> AppResult<Bootstrap> {
        Self::bootstrap_with(paths, None, None, None)
    }

    /// Build the application, optionally substituting the HTTP transport, the
    /// page renderer, and the media muxer.
    ///
    /// Each is injected for the same reason: the real one depends on something
    /// outside this process — a network, an installed browser, an installed
    /// media tool — and a test that reaches for those is slow, machine-dependent,
    /// and no longer a test of the code under it.
    pub fn bootstrap_with(
        paths: AppPaths,
        http_override: Option<Arc<dyn HttpClient>>,
        renderer_override: Option<Arc<dyn PageRenderer>>,
        muxer_override: Option<Arc<dyn MediaMuxer>>,
    ) -> AppResult<Bootstrap> {
        paths.ensure_directories()?;

        let database = Database::open(&paths.database_path)?;
        let settings = Arc::new(SettingsService::new(database.clone())?);

        let current = settings.current();
        let logging_guard = logging::init(&LoggingConfig::from_settings(
            current.log_level,
            current.log_to_file,
            &paths.data_dir,
        ))?;

        tracing::info!(
            target: "startup",
            version = env!("CARGO_PKG_VERSION"),
            database = %paths.database_path.display(),
            "application starting"
        );

        let http: Arc<dyn HttpClient> = match http_override {
            Some(client) => client,
            None => Arc::new(ReqwestHttpClient::new(current.request_timeout_seconds)?),
        };
        let demo_mode = demo_mode_enabled();
        let (renderer, browser) = match renderer_override {
            // A caller that supplies its own renderer is a test; there is no
            // browser behind it to sign in with, and pretending otherwise would
            // have the sign-in screen promise a window that cannot exist.
            Some(renderer) => (renderer, None),
            None => build_renderer(
                Arc::clone(&settings),
                Arc::clone(&http),
                paths.auth_profile_dir.clone(),
            ),
        };
        let resolvers = build_registry(Arc::clone(&http), Arc::clone(&renderer), demo_mode);

        let storage: Arc<dyn StorageManager> = Arc::new(LocalStorageManager::new());
        let muxer: Arc<dyn MediaMuxer> = match muxer_override {
            Some(muxer) => muxer,
            None => build_muxer(),
        };

        // The manager and the transfer worker share one event channel, so a
        // subscriber sees a single ordered stream whether a frame came from the
        // state machine or from the byte transfer.
        let download_manager = QueueDownloadManager::with_worker(
            database.clone(),
            Arc::clone(&settings),
            Arc::clone(&http),
            Arc::clone(&storage),
            Arc::clone(&muxer),
        );
        let recovered = download_manager.recover_interrupted()?;
        if recovered > 0 {
            tracing::info!(target: "startup", recovered, "recovered interrupted downloads");
        }

        // Queued work is handed back to the scheduler once the application is
        // running: queue state survives a restart, the worker pool does not, and
        // there is no runtime to start one on yet at this point. See
        // `spawn_download_scheduler` in the binary's entry point.

        tracing::info!(
            target: "startup",
            demo_mode,
            resolvers = resolvers.describe().len(),
            "application services ready"
        );

        // Preview pictures are written into the application's own data
        // directory. Nothing this application generates is ever put beside the
        // user's media, and the originals are only ever read.
        let thumbnails = crate::thumbnails::Thumbnailer::discover(paths.thumbnail_dir.clone());

        Ok(Bootstrap {
            core: Self {
                paths,
                database,
                settings,
                resolvers,
                renderer,
                browser,
                downloads: download_manager,
                storage,
                muxer,
                thumbnails,
                http,
                demo_mode,
                started_at: crate::models::common::now(),
            },
            logging_guard,
        })
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn data_directory(&self) -> &Path {
        &self.paths.data_dir
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn settings(&self) -> Arc<SettingsService> {
        Arc::clone(&self.settings)
    }

    pub fn resolvers(&self) -> &ResolverRegistry {
        &self.resolvers
    }

    /// The browser-assisted renderer, for callers that need the resource itself
    /// rather than a resolver built on it.
    pub fn renderer(&self) -> Arc<dyn PageRenderer> {
        Arc::clone(&self.renderer)
    }

    /// The browser the sign-in screen drives, or `None` when there is none.
    pub fn browser(&self) -> Option<Arc<ChromiumRenderer>> {
        self.browser.clone()
    }

    /// Close any browser this application started, without awaiting.
    ///
    /// Called on the way out. Waiting for `Drop` would be enough on an ordinary
    /// exit, but the updater's restart leaves the process without running
    /// destructors at all, and a browser left running is a process the user
    /// finds in Task Manager and cannot attribute to anything.
    pub fn shutdown_browser(&self) {
        self.renderer.shutdown_now();
    }

    pub fn downloads(&self) -> Arc<dyn DownloadManager> {
        Arc::clone(&self.downloads)
    }

    pub fn storage(&self) -> Arc<dyn StorageManager> {
        Arc::clone(&self.storage)
    }

    /// The muxer available to downloads, for callers that need to know whether
    /// two tracks can be combined before starting.
    pub fn muxer(&self) -> Arc<dyn MediaMuxer> {
        Arc::clone(&self.muxer)
    }

    /// The preview maker the Dashboard draws from.
    pub fn thumbnails(&self) -> &crate::thumbnails::Thumbnailer {
        &self.thumbnails
    }

    pub fn http(&self) -> Arc<dyn HttpClient> {
        Arc::clone(&self.http)
    }

    pub fn demo_mode(&self) -> bool {
        self.demo_mode
    }

    /// Snapshot used by the `get_app_info` command.
    ///
    /// `update_channel_configured` is the one thing here that is a property of
    /// the bundled configuration rather than of the running services, so the
    /// caller supplies it — see `commands::app_info`.
    pub fn app_info(&self, update_channel_configured: bool) -> AppInfo {
        let current = self.settings.current();
        AppInfo {
            name: APP_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            identifier: APP_IDENTIFIER.to_string(),
            os: std::env::consts::OS.to_string(),
            architecture: std::env::consts::ARCH.to_string(),
            data_directory: self.paths.data_dir.to_string_lossy().into_owned(),
            database_path: self.paths.database_path.to_string_lossy().into_owned(),
            log_directory: self.paths.log_dir.to_string_lossy().into_owned(),
            schema_version: self.database.latest_schema_version(),
            log_level: current.log_level.as_str().to_string(),
            demo_mode: self.demo_mode,
            update_channel_configured,
            supported_platforms: vec![Platform::Douyin.as_str().to_string()],
            resolvers: self.resolvers.describe(),
            started_at: self.started_at.to_rfc3339(),
        }
    }
}

/// Build the resolver registry.
///
/// The live Douyin resolver is always registered last, so an explicitly enabled
/// fixture resolver can take precedence for its own fixture URLs without
/// affecting any other input.
pub fn build_registry(
    http: Arc<dyn HttpClient>,
    renderer: Arc<dyn PageRenderer>,
    demo_mode: bool,
) -> ResolverRegistry {
    let mut registry = ResolverRegistry::new()
        .register_parser(DouyinUrlParser::new())
        .with_http_client(Arc::clone(&http));

    if demo_mode {
        let fixture = Arc::new(FixtureResolver::standard());
        registry = registry
            .register_profile_resolver(fixture.clone())
            .register_post_resolver(fixture.clone())
            .register_media_resolver(fixture);
    }

    let douyin = Arc::new(DouyinResolver::with_renderer(http, renderer));
    registry
        .register_profile_resolver(douyin.clone())
        .register_post_resolver(douyin.clone())
        .register_media_resolver(douyin)
}

/// Build the media muxer, or one that is never available.
///
/// Discovery happens once at start-up. A machine without FFmpeg gets a muxer
/// that reports itself unavailable, so a download that needs two tracks is
/// refused with a message naming FFmpeg rather than quietly producing a silent
/// video.
fn build_muxer() -> Arc<dyn MediaMuxer> {
    match FfmpegMuxer::discover() {
        Some(muxer) => {
            tracing::info!(
                target: "startup",
                ffmpeg = %muxer.executable().display(),
                "media muxing available"
            );
            Arc::new(muxer)
        }
        None => {
            tracing::info!(
                target: "startup",
                "no FFmpeg found; videos whose audio is a separate track cannot be combined"
            );
            Arc::new(NoopMuxer)
        }
    }
}

/// Build the browser renderer, or a renderer that is never available.
///
/// Discovery happens once at start-up. A machine with no supported browser
/// simply gets a renderer that reports itself unavailable, so the resolver skips
/// the fallback rather than reporting an error about a browser the user never
/// asked for.
fn build_renderer(
    settings: Arc<SettingsService>,
    http: Arc<dyn HttpClient>,
    auth_profile: PathBuf,
) -> (Arc<dyn PageRenderer>, Option<Arc<ChromiumRenderer>>) {
    match ChromiumRenderer::discover(http) {
        Some(renderer) => {
            let renderer = Arc::new(renderer.with_auth_profile(auth_profile));
            tracing::info!(
                target: "startup",
                browser = %renderer.executable().display(),
                "browser-assisted resolution available"
            );
            // The same renderer the resolvers use, handed on as well: signing
            // in has to open a browser that the next profile load will find
            // waiting, and that only works if it is one object.
            let gated = Arc::new(SettingGatedRenderer::new(
                Arc::clone(&renderer) as Arc<dyn PageRenderer>,
                settings,
            ));
            (gated, Some(renderer))
        }
        None => {
            tracing::info!(
                target: "startup",
                "no supported browser found; resolution is limited to the plain HTTP path"
            );
            (Arc::new(NoopRenderer), None)
        }
    }
}

fn demo_mode_enabled() -> bool {
    matches!(
        std::env::var(DEMO_ENV_VAR).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Runtime information returned to the frontend by `get_app_info`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub identifier: String,
    pub os: String,
    pub architecture: String,
    pub data_directory: String,
    pub database_path: String,
    pub log_directory: String,
    pub schema_version: i64,
    pub log_level: String,
    pub demo_mode: bool,
    /// Whether this build carries an update feed. A build without one reports
    /// that rather than offering a check that could only fail.
    pub update_channel_configured: bool,
    pub supported_platforms: Vec<String>,
    pub resolvers: Vec<ResolverInfo>,
    pub started_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::MockHttpClient;

    fn test_paths() -> (tempfile::TempDir, AppPaths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_data_dir(temp.path().to_path_buf());
        (temp, paths)
    }

    #[test]
    fn bootstrap_wires_every_service() {
        let (_temp, paths) = test_paths();
        let bootstrap = AppCore::bootstrap(paths).unwrap();
        let core = bootstrap.core;

        assert!(core.database().schema_version().unwrap() >= 1);
        assert_eq!(core.storage().name(), "local-filesystem");
        assert_eq!(core.downloads().name(), "queue");
        assert!(!core.settings().current().download_directory.is_empty());
    }

    #[test]
    fn app_info_reports_the_douyin_platform_and_resolvers() {
        let (_temp, paths) = test_paths();
        let core = AppCore::bootstrap(paths).unwrap().core;
        let info = core.app_info(false);

        assert_eq!(info.name, APP_NAME);
        assert_eq!(info.supported_platforms, vec!["douyin".to_string()]);
        assert_eq!(info.os, "windows");
        // parser + profile + post + media
        assert_eq!(info.resolvers.len(), 4);
        assert!(info
            .resolvers
            .iter()
            .any(|r| r.capability == "media" && r.platform == Platform::Douyin));
    }

    #[test]
    fn registry_prefers_fixtures_in_demo_mode() {
        let http: Arc<dyn HttpClient> = Arc::new(MockHttpClient::new());
        let registry = build_registry(http, Arc::new(NoopRenderer), true);
        let source = registry
            .parse(crate::resolvers::fixture::FIXTURE_VIDEO_URL)
            .unwrap();
        let resolver = registry.post_resolver_for(&source).unwrap();
        assert_eq!(resolver.name(), "fixture");
    }

    #[test]
    fn registry_uses_the_live_resolver_by_default() {
        let http: Arc<dyn HttpClient> = Arc::new(MockHttpClient::new());
        let registry = build_registry(http, Arc::new(NoopRenderer), false);
        let source = registry
            .parse(crate::resolvers::fixture::FIXTURE_VIDEO_URL)
            .unwrap();
        let resolver = registry.post_resolver_for(&source).unwrap();
        assert_eq!(resolver.name(), "douyin-url");
    }

    #[test]
    fn bootstrap_survives_a_second_start_over_the_same_directory() {
        let (_temp, paths) = test_paths();
        let first = AppCore::bootstrap(paths.clone()).unwrap().core;
        first
            .settings()
            .update(|s| {
                s.max_concurrent_downloads = 4;
                Ok(())
            })
            .unwrap();
        drop(first);

        // Reopening the same data directory must see the persisted setting.
        let second = AppCore::bootstrap(paths).unwrap().core;
        assert_eq!(second.settings().current().max_concurrent_downloads, 4);
    }
}
