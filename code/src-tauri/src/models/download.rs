//! Download domain models.
//!
//! These types describe *local download operations*. They are intentionally
//! separate from [`ContentStatus`](super::common::ContentStatus), which
//! describes the availability of the source content.

use serde::{Deserialize, Serialize};

use super::common::{new_id, DownloadState, Id, MediaType, Timestamp};

/// Byte-level progress of a download task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    /// `None` when the source does not report a content length.
    pub total_bytes: Option<u64>,
}

impl DownloadProgress {
    pub fn new(downloaded_bytes: u64, total_bytes: Option<u64>) -> Self {
        Self {
            downloaded_bytes,
            total_bytes,
        }
    }

    /// Completion ratio in `0.0..=1.0`, or `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        match self.total_bytes {
            Some(total) if total > 0 => Some((self.downloaded_bytes as f64 / total as f64).min(1.0)),
            _ => None,
        }
    }

    /// Percentage in `0..=100`, or `None` when the total is unknown.
    pub fn percent(&self) -> Option<f64> {
        self.fraction().map(|f| (f * 100.0 * 10.0).round() / 10.0)
    }

    pub fn is_complete(&self) -> bool {
        matches!(self.total_bytes, Some(total) if total > 0 && self.downloaded_bytes >= total)
    }
}

/// A queued or in-flight download.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadTask {
    pub id: Id,
    pub post_id: Option<Id>,
    pub media_item_id: Option<Id>,
    pub variant_id: Option<Id>,
    pub source_url: String,
    /// A second track to combine with `source_url`, when the source keeps its
    /// audio separate.
    ///
    /// Stored on the task rather than looked up again, because a retry has to
    /// fetch the same two tracks: re-resolving mid-retry would either fail or,
    /// worse, quietly produce a video with no sound.
    pub audio_source_url: Option<String>,
    /// Absolute destination path once the naming policy has been applied.
    pub destination_path: Option<String>,
    pub file_name: Option<String>,
    /// Extension of the resolved media (without the dot), used for `{ext}` and
    /// for appending the suffix when the naming template omits it.
    pub extension: Option<String>,
    pub media_type: MediaType,
    pub state: DownloadState,
    /// Attempts made so far (0 before the first try).
    pub attempts: u32,
    pub max_attempts: u32,
    pub progress: DownloadProgress,
    /// Last error message (technical), when the task failed.
    pub error: Option<String>,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
}

impl DownloadTask {
    pub fn new(request: &DownloadRequest) -> Self {
        let ts = super::common::now();
        Self {
            id: new_id(),
            post_id: request.post_id.clone(),
            media_item_id: request.media_item_id.clone(),
            variant_id: request.variant_id.clone(),
            source_url: request.source_url.clone(),
            audio_source_url: request.audio_source_url.clone(),
            destination_path: None,
            file_name: request.suggested_file_name.clone(),
            extension: request.extension.clone(),
            media_type: request.media_type,
            state: DownloadState::Queued,
            attempts: 0,
            max_attempts: request.max_attempts,
            progress: DownloadProgress::default(),
            error: None,
            created_at: Some(ts),
            updated_at: Some(ts),
            started_at: None,
            completed_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled
        )
    }
}

/// What a caller asks the [`DownloadManager`](crate::downloads::DownloadManager)
/// to fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    pub source_url: String,
    /// A second track to combine with `source_url` after both are fetched.
    ///
    /// Set when the source serves video and audio separately and the caller
    /// wants one playable file rather than a silent video.
    pub audio_source_url: Option<String>,
    pub media_type: MediaType,
    pub post_id: Option<Id>,
    pub media_item_id: Option<Id>,
    pub variant_id: Option<Id>,
    /// Suggested base file name (without directory). Sanitized by the storage
    /// layer before use.
    pub suggested_file_name: Option<String>,
    /// Extension of the selected media variant, without the dot.
    pub extension: Option<String>,
    /// Override for the download root; falls back to the configured directory.
    pub destination_directory: Option<String>,
    pub max_attempts: u32,
}

impl DownloadRequest {
    pub fn new(source_url: impl Into<String>, media_type: MediaType) -> Self {
        Self {
            source_url: source_url.into(),
            audio_source_url: None,
            media_type,
            post_id: None,
            media_item_id: None,
            variant_id: None,
            suggested_file_name: None,
            extension: None,
            destination_directory: None,
            max_attempts: 3,
        }
    }

    pub fn with_post(mut self, post_id: impl Into<Id>) -> Self {
        self.post_id = Some(post_id.into());
        self
    }

    pub fn with_media(mut self, media_item_id: impl Into<Id>, variant_id: impl Into<Id>) -> Self {
        self.media_item_id = Some(media_item_id.into());
        self.variant_id = Some(variant_id.into());
        self
    }

    pub fn with_file_name(mut self, name: impl Into<String>) -> Self {
        self.suggested_file_name = Some(name.into());
        self
    }

    /// Set the extension used for the saved file.
    pub fn with_extension(mut self, extension: impl Into<String>) -> Self {
        let extension = extension.into();
        let trimmed = extension.trim_start_matches('.').to_string();
        self.extension = (!trimmed.is_empty()).then_some(trimmed);
        self
    }

    /// Set how many attempts the transfer may make, including the first.
    pub fn with_max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    /// Fetch a second, audio-only track and combine it with the primary.
    ///
    /// The caller is responsible for setting this only when the source really
    /// serves the tracks apart; the download layer treats it as a promise that
    /// both are needed.
    pub fn with_audio_source(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.audio_source_url = (!url.trim().is_empty()).then_some(url);
        self
    }
}

/// Progress/state notifications emitted by the download manager.
///
/// Broadcast rather than returned so multiple UI surfaces (Downloads page, a
/// status bar, future notifications) can observe the same stream. Phase 04
/// builds the real worker pool on top of this channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DownloadEvent {
    /// A task entered the queue.
    Queued { task_id: Id },
    /// A task changed state.
    StateChanged { task_id: Id, state: DownloadState },
    /// Byte-level progress update.
    Progress {
        task_id: Id,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
        /// Transfer speed over a recent window, when one could be measured.
        ///
        /// Measured over a window rather than from the last chunk, so a single
        /// slow or fast chunk does not make the number jump around.
        speed_bytes_per_second: Option<u64>,
        /// Seconds remaining at the current speed.
        ///
        /// `None` whenever the total is unknown or the speed is not yet
        /// meaningful — an estimate built on nothing is worse than no estimate.
        eta_seconds: Option<u64>,
    },
    /// A task finished successfully.
    Completed { task_id: Id, destination_path: Option<String> },
    /// A task failed (possibly retryable).
    Failed { task_id: Id, message: String },
    /// A task was removed from the queue.
    Removed { task_id: Id },
}

impl DownloadEvent {
    pub fn task_id(&self) -> &str {
        match self {
            Self::Queued { task_id }
            | Self::StateChanged { task_id, .. }
            | Self::Progress { task_id, .. }
            | Self::Completed { task_id, .. }
            | Self::Failed { task_id, .. }
            | Self::Removed { task_id } => task_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_is_none_without_total() {
        let p = DownloadProgress::new(500, None);
        assert!(p.percent().is_none());
        assert!(p.fraction().is_none());
        assert!(!p.is_complete());
    }

    #[test]
    fn percent_clamps_to_100() {
        let p = DownloadProgress::new(1500, Some(1000));
        assert_eq!(p.percent(), Some(100.0));
        assert!(p.is_complete());
    }

    #[test]
    fn percent_rounds_to_one_decimal() {
        let p = DownloadProgress::new(1, Some(3));
        assert_eq!(p.percent(), Some(33.3));
    }

    #[test]
    fn task_starts_queued_and_is_not_terminal() {
        let req = DownloadRequest::new("https://cdn/v.mp4", MediaType::Video);
        let task = DownloadTask::new(&req);
        assert_eq!(task.state, DownloadState::Queued);
        assert!(!task.is_terminal());
        assert_eq!(task.attempts, 0);
    }

    #[test]
    fn event_exposes_task_id() {
        let ev = DownloadEvent::StateChanged {
            task_id: "t1".into(),
            state: DownloadState::Downloading,
        };
        assert_eq!(ev.task_id(), "t1");
    }
}
