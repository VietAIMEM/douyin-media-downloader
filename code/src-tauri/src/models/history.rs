//! Download history model.
//!
//! A history entry is an immutable record of one download *outcome*. It is
//! written once a task reaches a terminal state and is the basis for the
//! History screen, duplicate detection, and "download new only" workflows.

use serde::{Deserialize, Serialize};

use super::common::{new_id, DownloadState, Id, MediaType, Timestamp};

/// A completed or failed download, recorded for later reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadHistoryEntry {
    pub id: Id,
    /// Originating task, when still known.
    pub task_id: Option<Id>,
    pub post_id: Option<Id>,
    pub post_url: Option<String>,
    pub creator_id: Option<Id>,
    pub creator_name: Option<String>,
    pub media_type: MediaType,
    pub source_url: String,
    /// Absolute path of the saved file. `None` for failures.
    pub local_path: Option<String>,
    pub file_size: Option<u64>,
    /// Terminal state: `completed`, `failed`, or `cancelled`.
    pub status: DownloadState,
    pub error: Option<String>,
    pub downloaded_at: Option<Timestamp>,
}

impl DownloadHistoryEntry {
    /// Build an entry for a successful download.
    pub fn completed(
        source_url: impl Into<String>,
        media_type: MediaType,
        local_path: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id(),
            task_id: None,
            post_id: None,
            post_url: None,
            creator_id: None,
            creator_name: None,
            media_type,
            source_url: source_url.into(),
            local_path: Some(local_path.into()),
            file_size: None,
            status: DownloadState::Completed,
            error: None,
            downloaded_at: Some(super::common::now()),
        }
    }

    /// Build an entry for a failed download.
    pub fn failed(
        source_url: impl Into<String>,
        media_type: MediaType,
        error: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id(),
            task_id: None,
            post_id: None,
            post_url: None,
            creator_id: None,
            creator_name: None,
            media_type,
            source_url: source_url.into(),
            local_path: None,
            file_size: None,
            status: DownloadState::Failed,
            error: Some(error.into()),
            downloaded_at: Some(super::common::now()),
        }
    }

    pub fn was_successful(&self) -> bool {
        self.status == DownloadState::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_entry_has_path_and_no_error() {
        let e = DownloadHistoryEntry::completed("https://cdn/v.mp4", MediaType::Video, "C:/dl/v.mp4");
        assert!(e.was_successful());
        assert_eq!(e.local_path.as_deref(), Some("C:/dl/v.mp4"));
        assert!(e.error.is_none());
    }

    #[test]
    fn failed_entry_has_error_and_no_path() {
        let e = DownloadHistoryEntry::failed("https://cdn/v.mp4", MediaType::Video, "timeout");
        assert!(!e.was_successful());
        assert!(e.local_path.is_none());
        assert_eq!(e.error.as_deref(), Some("timeout"));
    }
}
