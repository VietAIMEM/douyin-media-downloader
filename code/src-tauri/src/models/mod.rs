//! Normalized application domain models.
//!
//! Nothing in this module knows about Douyin's response shapes. Resolvers
//! translate platform data into these types, and every other layer (database,
//! downloads, storage, UI) works exclusively with them.

pub mod common;
pub mod creator;
pub mod download;
pub mod history;
pub mod library;
pub mod media;
pub mod post;
pub mod settings;

pub use common::{
    new_id, now, ConflictPolicy, ContentStatus, DownloadState, Id, LogLevel, MediaType, Metadata,
    Platform, Theme, Timestamp, VariantKind,
};
pub use creator::Creator;
pub use download::{DownloadEvent, DownloadProgress, DownloadRequest, DownloadTask};
pub use history::DownloadHistoryEntry;
pub use library::{
    LibraryCounts, LibraryEntry, LibraryFilter, LibraryPage, LibrarySort, LibraryView,
};
pub use media::{MediaItem, MediaVariant, TrackLayout};
pub use post::Post;
pub use settings::AppSettings;
