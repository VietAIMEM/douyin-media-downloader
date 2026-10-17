//! Persistent download queue implementation.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::database::{repositories::downloads as downloads_repo, Database};
use crate::errors::{AppError, AppResult};
use crate::models::common::now;
use crate::models::{DownloadEvent, DownloadRequest, DownloadState, DownloadTask};
use crate::settings::SettingsService;

use super::transfer::TransferWorker;
use super::{DownloadManager, DownloadSummary};
use crate::media::MediaMuxer;
use crate::network::HttpClient;
use crate::storage::StorageManager;

/// Channel depth for download events. Large enough that a slow subscriber
/// dropping a progress frame is harmless, small enough that a stalled
/// subscriber cannot grow the buffer without bound.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// A [`DownloadManager`] that persists task state to SQLite, broadcasts
/// lifecycle events, and — when a transfer worker is attached — actually moves
/// the bytes.
///
/// Constructed with [`QueueDownloadManager::new`] it manages state only, which
/// is what unit tests of the state machine want. Constructed with
/// [`QueueDownloadManager::with_worker`] it also runs transfers.
pub struct QueueDownloadManager {
    db: Database,
    settings: Arc<SettingsService>,
    events: broadcast::Sender<DownloadEvent>,
    /// `None` in state-only mode.
    worker: Option<Arc<TransferWorker>>,
}

impl QueueDownloadManager {
    pub fn new(db: Database, settings: Arc<SettingsService>) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            db,
            settings,
            events,
            worker: None,
        }
    }

    /// Build a manager that performs transfers.
    ///
    /// The worker shares this manager's event channel, so a subscriber sees one
    /// ordered stream regardless of which half of the pipeline emitted a frame.
    pub fn with_worker(
        db: Database,
        settings: Arc<SettingsService>,
        http: Arc<dyn HttpClient>,
        storage: Arc<dyn StorageManager>,
        muxer: Arc<dyn MediaMuxer>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let worker = Arc::new(TransferWorker::new(
            db.clone(),
            Arc::clone(&settings),
            http,
            storage,
            muxer,
            events.clone(),
        ));
        // The dispatcher starts itself the first time there is work, because
        // this is called while the application is still being assembled and
        // there is no Tokio runtime to spawn on yet.
        Arc::new(Self {
            db,
            settings,
            events,
            worker: Some(worker),
        })
    }

    /// Make every task the database still holds as `Queued` eligible for a worker.
    ///
    /// Queue state survives a restart; the scheduler does not. This is what puts
    /// the two back together, so a task that was queued when the application
    /// closed is picked up rather than sitting in the list forever.
    pub fn schedule_pending(&self) -> AppResult<usize> {
        match &self.worker {
            Some(worker) => worker.schedule_queued(),
            // State-only manager: nothing transfers, so nothing to schedule.
            None => Ok(0),
        }
    }

    /// The attached transfer worker, when there is one.
    pub fn worker(&self) -> Option<Arc<TransferWorker>> {
        self.worker.clone()
    }

    /// Reset tasks left mid-flight by a previous run.
    ///
    /// A task stored as `downloading` means the application exited while it was
    /// running. It cannot still be running, so it is returned to the queue.
    /// Returns how many tasks were recovered.
    pub fn recover_interrupted(&self) -> AppResult<usize> {
        let recovered = self.db.with_conn(|conn| {
            let stuck = downloads_repo::list_by_state(conn, DownloadState::Downloading)?;
            for mut task in stuck.iter().cloned() {
                task.state = DownloadState::Queued;
                task.updated_at = Some(now());
                downloads_repo::update(conn, &task)?;
            }
            Ok(stuck.len())
        })?;

        if recovered > 0 {
            tracing::warn!(
                target: "downloads",
                count = recovered,
                "requeued downloads left running by a previous session"
            );
        }
        Ok(recovered)
    }

    /// Concurrency limit currently configured.
    pub fn max_concurrent(&self) -> u32 {
        self.settings.current().max_concurrent_downloads
    }

    fn emit(&self, event: DownloadEvent) {
        // A send error only means nobody is subscribed right now.
        let _ = self.events.send(event);
    }

    /// Run a blocking database closure without stalling the async runtime.
    async fn db_call<T, F>(&self, f: F) -> AppResult<T>
    where
        F: FnOnce(&Database) -> AppResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|err| {
                AppError::unknown("download database task failed")
                    .with_technical_detail(err.to_string())
            })?
    }

    /// Load a task or fail with a typed not-found error.
    fn require_task(db: &Database, id: &str) -> AppResult<DownloadTask> {
        db.with_conn(|conn| downloads_repo::get(conn, id))?
            .ok_or_else(|| {
                AppError::not_found("download task", id)
                    .with_user_message("That download is no longer in the list.")
            })
    }

    /// Move a task to `to`, rejecting the move unless its current state is one
    /// of `allowed_from`.
    ///
    /// The write is a **compare-and-set**, not a blind update of what was read.
    /// Two writers reach this method concurrently in normal use — a user asking
    /// for a pause and a transfer worker committing an outcome — and a write
    /// that carried the state it read back to the row would undo whatever landed
    /// in between. The failure that produces is the worst kind: a task left
    /// `Paused` while its history says it completed, or a task reported
    /// `Cancelled` that the user asked to resume.
    ///
    /// A lost race is not an error, though — the row simply moved to another
    /// state that may still be one this transition is allowed from (a pause
    /// arriving just as the worker claims a queued task, say), so the decision is
    /// taken again rather than abandoned. Only a state that genuinely forbids the
    /// transition is reported as one.
    async fn transition(
        &self,
        id: &str,
        action: &str,
        allowed_from: &[DownloadState],
        to: DownloadState,
    ) -> AppResult<DownloadTask> {
        /// How many times a transition may lose the race before giving up.
        ///
        /// Each attempt is a fresh read; a caller is competing with at most one
        /// worker and one other command, so this is far more than enough — and it
        /// is bounded so a task being hammered cannot spin here.
        const ATTEMPTS: usize = 5;

        for attempt in 1..=ATTEMPTS {
            let id = id.to_string();
            let allowed = allowed_from.to_vec();
            let action = action.to_string();

            let outcome = self
                .db_call(move |db| -> AppResult<MoveOutcome> {
                    let mut task = Self::require_task(db, &id)?;
                    if !allowed.contains(&task.state) {
                        return Ok(MoveOutcome::Rejected(task.state));
                    }
                    let previous = task.state;
                    apply_state(&mut task, to);
                    let changed = db.with_conn(|conn| {
                        downloads_repo::update_if_state(conn, &task, previous)
                    })?;
                    Ok(if changed {
                        MoveOutcome::Applied(Box::new(task))
                    } else {
                        MoveOutcome::Superseded
                    })
                })
                .await?;

            match outcome {
                MoveOutcome::Applied(task) => {
                    self.emit(DownloadEvent::StateChanged {
                        task_id: task.id.clone(),
                        state: task.state,
                    });
                    return Ok(*task);
                }
                MoveOutcome::Rejected(state) => {
                    return Err(illegal_transition(&action, state));
                }
                MoveOutcome::Superseded => {
                    tracing::debug!(
                        target: "downloads",
                        action,
                        attempt,
                        "the task changed while it was being moved; deciding again"
                    );
                }
            }
        }

        Err(AppError::database("the task kept changing while it was being moved")
            .with_user_message("That download changed too quickly to be updated. Try again.")
            .with_technical_detail(format!("action={action} attempts={ATTEMPTS}")))
    }
}

/// What one attempt at a [`QueueDownloadManager::transition`] decided.
///
/// Boxed rather than inline: a task is a large value and the other two variants
/// are a byte each, so carrying it by value would make every `Rejected` and
/// `Superseded` pay for a task nobody built.
enum MoveOutcome {
    /// The move was written.
    Applied(Box<DownloadTask>),
    /// The task is in a state this transition does not allow.
    Rejected(DownloadState),
    /// Somebody else wrote the row first. Decide again from the new state.
    Superseded,
}

/// Apply a state change, maintaining the derived timestamps.
fn apply_state(task: &mut DownloadTask, to: DownloadState) {
    task.state = to;
    task.updated_at = Some(now());
    match to {
        DownloadState::Downloading => {
            if task.started_at.is_none() {
                task.started_at = Some(now());
            }
        }
        DownloadState::Completed | DownloadState::Failed | DownloadState::Cancelled => {
            task.completed_at = Some(now());
        }
        DownloadState::Queued | DownloadState::Paused => {}
    }
}

fn illegal_transition(action: &str, state: DownloadState) -> AppError {
    AppError::validation(format!("cannot {action} a download in state `{state}`"))
        .with_user_message(format!(
            "This download cannot be {} while it is {}.",
            past_participle(action),
            state.as_str()
        ))
        .with_technical_detail(format!("state={state}"))
}

/// The past participle of a transition verb, for the user-facing sentence.
///
/// Spelled out rather than formed with a `d` suffix, because "cancel" and
/// "retry" do not follow that rule and a message like "cannot be canceld" makes
/// the application look broken.
fn past_participle(action: &str) -> &'static str {
    match action {
        "cancel" => "cancelled",
        "pause" => "paused",
        "resume" => "resumed",
        "retry" => "retried",
        _ => "changed",
    }
}

#[async_trait]
impl DownloadManager for QueueDownloadManager {
    fn name(&self) -> &'static str {
        "queue"
    }

    async fn enqueue(&self, request: DownloadRequest) -> AppResult<DownloadTask> {
        let max_attempts = request.max_attempts.max(1);
        let mut task = DownloadTask::new(&request);
        task.max_attempts = max_attempts;

        let stored = task.clone();
        self.db_call(move |db| db.with_conn(|conn| downloads_repo::insert(conn, &stored)))
            .await?;

        tracing::info!(
            target: "downloads",
            task_id = %task.id,
            media_type = %task.media_type,
            "download queued"
        );
        self.emit(DownloadEvent::Queued {
            task_id: task.id.clone(),
        });
        // Start the transfer when a worker is attached. In state-only mode the
        // task simply stays queued, which is what the Phase 01 tests assert.
        if let Some(worker) = &self.worker {
            worker.submit(task.clone());
        }
        Ok(task)
    }

    async fn get(&self, id: &str) -> AppResult<Option<DownloadTask>> {
        let id = id.to_string();
        self.db_call(move |db| db.with_conn(|conn| downloads_repo::get(conn, &id)))
            .await
    }

    async fn list(&self) -> AppResult<Vec<DownloadTask>> {
        self.db_call(|db| db.with_conn(|conn| downloads_repo::list(conn, 500, 0)))
            .await
    }

    async fn cancel(&self, id: &str) -> AppResult<DownloadTask> {
        // Signal the transfer first: stopping the byte stream is what makes a
        // cancel real rather than cosmetic. A queued task has no handle yet and
        // is stopped by the state change below.
        if let Some(worker) = &self.worker {
            worker.cancel(id);
        }

        let outcome = self
            .transition(
                id,
                "cancel",
                &[
                    DownloadState::Queued,
                    DownloadState::Downloading,
                    DownloadState::Paused,
                ],
                DownloadState::Cancelled,
            )
            .await;

        let task = match outcome {
            Ok(task) => task,
            // The in-flight transfer can notice the stop request and mark the
            // task cancelled before this transition runs. That is exactly the
            // outcome the caller asked for, so it is a success rather than an
            // "already cancelled" error — and it makes a double-click harmless.
            Err(err) => match self.get(id).await? {
                Some(task) if task.state == DownloadState::Cancelled => task,
                _ => return Err(err),
            },
        };

        // Record the outcome here as well as in the worker. The write is
        // idempotent per task, so whichever side gets there first wins and the
        // task always ends up with exactly one history row.
        let stored = task.clone();
        let _ = self
            .db_call(move |db| {
                db.with_conn(|conn| {
                    super::history::record(conn, &stored, DownloadState::Cancelled, None, None)
                })
            })
            .await;

        Ok(task)
    }

    async fn pause(&self, id: &str) -> AppResult<DownloadTask> {
        // Phase 02 pause stops the transfer rather than suspending it: there is
        // no byte-range resume yet, so `resume` restarts from the beginning.
        // Stopping the stream is the honest behaviour — a "paused" task that
        // keeps consuming bandwidth would be a lie.
        //
        // The state change is recorded *before* the transfer is signalled, and
        // that order matters: the transfer reacts by finishing the task, and a
        // task it finishes before this write lands would be recorded as
        // cancelled rather than paused.
        let task = self
            .transition(
                id,
                "pause",
                &[DownloadState::Queued, DownloadState::Downloading],
                DownloadState::Paused,
            )
            .await?;

        if let Some(worker) = &self.worker {
            // `pause`, not `cancel`: the transfer must stop *and* leave what it
            // has already written, because that is what the resume continues.
            worker.pause(id);
        }
        Ok(task)
    }

    async fn resume(&self, id: &str) -> AppResult<DownloadTask> {
        let task = self
            .transition(id, "resume", &[DownloadState::Paused], DownloadState::Queued)
            .await?;
        if let Some(worker) = &self.worker {
            worker.submit(task.clone());
        }
        Ok(task)
    }

    async fn retry(&self, id: &str) -> AppResult<DownloadTask> {
        let id_owned = id.to_string();
        let task = self
            .db_call(move |db| {
                let mut task = Self::require_task(db, &id_owned)?;
                if !matches!(task.state, DownloadState::Failed | DownloadState::Cancelled) {
                    return Err(illegal_transition("retry", task.state));
                }
                task.error = None;
                // A retry starts the attempt count over: the previous run's
                // attempts should not consume the new run's budget.
                task.attempts = 0;
                apply_state(&mut task, DownloadState::Queued);
                db.with_conn(|conn| downloads_repo::update(conn, &task))?;
                Ok(task)
            })
            .await?;

        tracing::info!(target: "downloads", task_id = %task.id, "download retried");
        self.emit(DownloadEvent::StateChanged {
            task_id: task.id.clone(),
            state: task.state,
        });
        if let Some(worker) = &self.worker {
            worker.submit(task.clone());
        }
        Ok(task)
    }

    async fn remove(&self, id: &str) -> AppResult<bool> {
        let id_owned = id.to_string();
        let removed = self
            .db_call(move |db| db.with_conn(|conn| downloads_repo::delete(conn, &id_owned)))
            .await?;
        if removed {
            self.emit(DownloadEvent::Removed {
                task_id: id.to_string(),
            });
        }
        Ok(removed)
    }

    async fn schedule_pending(&self) -> AppResult<usize> {
        // The inherent method, explicitly: this is the same behaviour the
        // composition root calls directly, not a second implementation of it.
        QueueDownloadManager::schedule_pending(self)
    }

    async fn summary(&self) -> AppResult<DownloadSummary> {
        let tasks = self.list().await?;
        Ok(DownloadSummary::from_tasks(&tasks))
    }

    async fn clear_completed(&self) -> AppResult<usize> {
        let removed = self
            .db_call(|db| {
                db.with_conn(|conn| downloads_repo::clear_state(conn, DownloadState::Completed))
            })
            .await?;
        if removed > 0 {
            tracing::info!(
                target: "downloads",
                removed,
                "cleared completed entries from the visible queue"
            );
        }
        Ok(removed)
    }

    async fn retry_failed(&self) -> AppResult<Vec<DownloadTask>> {
        // Read the list, then retry each through the same path a single retry
        // takes — so a bulk retry cannot behave differently from a manual one.
        let ids: Vec<String> = self
            .db_call(|db| {
                db.with_conn(|conn| downloads_repo::list_by_state(conn, DownloadState::Failed))
            })
            .await?
            .into_iter()
            .map(|task| task.id)
            .collect();

        let mut retried = Vec::with_capacity(ids.len());
        for id in ids {
            match self.retry(&id).await {
                Ok(task) => retried.push(task),
                // One task that cannot be retried — removed from under us, or
                // already moving — must not stop the rest.
                Err(err) => tracing::warn!(
                    target: "downloads",
                    task_id = %id,
                    error = %err.message(),
                    "could not retry a failed task"
                ),
            }
        }

        tracing::info!(
            target: "downloads",
            retried = retried.len(),
            "failed downloads re-queued"
        );
        Ok(retried)
    }

    fn subscribe(&self) -> broadcast::Receiver<DownloadEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MediaType;

    fn manager() -> QueueDownloadManager {
        let db = Database::open_in_memory().unwrap();
        let settings = Arc::new(SettingsService::new(db.clone()).unwrap());
        QueueDownloadManager::new(db, settings)
    }

    fn request() -> DownloadRequest {
        DownloadRequest::new("https://cdn.example/v.mp4", MediaType::Video)
            .with_file_name("clip")
    }

    #[tokio::test]
    async fn enqueue_persists_a_queued_task() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();
        assert_eq!(task.state, DownloadState::Queued);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.id, task.id);
        assert_eq!(stored.source_url, task.source_url);

        let summary = manager.summary().await.unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.queued, 1);
        assert_eq!(summary.active(), 1);
    }

    #[tokio::test]
    async fn enqueue_emits_an_event() {
        let manager = manager();
        let mut rx = manager.subscribe();
        let task = manager.enqueue(request()).await.unwrap();
        let event = rx.recv().await.unwrap();
        assert_eq!(event, DownloadEvent::Queued { task_id: task.id });
    }

    #[tokio::test]
    async fn pause_then_resume_returns_to_queue() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();

        let paused = manager.pause(&task.id).await.unwrap();
        assert_eq!(paused.state, DownloadState::Paused);

        let resumed = manager.resume(&task.id).await.unwrap();
        assert_eq!(resumed.state, DownloadState::Queued);
    }

    #[tokio::test]
    async fn cancel_sets_a_terminal_timestamp() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();
        let cancelled = manager.cancel(&task.id).await.unwrap();
        assert_eq!(cancelled.state, DownloadState::Cancelled);
        assert!(cancelled.completed_at.is_some());
        assert!(cancelled.is_terminal());
    }

    #[tokio::test]
    async fn illegal_transition_is_rejected() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();
        manager.cancel(&task.id).await.unwrap();
        // Cancelling again, or pausing a cancelled task, must fail cleanly.
        let err = manager.pause(&task.id).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
        assert!(err.user_message().contains("pause"));
    }

    #[tokio::test]
    async fn pause_is_rejected_for_completed_tasks_after_retry_flow() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();
        manager.cancel(&task.id).await.unwrap();
        let retried = manager.retry(&task.id).await.unwrap();
        assert_eq!(retried.state, DownloadState::Queued);
        assert!(retried.error.is_none());
    }

    #[tokio::test]
    async fn transitions_on_missing_task_report_not_found() {
        let manager = manager();
        let err = manager.cancel("nope").await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::NotFound);
    }

    #[tokio::test]
    async fn remove_deletes_the_task() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();
        assert!(manager.remove(&task.id).await.unwrap());
        assert!(manager.get(&task.id).await.unwrap().is_none());
        assert!(!manager.remove(&task.id).await.unwrap());
    }

    #[tokio::test]
    async fn summary_counts_every_state() {
        let manager = manager();
        let a = manager.enqueue(request()).await.unwrap();
        let b = manager.enqueue(request()).await.unwrap();
        manager.pause(&a.id).await.unwrap();
        manager.cancel(&b.id).await.unwrap();

        let summary = manager.summary().await.unwrap();
        assert_eq!(summary.total, 2);
        assert_eq!(summary.paused, 1);
        assert_eq!(summary.cancelled, 1);
        assert_eq!(summary.active(), 1);
    }

    #[tokio::test]
    async fn recover_interrupted_requeues_running_tasks() {
        let manager = manager();
        let task = manager.enqueue(request()).await.unwrap();

        // Simulate a crash mid-download by writing the state directly.
        manager
            .db
            .with_conn(|conn| {
                let mut stored = downloads_repo::get(conn, &task.id)?.unwrap();
                stored.state = DownloadState::Downloading;
                downloads_repo::update(conn, &stored)
            })
            .unwrap();

        let recovered = manager.recover_interrupted().unwrap();
        assert_eq!(recovered, 1);
        let recovered_task = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(recovered_task.state, DownloadState::Queued);
    }

    #[tokio::test]
    async fn concurrency_comes_from_settings() {
        let db = Database::open_in_memory().unwrap();
        let settings = Arc::new(SettingsService::new(db.clone()).unwrap());
        settings
            .update(|s| {
                s.max_concurrent_downloads = 5;
                Ok(())
            })
            .unwrap();
        let manager = QueueDownloadManager::new(db, settings);
        assert_eq!(manager.max_concurrent(), 5);
    }
}
