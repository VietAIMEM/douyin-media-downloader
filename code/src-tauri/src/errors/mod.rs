//! Application-wide error model.
//!
//! Every fallible operation in the application core returns [`AppResult`], so
//! there is exactly one error type crossing module boundaries. The model keeps
//! two layers of information on purpose:
//!
//! * **technical** — developer-facing text and the original source error, kept
//!   for logs. Never rendered to the user.
//! * **user-facing** — a short, safe message derived from the error *kind* so
//!   the UI can present something actionable without leaking internals.
//!
//! Errors are serialized for the frontend as [`SerializedError`], a stable
//! structure (kind + code + message) rather than a bare string, so the UI can
//! branch on `kind` without parsing prose.

use std::fmt;

use serde::Serialize;

/// Coarse classification of an error, shared with the frontend.
///
/// The set mirrors the categories required for user-facing error handling and
/// is deliberately small: new variants should only be added when the UI needs
/// to react differently to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppErrorKind {
    /// The caller supplied input that failed validation (URL, settings, path).
    Validation,
    /// A network request failed or timed out.
    Network,
    /// A URL could not be resolved into domain data.
    Resolution,
    /// A download operation failed.
    Download,
    /// A filesystem/storage operation failed.
    Storage,
    /// A database operation failed.
    Database,
    /// The content is a kind the application does not support.
    UnsupportedContent,
    /// The content exists but is unavailable/private/deleted.
    UnavailableContent,
    /// A browser was needed to read the content and none could be used.
    BrowserUnavailable,
    /// The source would not show this content without a signed-in account.
    ///
    /// Separate from `UnavailableContent` because the recovery is different and
    /// is something the user can do: a private post is simply not there, an
    /// authentication prompt is the source asking a question the user can
    /// answer by signing in.
    AuthenticationRequired,
    /// Separate media tracks needed combining and no muxer could be used.
    MuxerUnavailable,
    /// A requested entity was not found.
    NotFound,
    /// Anything not covered above.
    Unknown,
}

impl AppErrorKind {
    /// Stable machine-readable code. Safe to persist or match on in the UI.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Validation => "validation_error",
            Self::Network => "network_error",
            Self::Resolution => "resolution_error",
            Self::Download => "download_error",
            Self::Storage => "storage_error",
            Self::Database => "database_error",
            Self::UnsupportedContent => "unsupported_content",
            Self::UnavailableContent => "unavailable_content",
            Self::BrowserUnavailable => "browser_unavailable",
            Self::AuthenticationRequired => "authentication_required",
            Self::MuxerUnavailable => "muxer_unavailable",
            Self::NotFound => "not_found",
            Self::Unknown => "unknown_error",
        }
    }

    /// Whether an error of this kind is worth retrying by default.
    ///
    /// Retrying is only meaningful for failures that can plausibly succeed on a
    /// second attempt: a dropped connection, a throttled or overloaded server.
    /// A validation failure, an unsupported content type, or a missing resource
    /// will fail identically every time, so the retry scheduler must not spend
    /// attempts on them.
    ///
    /// This is a *default*: a specific call site can override it via
    /// [`AppError::with_retryable`] when it knows better (for example a `404`
    /// during a transfer is permanent even though transfers in general are
    /// retryable).
    pub const fn default_retryable(self) -> bool {
        match self {
            // Transient by nature.
            Self::Network | Self::Download => true,
            // Deterministic given the same input.
            Self::Validation
            | Self::Resolution
            | Self::Storage
            | Self::Database
            | Self::UnsupportedContent
            | Self::UnavailableContent
            | Self::BrowserUnavailable
            | Self::MuxerUnavailable
            | Self::NotFound
            // Asked again, the source gives the same answer, and a render that
            // kept retrying a sign-in wall is the endless loop this must not
            // become. Signing in changes the answer; repeating does not.
            | Self::AuthenticationRequired
            | Self::Unknown => false,
        }
    }

    /// Default safe user-facing message for this kind.
    ///
    /// This is the fallback when a call site does not supply a more specific
    /// (but equally safe) message via [`AppError::with_user_message`].
    pub const fn default_user_message(self) -> &'static str {
        match self {
            Self::Validation => "Please check the information you entered and try again.",
            Self::Network => "A network problem prevented this operation. Check your connection and try again.",
            Self::Resolution => "This content could not be analyzed. It may be in an unsupported format.",
            Self::Download => "The file could not be downloaded. You can retry the download.",
            Self::Storage => "The file could not be saved to disk. Check the download folder and available space.",
            Self::Database => "The local database could not be updated. Please restart the application.",
            Self::UnsupportedContent => "This type of content is not supported.",
            Self::UnavailableContent => "This content is no longer available.",
            Self::BrowserUnavailable => {
                "This content needs a browser to read it, and no supported browser was found. \
                 Install Chrome or Edge, or point the application at one."
            }
            Self::MuxerUnavailable => {
                "This video keeps its sound in a separate track, and putting the two together \
                 needs FFmpeg. Install FFmpeg, or point the application at an ffmpeg executable."
            }
            Self::AuthenticationRequired => {
                "Douyin asked this visitor to sign in before it would show this. Sign in from \
                 the Profile Downloader, then load the profile again."
            }
            Self::NotFound => "The requested item could not be found.",
            Self::Unknown => "Something went wrong. Please try again.",
        }
    }
}

/// The single error type used across the application core.
#[derive(Debug)]
pub struct AppError {
    kind: AppErrorKind,
    /// Developer-facing message. Safe for logs, never for the UI directly.
    message: String,
    /// Optional override for the message shown to users. Callers must ensure
    /// this text contains no internal detail.
    user_message: Option<String>,
    /// Optional technical detail (e.g. status code, path). Debug-only in the
    /// serialized form.
    technical_detail: Option<String>,
    /// Whether a retry could plausibly succeed. Drives the download retry
    /// scheduler; see [`AppErrorKind::default_retryable`].
    retryable: bool,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl AppError {
    /// Create an error of the given kind with a technical message.
    pub fn new(kind: AppErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            user_message: None,
            technical_detail: None,
            retryable: kind.default_retryable(),
            source: None,
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Validation, message)
    }
    pub fn network(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Network, message)
    }
    pub fn resolution(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Resolution, message)
    }
    pub fn download(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Download, message)
    }
    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Storage, message)
    }
    pub fn database(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Database, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::UnsupportedContent, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::UnavailableContent, message)
    }
    /// A page needed a browser, and none could be used.
    pub fn browser_unavailable(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::BrowserUnavailable, message)
    }
    /// The source asked for a signed-in visitor.
    pub fn authentication_required(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::AuthenticationRequired, message)
    }
    /// Separate tracks needed combining, and no muxer could be used.
    pub fn muxer_unavailable(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::MuxerUnavailable, message)
    }
    pub fn not_found(entity: &str, id: impl fmt::Display) -> Self {
        Self::new(AppErrorKind::NotFound, format!("{entity} not found: {id}"))
    }
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(AppErrorKind::Unknown, message)
    }

    /// Override the message shown to the user. `text` must be safe to display.
    pub fn with_user_message(mut self, text: impl Into<String>) -> Self {
        self.user_message = Some(text.into());
        self
    }

    /// Attach technical detail used for debugging (status codes, paths...).
    pub fn with_technical_detail(mut self, detail: impl Into<String>) -> Self {
        self.technical_detail = Some(detail.into());
        self
    }

    /// Override whether this error is retryable.
    ///
    /// Call sites that can tell a permanent failure from a transient one — an
    /// HTTP status code, for instance — set this explicitly rather than relying
    /// on the per-kind default.
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Attach the originating error, preserving the source chain for logs.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn kind(&self) -> AppErrorKind {
        self.kind
    }

    /// Developer-facing message for logs.
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn technical_detail(&self) -> Option<&str> {
        self.technical_detail.as_deref()
    }

    /// Whether a retry could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }

    /// Safe message suitable for display in the UI.
    pub fn user_message(&self) -> String {
        self.user_message
            .clone()
            .unwrap_or_else(|| self.kind.default_user_message().to_string())
    }

    /// Convert into the frontend wire format.
    pub fn to_serialized(&self) -> SerializedError {
        SerializedError {
            kind: self.kind,
            code: self.kind.code().to_string(),
            message: self.user_message(),
            details: if cfg!(debug_assertions) {
                self.technical_detail.clone()
            } else {
                None
            },
            retryable: self.retryable,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for AppError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// Wire representation of an error sent to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct SerializedError {
    pub kind: AppErrorKind,
    /// Stable machine-readable code, e.g. `validation_error`.
    pub code: String,
    /// Safe, user-facing message.
    pub message: String,
    /// Technical detail. Populated only in debug builds so production users
    /// never see internal implementation details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Whether trying the same thing again could reasonably work.
    ///
    /// The backend classifies every failure this way for its own retries; the
    /// frontend needs the same answer, because a screen that offers "try again"
    /// or that pages through a listing has to know which failures are worth
    /// repeating and which are the source refusing.
    pub retryable: bool,
}

impl fmt::Display for SerializedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Tauri command result alias.
pub type AppResult<T> = Result<T, AppError>;

/// Tauri command return type: errors travel as structured JSON.
pub type CommandResult<T> = Result<T, SerializedError>;

/// Helper so command bodies can use `?` on `AppResult` and still return
/// `CommandResult`.
pub fn to_command_error(err: AppError) -> SerializedError {
    err.to_serialized()
}

// --- Conversions from lower layers ----------------------------------------
//
// These are the single place where third-party errors become application
// errors, so the rest of the code never has to know about rusqlite/reqwest.

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        AppError::database("database operation failed")
            .with_technical_detail(err.to_string())
            .with_source(err)
    }
}

impl From<r2d2::Error> for AppError {
    fn from(err: r2d2::Error) -> Self {
        AppError::database("could not acquire a database connection")
            .with_technical_detail(err.to_string())
            .with_source(err)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        let detail = if err.is_timeout() {
            "request timed out"
        } else if err.is_connect() {
            "connection failed"
        } else if err.is_status() {
            "unexpected status code"
        } else {
            "request failed"
        };
        AppError::network("network request failed")
            .with_technical_detail(detail)
            .with_source(err)
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::storage("filesystem operation failed")
            .with_technical_detail(err.to_string())
            .with_source(err)
    }
}

impl From<url::ParseError> for AppError {
    fn from(err: url::ParseError) -> Self {
        AppError::validation("the URL could not be parsed")
            .with_technical_detail(err.to_string())
            .with_source(err)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        AppError::unknown("failed to (de)serialize data")
            .with_technical_detail(err.to_string())
            .with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_have_stable_codes() {
        assert_eq!(AppErrorKind::Validation.code(), "validation_error");
        assert_eq!(AppErrorKind::Database.code(), "database_error");
        assert_eq!(
            AppErrorKind::BrowserUnavailable.code(),
            "browser_unavailable"
        );
        assert_eq!(AppErrorKind::Unknown.code(), "unknown_error");
    }

    #[test]
    fn browser_unavailable_is_not_retryable_and_says_what_to_do() {
        let err = AppError::browser_unavailable("no chrome found");
        assert_eq!(err.kind(), AppErrorKind::BrowserUnavailable);
        // Retrying will not install a browser.
        assert!(!err.is_retryable());
        // And the user is told what would fix it.
        assert!(err.user_message().contains("Chrome"));
    }

    #[test]
    fn user_message_falls_back_to_kind_default() {
        let err = AppError::network("connect refused at 10.0.0.1:443");
        assert_eq!(err.kind(), AppErrorKind::Network);
        // Technical text must not leak into the user-facing message.
        assert!(!err.user_message().contains("10.0.0.1"));
        assert_eq!(
            err.user_message(),
            AppErrorKind::Network.default_user_message()
        );
    }

    #[test]
    fn explicit_user_message_wins() {
        let err = AppError::validation("field `download_dir` must be absolute")
            .with_user_message("Choose a valid download folder.");
        assert_eq!(err.user_message(), "Choose a valid download folder.");
    }

    #[test]
    fn serialized_error_hides_details_in_release_shape() {
        let err = AppError::database("boom").with_technical_detail("SQLITE_BUSY");
        let wire = err.to_serialized();
        assert_eq!(wire.code, "database_error");
        // In debug builds details are present; the field must never be the
        // user message itself.
        assert_eq!(wire.message, AppErrorKind::Database.default_user_message());
    }

    #[test]
    fn io_error_maps_to_storage() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: AppError = io.into();
        assert_eq!(err.kind(), AppErrorKind::Storage);
    }

    #[test]
    fn source_chain_is_preserved() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err: AppError = io.into();
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn retryability_defaults_follow_the_kind() {
        assert!(AppError::network("dropped").is_retryable());
        assert!(AppError::download("interrupted").is_retryable());
        assert!(!AppError::validation("bad input").is_retryable());
        assert!(!AppError::unsupported("nope").is_retryable());
        assert!(!AppError::unavailable("gone").is_retryable());
        assert!(!AppError::not_found("post", "1").is_retryable());
    }

    #[test]
    fn retryability_can_be_overridden_per_call_site() {
        // A 404 during a transfer: the kind is retryable, the instance is not.
        let err = AppError::download("status 404").with_retryable(false);
        assert!(!err.is_retryable());
        // And the inverse, for a resolution failure that is worth another go.
        let err = AppError::resolution("temporary").with_retryable(true);
        assert!(err.is_retryable());
    }
}
