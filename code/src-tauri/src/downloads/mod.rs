//! Download layer.
//!
//! [`DownloadManager`] is the single entry point for *all* downloads — the
//! Single URL screen (Phase 02), the Profile Downloader (Phase 03), and any
//! future source all submit here. The UI never implements download logic.
//!
//! # Scope in Phase 01
//!
//! [`QueueDownloadManager`] implements the full *task lifecycle*: queueing,
//! persistence, state transitions, progress/event broadcast, and restart
//! recovery. It deliberately does **not** perform network transfers — the
//! worker pool that moves bytes is Phase 04, and the resolver work that
//! produces real media URLs is Phase 02/03. Keeping the manager honest about
//! that boundary is what lets Phase 04 add workers without changing this
//! interface.
//!
//! ```text
//! Single URL ──┐
//!              ├──▶ DownloadManager ──▶ (Phase 04: worker pool → File Storage)
//! Profile ─────┘
//! ```

pub mod history;
pub mod partial;
pub mod manager;
pub mod transfer;

pub use manager::QueueDownloadManager;
pub use transfer::TransferWorker;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::errors::AppResult;
use crate::models::{DownloadEvent, DownloadRequest, DownloadState, DownloadTask};

/// Aggregate counters for the Downloads screen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSummary {
    pub total: usize,
    pub queued: usize,
    pub downloading: usize,
    pub paused: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    /// Bytes transferred so far across tasks that have not finished.
    ///
    /// Deliberately *not* the size of the finished files: a progress bar over
    /// work in flight and a total of what has already landed are different
    /// facts, and adding them together would produce a number that means
    /// neither.
    pub active_downloaded_bytes: u64,
    /// Those same tasks' totals, summed over the ones whose size the source
    /// reported. `None` when not one of them did — which is not the same as
    /// zero bytes of work.
    pub active_total_bytes: Option<u64>,
    /// Unfinished tasks whose total the source never reported. They are left out
    /// of the byte aggregate rather than counted as zero, and this is how the
    /// screen can say so.
    pub active_without_size: usize,
    /// Total size of the files completed downloads produced.
    pub completed_bytes: u64,
}

impl DownloadSummary {
    /// Tasks that are neither finished nor abandoned.
    pub fn active(&self) -> usize {
        self.queued + self.downloading + self.paused
    }

    /// Build a summary from a task list.
    pub fn from_tasks(tasks: &[DownloadTask]) -> Self {
        let mut summary = Self {
            total: tasks.len(),
            ..Default::default()
        };
        let mut known_total: Option<u64> = None;

        for task in tasks {
            match task.state {
                DownloadState::Queued => summary.queued += 1,
                DownloadState::Downloading => summary.downloading += 1,
                DownloadState::Paused => summary.paused += 1,
                DownloadState::Completed => summary.completed += 1,
                DownloadState::Failed => summary.failed += 1,
                DownloadState::Cancelled => summary.cancelled += 1,
            }

            if task.state == DownloadState::Completed {
                summary.completed_bytes += task
                    .progress
                    .total_bytes
                    .unwrap_or(task.progress.downloaded_bytes);
            } else if !task.is_terminal() {
                // Queued, downloading, or paused: work that is not finished.
                summary.active_downloaded_bytes += task.progress.downloaded_bytes;
                match task.progress.total_bytes {
                    Some(total) => *known_total.get_or_insert(0) += total,
                    None => summary.active_without_size += 1,
                }
            }
        }

        summary.active_total_bytes = known_total;
        summary
    }
}

/// Manages the lifecycle of every download task.
///
/// Implementations must be safe to share across the async runtime.
#[async_trait]
pub trait DownloadManager: Send + Sync {
    /// Short identifier used in diagnostics and logs.
    fn name(&self) -> &'static str;

    /// Submit a new download and return the created (queued) task.
    async fn enqueue(&self, request: DownloadRequest) -> AppResult<DownloadTask>;

    /// Fetch a task by id.
    async fn get(&self, id: &str) -> AppResult<Option<DownloadTask>>;

    /// All tasks, newest first.
    async fn list(&self) -> AppResult<Vec<DownloadTask>>;

    /// Cancel a queued, running, or paused task.
    async fn cancel(&self, id: &str) -> AppResult<DownloadTask>;

    /// Pause a queued or running task.
    async fn pause(&self, id: &str) -> AppResult<DownloadTask>;

    /// Resume a paused task.
    async fn resume(&self, id: &str) -> AppResult<DownloadTask>;

    /// Re-queue a failed or cancelled task.
    async fn retry(&self, id: &str) -> AppResult<DownloadTask>;

    /// Remove a task record. Returns whether it existed.
    async fn remove(&self, id: &str) -> AppResult<bool>;

    /// Remove the finished tasks from the visible queue.
    ///
    /// A *list* operation: the queue entries go, the files they produced and the
    /// history they wrote stay. Clearing what the screen shows is not deleting
    /// what was downloaded. Returns how many entries were removed.
    async fn clear_completed(&self) -> AppResult<usize>;

    /// Re-queue every failed task. Returns the tasks that were re-queued, so the
    /// caller can report what actually happened rather than what it asked for.
    async fn retry_failed(&self) -> AppResult<Vec<DownloadTask>>;

    /// Make every persisted task that is still waiting eligible for a worker.
    ///
    /// Queue state survives a restart; the scheduler does not. A manager that
    /// has just started therefore has to be told to pick up where it left off,
    /// or a task that was queued when the application closed sits in the list
    /// forever. Returns how many tasks were made eligible.
    async fn schedule_pending(&self) -> AppResult<usize>;

    /// Aggregate counters for the UI.
    async fn summary(&self) -> AppResult<DownloadSummary>;

    /// Subscribe to task lifecycle events.
    fn subscribe(&self) -> broadcast::Receiver<DownloadEvent>;
}
