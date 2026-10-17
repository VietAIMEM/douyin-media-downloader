//! Structured application logging.
//!
//! Logging is initialized exactly once at startup and writes to two sinks:
//!
//! * **stderr** — always, for `tauri dev` and crash reports.
//! * **a daily rolling file** under the app log directory — unless disabled.
//!
//! Every log line carries a `target` (the module/area: `database`, `resolver`,
//! `downloads`, `settings`, ...) so output can be filtered per subsystem, and
//! the level is user-configurable in Settings.
//!
//! # What must never be logged
//!
//! Cookies, tokens, signed CDN URLs, or whole HTTP response bodies. Call sites
//! log ids, counts, states, and short technical details only — see the
//! `with_technical_detail` guidance in [`crate::errors`].

use std::path::{Path, PathBuf};

use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::errors::AppResult;
use crate::models::LogLevel;

/// Environment variable that overrides the configured level.
///
/// Useful for `RUST_LOG=debug npm run tauri:dev` without touching settings.
pub const LOG_ENV_VAR: &str = "RUST_LOG";

/// Log file name inside the log directory.
pub const LOG_FILE_NAME: &str = "douyin-downloader.log";

/// Keeps the async file appender alive.
///
/// The file writer flushes on a background thread; dropping this guard too
/// early truncates the tail of the log. Hold it for the process lifetime.
pub struct LoggingGuard {
    _file_guard: Option<WorkerGuard>,
}

impl LoggingGuard {
    /// A guard for a stderr-only setup.
    pub fn stderr_only() -> Self {
        Self { _file_guard: None }
    }
}

/// How to initialize logging.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// Verbosity when `RUST_LOG` is not set.
    pub level: LogLevel,
    /// Directory for the log file. `None` disables file logging.
    pub log_directory: Option<PathBuf>,
}

impl LoggingConfig {
    /// Build from application settings, placing logs next to the database.
    pub fn from_settings(log_level: LogLevel, log_to_file: bool, data_directory: &Path) -> Self {
        Self {
            level: log_level,
            log_directory: log_to_file.then(|| data_directory.join("logs")),
        }
    }
}

/// Map the settings log level onto a `tracing` level.
pub fn to_tracing_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Error => Level::ERROR,
        LogLevel::Warn => Level::WARN,
        LogLevel::Info => Level::INFO,
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Trace => Level::TRACE,
    }
}

/// Initialize the global subscriber.
///
/// Returns a guard that must be kept alive. If a subscriber is already
/// installed (for example a second call in tests), this is a no-op and a
/// stderr-only guard is returned.
pub fn init(config: &LoggingConfig) -> AppResult<LoggingGuard> {
    let filter = build_filter(config.level);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true)
        .compact()
        .with_filter(filter.clone());

    let registry = tracing_subscriber::registry().with(stderr_layer);

    // File logging is best-effort: if the directory cannot be created we keep
    // going with stderr only rather than refusing to start the application.
    let (file_layer, guard) = match &config.log_directory {
        Some(dir) => match prepare_log_directory(dir) {
            Ok(()) => {
                let appender = tracing_appender::rolling::daily(dir, LOG_FILE_NAME);
                let (writer, guard) = tracing_appender::non_blocking(appender);
                let layer = tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_target(true)
                    .with_filter(filter);
                (Some(layer), Some(guard))
            }
            Err(err) => {
                eprintln!(
                    "warning: file logging disabled ({err}); continuing with stderr only"
                );
                (None, None)
            }
        },
        None => (None, None),
    };

    match registry.with(file_layer).try_init() {
        Ok(()) => Ok(LoggingGuard {
            _file_guard: guard,
        }),
        Err(_) => Ok(LoggingGuard::stderr_only()),
    }
}

fn build_filter(level: LogLevel) -> EnvFilter {
    // A user-provided RUST_LOG always wins, so developers can raise verbosity
    // for one subsystem without editing settings.
    if std::env::var(LOG_ENV_VAR).is_ok() {
        return EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter(level));
    }
    default_filter(level)
}

fn default_filter(level: LogLevel) -> EnvFilter {
    // `off` for third-party crates by default: `reqwest`/`hyper` at debug level
    // are extremely noisy and can echo URLs.
    EnvFilter::new(format!(
        "{level},hyper=warn,reqwest=warn,rustls=warn,h2=warn,tokio=warn,want=warn,mio=warn"
    ))
}

fn prepare_log_directory(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// A URL reduced to the parts that are safe to log.
///
/// A Douyin *media* URL is a signed URL — its query string is a bearer token
/// (`token=`, `video_id=`, `expire=`) — and the resolver's landing URLs carry
/// share parameters of their own. Neither belongs in a log file that is written
/// to disk by default and mirrored to stderr.
///
/// Scheme, host, port, and path are kept, because those are what identify which
/// operation failed and against which host. Everything that can carry a
/// credential is dropped: the query, the fragment, and any userinfo before the
/// host.
///
/// A string that does not parse as a URL is replaced wholesale rather than
/// echoed. It is at least as likely to be a token as a URL, and there is no way
/// to tell from the string alone.
pub fn redact_url(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw) else {
        return "<unparseable-url>".to_string();
    };
    let host = parsed.host_str().unwrap_or("<no-host>");
    match parsed.port() {
        Some(port) => format!("{}://{host}:{port}{}", parsed.scheme(), parsed.path()),
        None => format!("{}://{host}{}", parsed.scheme(), parsed.path()),
    }
}

/// Just the host of a URL, for diagnostics that need no path either.
pub fn url_host(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| "<no-host>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_map_to_tracing_levels() {
        assert_eq!(to_tracing_level(LogLevel::Error), Level::ERROR);
        assert_eq!(to_tracing_level(LogLevel::Warn), Level::WARN);
        assert_eq!(to_tracing_level(LogLevel::Info), Level::INFO);
        assert_eq!(to_tracing_level(LogLevel::Debug), Level::DEBUG);
        assert_eq!(to_tracing_level(LogLevel::Trace), Level::TRACE);
    }

    #[test]
    fn a_signed_url_is_reduced_to_what_is_safe_to_log() {
        // A Douyin media URL: the query string is the credential.
        let signed = "https://v3-cdn.douyin.com/aweme/v1/playwm/?video_id=v0200&token=SECRET&expire=1";
        let safe = redact_url(signed);
        assert_eq!(safe, "https://v3-cdn.douyin.com/aweme/v1/playwm/");
        assert!(!safe.contains("SECRET"), "the token must not survive");
        assert!(!safe.contains("video_id"), "nor the parameters around it");
        // What is kept is what identifies the operation.
        assert!(safe.contains("douyin.com"));
        assert!(safe.contains("playwm"));
    }

    #[test]
    fn a_landing_url_loses_its_share_parameters() {
        let safe = redact_url("https://www.douyin.com/video/7123?share_sign=abc&from_ssr=1");
        assert_eq!(safe, "https://www.douyin.com/video/7123");
    }

    #[test]
    fn credentials_in_the_authority_are_dropped() {
        let safe = redact_url("https://user:hunter2@example.com/private/clip.mp4");
        assert!(!safe.contains("hunter2"), "userinfo must not survive: {safe}");
        assert!(safe.contains("example.com"));
    }

    #[test]
    fn a_port_is_kept_because_it_changes_which_endpoint_it_is() {
        assert_eq!(redact_url("http://example.com:8080/a/b?x=1"), "http://example.com:8080/a/b");
    }

    #[test]
    fn something_that_is_not_a_url_is_not_echoed() {
        // It is at least as likely to be a token as a URL, and there is no way
        // to tell from the string alone.
        for raw in ["not a url", "", "SECRET-VERY-SECRET"] {
            let safe = redact_url(raw);
            assert_eq!(safe, "<unparseable-url>");
            assert!(!safe.contains("SECRET"));
        }
    }

    #[test]
    fn the_host_helper_strips_everything_but_the_host() {
        assert_eq!(
            url_host("https://v3-web.douyinvod.com/a/b?sign=token"),
            "v3-web.douyinvod.com"
        );
        assert_eq!(url_host("not a url"), "<no-host>");
        assert!(!url_host("https://x.com/a?token=SECRET").contains("SECRET"));
    }

    #[test]
    fn config_disables_file_logging_when_requested() {
        let config = LoggingConfig::from_settings(LogLevel::Info, false, Path::new("C:\\data"));
        assert!(config.log_directory.is_none());
    }

    #[test]
    fn config_places_logs_under_the_data_directory() {
        let config = LoggingConfig::from_settings(LogLevel::Info, true, Path::new("C:\\data"));
        assert!(config.log_directory.unwrap().ends_with("logs"));
    }

    #[test]
    fn default_filter_describes_the_configured_level() {
        let rendered = default_filter(LogLevel::Debug).to_string();
        assert!(rendered.contains("debug"), "got {rendered}");
        assert!(rendered.contains("reqwest=warn"));
    }
}
