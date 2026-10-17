//! Primitive, shared value types used by every domain model.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::errors::AppError;

/// Local identifier. Always a string so the storage layer can adopt any
/// scheme (UUID today, stable platform ids later) without a migration.
pub type Id = String;

/// UTC timestamp. Stored as RFC 3339 text in SQLite.
pub type Timestamp = DateTime<Utc>;

/// Generate a new opaque local identifier.
pub fn new_id() -> Id {
    Uuid::new_v4().to_string()
}

/// Current time in UTC. Single indirection so time can be faked in tests.
pub fn now() -> Timestamp {
    Utc::now()
}

/// Free-form key/value metadata attached to posts and media items.
///
/// Kept as JSON so platform-specific extras can be carried through the domain
/// layer without teaching the models about raw Douyin response shapes.
pub type Metadata = std::collections::BTreeMap<String, serde_json::Value>;

/// Declares a unit enum that round-trips to/from a stable lowercase string.
///
/// The string form is what gets written to SQLite, so it must stay backwards
/// compatible: rename the Rust variant, keep the literal.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident => $lit:literal),+ $(,)?
    }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$variant_meta])* $variant),+
        }

        impl $name {
            /// Stable string form used for persistence and the wire format.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $lit),+
                }
            }

            /// All variants, in declaration order.
            pub const fn all() -> &'static [$name] {
                &[$(Self::$variant),+]
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = AppError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($lit => Ok(Self::$variant),)+
                    other => Err(AppError::validation(format!(
                        "unknown {} value: `{other}`",
                        stringify!($name)
                    ))),
                }
            }
        }
    };
}

string_enum!(
    /// Source platform of a creator/post.
    Platform {
        Douyin => "douyin",
        Unknown => "unknown",
    }
);

string_enum!(
    /// Kind of media a post or media item contains.
    ///
    /// `Text` is a post whose content is its words and nothing else — a
    /// caption-only post, which Douyin publishes and which is neither a video
    /// nor a picture. It is deliberately not folded into one of the others: a
    /// post given a media type it does not have would be offered a download it
    /// cannot perform, and calling it a video is how a text post came to be
    /// reported as a failed download.
    ///
    /// Nothing in the download path ever carries it. A text post has no media
    /// items, so there is no task to queue and no file to transfer; its text is
    /// saved through its own command, which writes a `.txt` rather than a task.
    MediaType {
        Video => "video",
        Image => "image",
        Carousel => "carousel",
        Audio => "audio",
        Text => "text",
    }
);

string_enum!(
    /// Availability of the *source* content, independent of any local download.
    ContentStatus {
        New => "new",
        Ready => "ready",
        Downloading => "downloading",
        Downloaded => "downloaded",
        Failed => "failed",
        Unavailable => "unavailable",
        Private => "private",
        Unsupported => "unsupported",
    }
);

string_enum!(
    /// State of a *local download operation*.
    DownloadState {
        Queued => "queued",
        Downloading => "downloading",
        Paused => "paused",
        Completed => "completed",
        Failed => "failed",
        Cancelled => "cancelled",
    }
);

string_enum!(
    /// How a media variant relates to the primary resource of its item.
    VariantKind {
        /// Original or clean source offered by the platform.
        Original => "original",
        Standard => "standard",
        High => "high",
        Low => "low",
        AudioOnly => "audio_only",
        Other => "other",
    }
);

string_enum!(
    /// What to do when a destination file already exists.
    ConflictPolicy {
        /// Pick a unique name (`clip (1).mp4`).
        Rename => "rename",
        Overwrite => "overwrite",
        Skip => "skip",
    }
);

string_enum!(
    /// Application colour theme preference.
    Theme {
        System => "system",
        Light => "light",
        Dark => "dark",
    }
);

string_enum!(
    /// Log verbosity, mapped onto `tracing` levels at initialization time.
    LogLevel {
        Error => "error",
        Warn => "warn",
        Info => "info",
        Debug => "debug",
        Trace => "trace",
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_enums_round_trip() {
        for v in MediaType::all() {
            assert_eq!(MediaType::from_str(v.as_str()).unwrap(), *v);
        }
        for v in DownloadState::all() {
            assert_eq!(DownloadState::from_str(v.as_str()).unwrap(), *v);
        }
        for v in ContentStatus::all() {
            assert_eq!(ContentStatus::from_str(v.as_str()).unwrap(), *v);
        }
        assert_eq!(Platform::Douyin.as_str(), "douyin");
        assert_eq!(VariantKind::AudioOnly.as_str(), "audio_only");
    }

    #[test]
    fn unknown_value_is_a_validation_error() {
        let err = DownloadState::from_str("teleporting").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
    }

    #[test]
    fn ids_are_unique_and_non_empty() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(!a.is_empty());
    }
}
