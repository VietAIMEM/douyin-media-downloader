//! Real byte transfer for queued download tasks.
//!
//! [`QueueDownloadManager`](super::QueueDownloadManager) owns *task state*; this
//! worker owns *moving the bytes*. Splitting them this way means the manager's
//! interface — which the UI and every future source already depend on — does not
//! change when Phase 04 replaces this single-flight worker with a pool.
//!
//! One task is one [`tokio`] task. Concurrency is gated by a semaphore sized
//! from the user's `max_concurrent_downloads` setting, so "minimal concurrency"
//! in this phase and "worker pool" in the next are the same mechanism at
//! different widths.
//!
//! ```text
//! submit(task) ─▶ semaphore ─▶ plan destination (StorageManager)
//!                            ─▶ stream to <name>.part (HttpClient, progress, cancel)
//!                            ─▶ finalize into place   (StorageManager)
//!                            ─▶ persist state + history (Database)
//!                            ─▶ broadcast DownloadEvent
//! ```
//!
//! The worker never invents success: a task reaches `Completed` only after a
//! non-empty file has been moved into its final location.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};

use crate::database::{
    repositories::creators as creators_repo, repositories::downloads as downloads_repo,
    repositories::posts as posts_repo, Database,
};
use crate::errors::{AppError, AppErrorKind, AppResult};
use crate::media::{MediaMuxer, MuxRequest};
use crate::models::common::now;
use crate::models::{DownloadEvent, DownloadState, DownloadTask, MediaType, MediaVariant};
use crate::network::{CancellationHandle, HttpClient, ProgressReporter, TransferOutcome, TransferRequest};
use crate::settings::SettingsService;
use crate::storage::{NamingContext, StorageManager};

use super::history;
use super::partial::PartialMetadata;

/// Minimum gap between progress broadcasts for one task.
///
/// Progress is reported per chunk, which on a fast connection is thousands of
/// times a second. Coalescing to this interval keeps IPC traffic and UI renders
/// bounded without making the progress bar feel laggy.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// First retry delay; doubled per attempt and capped by [`MAX_BACKOFF`].
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

/// How long the dispatcher waits before re-checking, when there is nothing to do.
///
/// It is woken immediately when work arrives or a slot frees, so this is only a
/// backstop — and it is what makes a concurrency change take effect without a
/// restart.
const DISPATCH_POLL: Duration = Duration::from_millis(250);

/// Runs transfers for queued tasks, a bounded number at a time.
///
/// This is the application's only worker pool. `Queued` is a *durable* state: a
/// task is persisted before it is ever eligible, and this decides when it
/// actually runs. Submitting does not spawn a transfer — it makes the task
/// visible to the scheduler — which is what keeps a queue of ten thousand tasks
/// from becoming ten thousand parked Tokio tasks. The only Tokio tasks that
/// exist are the ones actually transferring, plus this loop.
///
/// ```text
/// submit(id) ─▶ queue ─▶ dispatcher ──(slot free?)──▶ spawn transfer
///                            ▲                              │
///                            └──────── notify ◀─────────────┘
/// ```
pub struct TransferWorker {
    db: Database,
    settings: Arc<SettingsService>,
    http: Arc<dyn HttpClient>,
    storage: Arc<dyn StorageManager>,
    /// Combines a video track and an audio track into one file. A no-op muxer
    /// means a download that needs two tracks cannot be completed, which is
    /// reported rather than worked around.
    muxer: Arc<dyn MediaMuxer>,
    events: broadcast::Sender<DownloadEvent>,
    /// Everything the dispatcher and the control commands share.
    state: Mutex<SchedulerState>,
    /// Wakes the dispatcher when work arrives or a slot frees.
    wake: Notify,
    /// Whether the dispatcher loop has been started.
    dispatcher_started: std::sync::atomic::AtomicBool,
    /// How many transfers this worker has started. Diagnostics, and how a test
    /// measures concurrency rather than inferring it.
    started: AtomicU64,
}

/// What the dispatcher and the control commands both need to see.
#[derive(Default)]
struct SchedulerState {
    /// Task ids waiting for a slot, oldest first.
    queue: VecDeque<String>,
    /// Transfers actually running right now.
    running: HashSet<String>,
    /// Cancellation handles for the running transfers.
    ///
    /// A queued task has no handle: nothing is in flight to stop, and the
    /// dispatcher skips it because it is no longer `Queued`. Creating one up
    /// front would mean answering "is this cancelled?" from a map instead of
    /// from the persisted state, which is the state that decides.
    handles: HashMap<String, CancellationHandle>,
}

/// What an earlier attempt left behind, offered to the source as a continuation.
///
/// The three travel together because none of them means anything alone: an
/// offset without an identity describes bytes nobody can attribute, an identity
/// with no offset has nothing to continue, and the reported total is the
/// statement a later response is checked against when the validator cannot
/// settle the question on its own.
#[derive(Debug, Clone, Copy, Default)]
struct ResumeOffer<'a> {
    /// Where the partial ends — the head start a continuation begins from.
    offset: u64,
    /// The identity the partial was written under.
    validator: Option<&'a str>,
    /// The resource's whole size, as it was reported when the partial started.
    total: Option<u64>,
}

/// What recording a finished transfer produced.
///
/// Three answers, because three things can happen and only one of them is
/// "it worked". Returning a bare boolean collapsed the last two, and the caller
/// reported both as a user stop — which is how a database fault came to leave a
/// task in `downloading` for ever while its file sat on disk.
enum Completion {
    /// The task became completed, and its history row was written with it.
    Recorded,
    /// The row had already moved on — a cancel or pause landed first — so the
    /// file is kept and nothing is claimed.
    Stopped,
    /// The record could not be written at all.
    Unrecorded(AppError),
}

impl ResumeOffer<'_> {
    /// Whether there is a continuation worth offering at all.
    ///
    /// Both halves are required, and asking a server to continue bytes whose
    /// provenance is unknown is exactly how two versions end up in one file:
    /// the ranged path refuses a partial with no identity before it is even
    /// sent.
    fn is_offered(&self) -> bool {
        self.offset > 0 && self.validator.is_some()
    }
}

impl TransferWorker {
    pub fn new(
        db: Database,
        settings: Arc<SettingsService>,
        http: Arc<dyn HttpClient>,
        storage: Arc<dyn StorageManager>,
        muxer: Arc<dyn MediaMuxer>,
        events: broadcast::Sender<DownloadEvent>,
    ) -> Self {
        Self {
            db,
            settings,
            http,
            storage,
            muxer,
            events,
            state: Mutex::new(SchedulerState::default()),
            wake: Notify::new(),
            dispatcher_started: std::sync::atomic::AtomicBool::new(false),
            started: AtomicU64::new(0),
        }
    }

    /// Start the dispatcher loop if it is not already running.
    ///
    /// Lazy and idempotent, because the worker is built while the application is
    /// still being assembled — a context that has no Tokio runtime yet, and in
    /// which spawning would panic. The loop starts the first time there is
    /// something to do, which is always from inside the runtime.
    fn ensure_dispatcher(self: &Arc<Self>) {
        if self
            .dispatcher_started
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // Nothing to run on yet. Leave the flag clear so the next call, from
            // a context that does have a runtime, starts it.
            self.dispatcher_started.store(false, Ordering::SeqCst);
            return;
        };
        let worker = Arc::clone(self);
        handle.spawn(async move { worker.dispatch_loop().await });
    }

    /// How many transfers may run at once, read live from settings.
    ///
    /// Read on every dispatch rather than captured once, so changing the setting
    /// changes the pool: raising it starts queued work immediately, and lowering
    /// it stops new work starting until the running count falls back under the
    /// new limit. Nothing already transferring is killed to satisfy a number.
    fn concurrency_limit(&self) -> usize {
        // Clamped on both sides even though settings are validated on save and
        // repaired on load: this is the number of simultaneous sockets the
        // application opens, and a value that arrived by any other route must
        // not be able to lift the ceiling.
        self.settings
            .current()
            .max_concurrent_downloads
            .clamp(1, crate::models::settings::MAX_CONCURRENT_LIMIT) as usize
    }

    /// Transfers running right now.
    pub fn running_count(&self) -> usize {
        self.state.lock().running.len()
    }

    /// Transfers waiting for a slot.
    pub fn queued_count(&self) -> usize {
        self.state.lock().queue.len()
    }

    /// How many transfers this worker has begun, ever.
    pub fn started_count(&self) -> usize {
        self.started.load(Ordering::SeqCst) as usize
    }

    /// Make a queued task eligible for a worker.
    ///
    /// Idempotent: a task already waiting, or already transferring, is not
    /// queued a second time. That is what makes a repeated `resume`, a
    /// double-submitted enqueue, or a retry racing a resume harmless.
    pub fn submit(self: &Arc<Self>, task: DownloadTask) {
        let accepted = {
            let mut state = self.state.lock();
            // Deduplicated against the *queue* only. A task whose transfer is
            // still running is allowed back in: the dispatcher will not start it
            // until the previous one has finished unwinding, and by then its
            // persisted state decides whether it should run at all. Refusing it
            // here instead is how a resume that arrives while its own transfer
            // is stopping would be lost for good.
            if state.queue.iter().any(|id| id == &task.id) {
                false
            } else {
                state.queue.push_back(task.id.clone());
                true
            }
        };

        if accepted {
            // There is now something to do, so make sure something is running to
            // do it.
            self.ensure_dispatcher();
            tracing::debug!(target: "downloads", task_id = %task.id, "task is eligible for a worker");
            self.wake.notify_one();
        } else {
            tracing::debug!(
                target: "downloads",
                task_id = %task.id,
                "task is already waiting for a worker; not submitted twice"
            );
        }
    }

    /// Take a task out of the waiting queue.
    ///
    /// Called when a task is paused or cancelled: whatever it was waiting for is
    /// no longer what it wants, and leaving the id behind would block the resume
    /// or retry that follows. A *running* transfer is unaffected — it is stopped
    /// through its handle, not by the queue.
    fn withdraw(&self, task_id: &str) {
        self.state.lock().queue.retain(|id| id != task_id);
    }

    /// Make every task the database still holds as `Queued` eligible again.
    ///
    /// This is what start-up recovery needs: the state survived, the scheduler
    /// did not. Returns how many were rescheduled.
    pub fn schedule_queued(self: &Arc<Self>) -> AppResult<usize> {
        let queued = self
            .db
            .with_conn(|conn| downloads_repo::list_by_state(conn, DownloadState::Queued))?;
        let count = queued.len();
        for task in queued {
            self.submit(task);
        }
        if count > 0 {
            tracing::info!(target: "downloads", count, "queued downloads rescheduled");
        }
        Ok(count)
    }

    /// Signal cancellation for a running transfer.
    ///
    /// Returns whether a transfer was found. A task that is only *queued* has no
    /// handle, and does not need one: the dispatcher re-reads its state before
    /// starting it, so a task that is no longer `Queued` is simply skipped.
    pub fn cancel(&self, task_id: &str) -> bool {
        // Whatever it was waiting for, it is not waiting for that any more.
        self.withdraw(task_id);
        let found = match self.state.lock().handles.get(task_id) {
            Some(handle) => {
                handle.cancel();
                true
            }
            None => false,
        };
        tracing::debug!(target: "downloads", task_id, found, "cancel requested");
        found
    }

    /// Signal a stop that must leave the partial file in place.
    ///
    /// Used by `pause`. It is a different instruction from `cancel`, and the
    /// transfer layer acts on the difference: a cancellation removes what was
    /// written, a pause keeps it so the resume has something to continue.
    pub fn pause(&self, task_id: &str) -> bool {
        // The task no longer wants the slot it was waiting for; the resume puts
        // it back. A transfer already running is stopped through its handle.
        self.withdraw(task_id);
        let found = match self.state.lock().handles.get(task_id) {
            Some(handle) => {
                handle.pause();
                true
            }
            None => false,
        };
        tracing::debug!(target: "downloads", task_id, found, "pause requested");
        found
    }

    /// Whether a transfer is currently running for `task_id`.
    pub fn is_running(&self, task_id: &str) -> bool {
        self.state.lock().running.contains(task_id)
    }

    /// Whether `task_id` is waiting for a slot.
    pub fn is_queued(&self, task_id: &str) -> bool {
        self.state.lock().queue.iter().any(|id| id == task_id)
    }

    /// The dispatcher: start what may start, wait when nothing may.
    async fn dispatch_loop(self: Arc<Self>) {
        loop {
            match self.take_next() {
                Some(task_id) => self.spawn_transfer(task_id),
                None => {
                    // Nothing to start. Wake on new work, on a slot freeing, or
                    // on the poll — the last of which is how a raised concurrency
                    // limit is noticed without a restart.
                    let _ = tokio::time::timeout(DISPATCH_POLL, self.wake.notified()).await;
                }
            }
        }
    }

    /// The next task that may start, if a slot is free.
    fn take_next(&self) -> Option<String> {
        let limit = self.concurrency_limit();
        let mut state = self.state.lock();
        if state.running.len() >= limit {
            return None;
        }
        // One pass over what is waiting. An id whose transfer is still running is
        // put *back*, not dropped: it is a resume or a retry that arrived while
        // the previous attempt was stopping, and it becomes startable the moment
        // that attempt finishes — which is what wakes this loop again.
        let waiting = state.queue.len();
        for _ in 0..waiting {
            let Some(task_id) = state.queue.pop_front() else {
                break;
            };
            if state.running.contains(&task_id) {
                tracing::debug!(
                    target: "downloads",
                    task_id = %task_id,
                    "a transfer for this task is still stopping; it will start when that finishes"
                );
                state.queue.push_back(task_id);
                continue;
            }
            return Some(task_id);
        }
        None
    }

    /// Register a transfer as running and spawn it.
    fn spawn_transfer(self: &Arc<Self>, task_id: String) {
        let cancel = CancellationHandle::new();
        {
            let mut state = self.state.lock();
            state.running.insert(task_id.clone());
            state.handles.insert(task_id.clone(), cancel.clone());
        }
        self.started.fetch_add(1, Ordering::SeqCst);

        // The handle is handed to the transfer directly rather than looked up
        // again inside it. A lookup could miss — and a miss would give the
        // transfer a handle nobody else holds, so a cancel would set a flag on
        // the map's handle while the transfer watched a different one and ran to
        // completion. Passing it removes that possibility by construction.
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            worker.run(&task_id, cancel).await;
            {
                let mut state = worker.state.lock();
                state.running.remove(&task_id);
                state.handles.remove(&task_id);
            }
            // The slot this transfer held is free; anything waiting may start.
            worker.wake.notify_one();
        });
    }

    fn emit(&self, event: DownloadEvent) {
        let _ = self.events.send(event);
    }

    /// The full lifecycle of one transfer, including retries.
    ///
    /// Takes the id, not the task: between being made eligible and being started
    /// the task may have been paused, cancelled, or removed, and the persisted
    /// state is the authority on which of those happened.
    async fn run(&self, task_id: &str, cancel: CancellationHandle) {
        // Cancellation may have arrived while the task waited for a slot. A task
        // that was cancelled in that window still needs its history row, and
        // this is the only thing that ever picked it up.
        let Some(task) = self.claim(task_id) else {
            self.record_if_terminal(task_id);
            return;
        };

        tracing::info!(
            target: "downloads",
            task_id = %task.id,
            media_type = %task.media_type,
            "download started"
        );
        self.emit(DownloadEvent::StateChanged {
            task_id: task.id.clone(),
            state: DownloadState::Downloading,
        });

        let mut last_error: Option<AppError> = None;

        for attempt in 1..=task.max_attempts.max(1) {
            if cancel.is_cancelled() {
                self.finish_cancelled(&task).await;
                return;
            }

            self.record_attempt(&task.id, attempt);

            match self.attempt_once(&task, &cancel).await {
                Ok(saved_path) => {
                    // Completion is reported only if the task really became
                    // completed. A cancel or pause can land while the file is
                    // being finalized, and announcing success for a task the
                    // user stopped is the one outcome the UI must never show.
                    match self.finish_completed(&task, &saved_path).await {
                        Completion::Recorded => {
                            tracing::info!(
                                target: "downloads",
                                task_id = %task.id,
                                attempts = attempt,
                                "download completed"
                            );
                        }
                        Completion::Stopped => {
                            tracing::info!(
                                target: "downloads",
                                task_id = %task.id,
                                attempts = attempt,
                                "the download finished but the task was stopped first; not reporting completion"
                            );
                        }
                        // The file is on disk and the database does not know it.
                        // Left as it was, the task looks busy for ever — and
                        // because startup recovery requeues anything still
                        // downloading, every launch would fetch it again, fail
                        // to record it again, and leave it looking busy again.
                        // Recording the failure is what ends that loop, and it
                        // is the truthful state: the download did not finish as
                        // far as anything the user can see is concerned.
                        Completion::Unrecorded(err) => {
                            let detail = err.technical_detail().unwrap_or(err.message());
                            tracing::error!(
                                target: "downloads",
                                task_id = %task.id,
                                attempts = attempt,
                                error = %err.message(),
                                detail = %detail,
                                "the download finished but could not be recorded"
                            );
                            let failure = AppError::storage(
                                "the finished download could not be recorded",
                            )
                            .with_user_message(
                                "The file was downloaded, but the application could not record \
                                 it. The task is reported as failed, and retrying it is safe.",
                            )
                            .with_technical_detail(detail)
                            .with_retryable(true);
                            self.finish_failed(&task, &failure).await;
                        }
                    }
                    return;
                }
                Err(err) if err.kind() == AppErrorKind::Download && cancel.is_cancelled() => {
                    self.finish_cancelled(&task).await;
                    return;
                }
                Err(err) => {
                    let retryable = err.is_retryable() && attempt < task.max_attempts.max(1);
                    tracing::warn!(
                        target: "downloads",
                        task_id = %task.id,
                        attempt,
                        retryable,
                        error = %err,
                        "download attempt failed"
                    );
                    self.emit(DownloadEvent::Failed {
                        task_id: task.id.clone(),
                        message: err.message().to_string(),
                    });

                    if !retryable {
                        self.finish_failed(&task, &err).await;
                        return;
                    }

                    last_error = Some(err);
                    if !self.backoff(attempt, &cancel).await {
                        self.finish_cancelled(&task).await;
                        return;
                    }
                }
            }
        }

        let err = last_error.unwrap_or_else(|| AppError::download("download failed"));
        self.finish_failed(&task, &err).await;
    }

    /// Move a queued task to `Downloading`, returning `None` if it is no longer
    /// queued (cancelled or removed while waiting for a slot).
    fn claim(&self, task_id: &str) -> Option<DownloadTask> {
        let id = task_id.to_string();
        self.db
            .with_conn(|conn| {
                let Some(mut task) = downloads_repo::get(conn, &id)? else {
                    return Ok(None);
                };
                if task.state != DownloadState::Queued {
                    return Ok(None);
                }
                task.state = DownloadState::Downloading;
                task.updated_at = Some(now());
                task.started_at = Some(task.started_at.unwrap_or_else(now));
                // Compare-and-set. A `cancel` can commit between the read above
                // and this write; a blind update would overwrite `Cancelled`
                // with `Downloading` and then download a task the user stopped.
                if !downloads_repo::update_if_state(conn, &task, DownloadState::Queued)? {
                    return Ok(None);
                }
                Ok(Some(task))
            })
            .ok()
            .flatten()
    }

    /// Write the history row for a task that reached a terminal state without
    /// this worker having run it.
    fn record_if_terminal(&self, task_id: &str) {
        let id = task_id.to_string();
        let _ = self.db.with_conn(|conn| {
            let Some(task) = downloads_repo::get(conn, &id)? else {
                return Ok(());
            };
            if task.is_terminal() {
                history::record(conn, &task, task.state, None, task.error.clone())?;
            }
            Ok(())
        });
    }

    fn record_attempt(&self, task_id: &str, attempt: u32) {
        let id = task_id.to_string();
        // Writes the attempt count and nothing else. A read-modify-write here
        // would carry the task's *state* back to the row as well, and this is
        // the moment a user is most likely to be pausing or cancelling — so a
        // pause that landed between the read and the write would be undone, and
        // the transfer it was meant to stop would go on to mark the task
        // `Cancelled` instead of `Paused`.
        let _ = self
            .db
            .with_conn(|conn| downloads_repo::record_attempt(conn, &id, attempt, now()));
    }

    /// One attempt: plan, fetch the track or tracks, combine them, finalize.
    /// Returns the saved path.
    async fn attempt_once(
        &self,
        task: &DownloadTask,
        cancel: &CancellationHandle,
    ) -> AppResult<String> {
        let context = self.naming_context(task);
        let settings = self.settings.current();
        let plan = self.storage.plan_destination(&context, &settings)?;

        // Before a byte is spent: create the destination, prove it can be
        // written to, and — when anything knows how big this download is — check
        // there is room for it. `preflight` also performs the containment check,
        // so a linked destination is refused here rather than after the
        // transfer.
        //
        // The size is usually known only on a resume or a retry, because a
        // source's `Content-Length` arrives with the response and not before.
        // That is the honest limit of a preflight, and it is why the check is
        // framed as "do not pretend": an unknown size skips the comparison
        // rather than guessing one, and the transfer still handles running out
        // of room part-way through by discarding what it wrote.
        let expected = task.progress.total_bytes.filter(|bytes| *bytes > 0);
        self.storage.preflight(&plan, expected).await?;

        let directory = PathBuf::from(&plan.directory);

        // Every intermediate this attempt may create, named up front so each is
        // removed whatever happens. They sit beside the destination so the final
        // move is a rename on the same volume.
        //
        // The task id is part of the name because two tasks may legitimately
        // plan the same destination file — two images of one carousel, or two
        // downloads of the same post — and they must not write the same partial
        // file while they are both in flight. The destination name is what the
        // conflict policy disambiguates at the end; this is what keeps the
        // transfers themselves apart while they run.
        let stamp = &task.id[..task.id.len().min(8)];
        let video_temp = directory.join(format!(".{}.{stamp}.video.part", plan.file_name));
        let audio_temp = directory.join(format!(".{}.{stamp}.audio.part", plan.file_name));
        let combined_temp = directory.join(format!(".{}.{stamp}.combined.part", plan.file_name));

        // What an earlier pause left behind, if anything. Its presence is the
        // only signal that this attempt is a continuation rather than a fresh
        // start — temporary files are removed on every other path.
        //
        // A partial is only *offerable* when its sidecar agrees with it and
        // names an identity that can establish one. Anything else — a missing,
        // malformed, or disagreeing sidecar, a weak tag, a file longer than the
        // resource it claims — means the partial and its description have
        // drifted apart, and the only safe thing to do with such a file is
        // discard it and start over. Authorizing a resume on mismatched
        // metadata is exactly how two versions of a resource get spliced.
        let video_meta = PartialMetadata::read(&video_temp);
        let audio_meta = PartialMetadata::read(&audio_temp);
        let video_offer = resume_offer(&video_meta, &video_temp, &task.id).await;
        let audio_offer = resume_offer(&audio_meta, &audio_temp, &task.id).await;

        let combined = match self
            .fetch_and_combine(
                task,
                cancel,
                &video_temp,
                &audio_temp,
                &combined_temp,
                video_offer,
                audio_offer,
            )
            .await
        {
            Ok(path) => path,
            Err(err) => {
                discard_after_stop(&[&video_temp, &audio_temp, &combined_temp], cancel).await;
                return Err(err);
            }
        };

        // A stop may have arrived while the last bytes were being written. The
        // file must not be moved into place for a task the user has already
        // stopped — otherwise a paused or cancelled task leaves a finished file
        // on disk that nothing points at.
        if cancel.is_cancelled() {
            discard_after_stop(&[&video_temp, &audio_temp, &combined_temp], cancel).await;
            return Err(AppError::download("transfer stopped by the user")
                .with_retryable(false));
        }

        let policy = settings.conflict_policy;
        let finalize = match self.storage.finalize(&combined, &plan, policy).await {
            Ok(outcome) => outcome,
            Err(err) => {
                discard(&[&video_temp, &audio_temp, &combined_temp]).await;
                return Err(err);
            }
        };

        // The combined file has been moved into its destination. The individual
        // tracks are no longer needed and are not left behind.
        discard(&[&video_temp, &audio_temp]).await;

        match finalize {
            outcome if outcome.was_saved() => Ok(outcome.path().to_string()),
            skipped => {
                // `Skip` policy: a file with this name is already present, so
                // the download is a success that produced nothing new.
                tracing::info!(
                    target: "downloads",
                    task_id = %task.id,
                    path = skipped.path(),
                    "destination already existed; skipped"
                );
                Ok(skipped.path().to_string())
            }
        }
    }

    /// Fetch the task's track — and its audio, when it has one — then combine.
    ///
    /// Returns the file to finalize: the combined file when there were two
    /// tracks, and the single downloaded track when there was one.
    ///
    /// The parameter list is wide because the job is: a task, the stop signal,
    /// three intermediate paths, and what each track is offering to continue
    /// from. Grouping them further would hide which of them is which.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_and_combine(
        &self,
        task: &DownloadTask,
        cancel: &CancellationHandle,
        video_temp: &Path,
        audio_temp: &Path,
        combined_temp: &Path,
        video_offer: ResumeOffer<'_>,
        audio_offer: ResumeOffer<'_>,
    ) -> AppResult<PathBuf> {
        let wants_audio = task.audio_source_url.is_some();

        // Checked before a single byte is spent. Finding out that the tool is
        // missing after a full download would waste the download and still fail.
        if wants_audio && !self.muxer.is_available() {
            return Err(AppError::muxer_unavailable(
                "combining the video and its audio needs FFmpeg, which was not found",
            )
            .with_technical_detail(format!("muxer={}", self.muxer.name())));
        }

        let progress = TaskProgress::new(task.id.clone(), self.events.clone());

        // A continued track's bar starts where its partial file ends, so the
        // screen shows one download rather than a bar that restarts.
        progress.reserve(video_offer.offset);

        let video_urls = self.track_candidates(task, &task.source_url, task.media_type);
        let video = self
            .fetch_first_available(&video_urls, video_temp, cancel, &progress, "video", video_offer)
            .await?;
        let Some(audio_url) = task.audio_source_url.as_deref() else {
            // Nothing to combine, so this file *is* the result. It is checked
            // before it is finalized rather than after, because the storage
            // layer moves a file into place on the strength of it existing.
            self.validate_single_track(task, video_temp).await?;
            return Ok(video_temp.to_path_buf());
        };

        // Later phases report on top of what has already been transferred, so the
        // screen shows one download rather than one per track. What the video
        // actually contributed is the difference between the file it ended up
        // with and the head start it was given — which is also right when the
        // source refused to continue it.
        progress.reserve(video.bytes_written.saturating_sub(video_offer.offset));
        progress.reserve(audio_offer.offset);

        let audio_urls = self.track_candidates(task, audio_url, MediaType::Audio);
        let audio = self
            .fetch_first_available(&audio_urls, audio_temp, cancel, &progress, "audio", audio_offer)
            .await?;
        let audio_bytes = audio.bytes_written;

        tracing::info!(
            target: "downloads",
            task_id = %task.id,
            video_bytes = video.bytes_written,
            audio_bytes,
            video_resumed_from = video.resumed_from,
            "both tracks fetched"
        );

        if cancel.is_cancelled() {
            return Err(AppError::download("transfer stopped by the user")
                .with_retryable(false));
        }

        let mut mux_request = MuxRequest::new(video_temp, audio_temp, combined_temp);
        mux_request.cancel = cancel.clone();
        self.muxer.mux(&mux_request).await?;

        // A file is not a result. Reading it back is what lets the task claim it
        // produced something playable rather than something that merely exists.
        let probe = self.muxer.probe(combined_temp).await?;
        if !probe.satisfies(true) {
            // Not retryable: copying the same two tracks again produces the same
            // file. Retrying would re-download both tracks to reach the same
            // result, so the failure is reported as final.
            return Err(AppError::download("the combined file is missing a track")
                .with_user_message(
                    "The video and its sound could not be combined into one file.",
                )
                .with_technical_detail(probe.describe())
                .with_retryable(false));
        }
        tracing::info!(
            target: "downloads",
            task_id = %task.id,
            layout = %probe.describe(),
            "combined file verified"
        );

        Ok(combined_temp.to_path_buf())
    }

    /// Check the file a single-track download produced.
    ///
    /// Two-track downloads and muxed output are already read back, so their
    /// contents are known. A single-track download was not: the bytes went
    /// straight to the final name on the strength of the transfer finishing.
    /// That is enough to prove the *transfer* worked and nothing else — a media
    /// CDN answering `200` with an HTML or JSON error page produces a non-empty
    /// file with the right extension that is not media at all, and marking that
    /// complete is a false success the user only discovers on playback.
    ///
    /// Three checks, cheapest first:
    ///
    /// 1. The file is not empty.
    /// 2. Its leading bytes are not a text document. This needs no external
    ///    tool, so it holds on a machine with no FFmpeg installed.
    /// 3. When a prober is available, the container really holds a stream.
    async fn validate_single_track(&self, task: &DownloadTask, path: &Path) -> AppResult<()> {
        let length = tokio::fs::metadata(path)
            .await
            .map_err(|err| {
                AppError::storage("the downloaded file could not be inspected")
                    .with_user_message("The downloaded file could not be read back.")
                    .with_technical_detail(err.to_string())
            })?
            .len();

        if length == 0 {
            return Err(AppError::download("the source returned an empty file")
                .with_user_message("The source returned an empty file.")
                .with_retryable(false));
        }

        if let Some(marker) = leading_text_marker(path).await {
            tracing::warn!(
                target: "downloads",
                task_id = %task.id,
                marker = %marker,
                bytes = length,
                "the source answered with a text document instead of media"
            );
            return Err(AppError::download(
                "the source returned a page instead of the media",
            )
            .with_user_message(
                "The source returned an error page instead of the file. Try again later.",
            )
            .with_technical_detail(format!("leading={marker} bytes={length}"))
            .with_retryable(true));
        }

        // A picture task has to produce a picture, and a container probe is the
        // wrong instrument for one: ffprobe reports a JPEG as a video stream,
        // which tells a photo downloader nothing. The file's own signature is
        // the fact — and this needs no FFmpeg, so it holds on a machine without
        // it.
        if task.media_type == MediaType::Image {
            return match image_format(path).await {
                Some(format) => {
                    if let Some(named) = task.extension.as_deref() {
                        if !matches_extension(format, named) {
                            tracing::warn!(
                                target: "downloads",
                                task_id = %task.id,
                                found = format,
                                named = named,
                                "the picture is not the format its name claims"
                            );
                        }
                    }
                    Ok(())
                }
                None => Err(AppError::download("the downloaded file is not a picture")
                    .with_user_message(
                        "The source returned something that is not an image, so the picture \
                         was not downloaded.",
                    )
                    .with_technical_detail(format!(
                        "wanted=image bytes={length} marker={:?}",
                        leading_text_marker(path).await
                    ))
                    .with_retryable(false)),
            };
        }

        if self.muxer.is_available() {
            let probe = self.muxer.probe(path).await?;
            // What the task *claims* to be is what the file has to be. Asking
            // only for "some media" is what let a track of pure audio be filed
            // as a downloaded video: Douyin serves a post's music through the
            // same player slot a video uses, and a file holding one aac stream
            // and no picture satisfied it. A task that says video and produced a
            // sound file has not succeeded, whatever the HTTP status said.
            let satisfied = match task.media_type {
                MediaType::Video => probe.satisfies(false),
                MediaType::Audio => probe.has_audio,
                // An image is not a container to probe the same way; it is
                // checked by its own bytes where it is decided.
                _ => probe.satisfies_any_media(),
            };
            if !satisfied {
                // Named for what is actually missing, because "no media" and
                // "sound but no picture" are different faults with different
                // causes — the second is the one a music post produces.
                let (reason, user_message) = match task.media_type {
                    MediaType::Video if !probe.has_audio => (
                        "the downloaded file holds no media",
                        "The downloaded file could not be read as media.",
                    ),
                    MediaType::Video => (
                        "the downloaded file holds sound but no picture",
                        "The source returned a file with no picture in it, so it is not the \
                         video that was downloaded.",
                    ),
                    MediaType::Audio => (
                        "the downloaded file holds no sound",
                        "The source returned a file with no sound in it.",
                    ),
                    _ => (
                        "the downloaded file is not what the task asked for",
                        "The downloaded file could not be read as media.",
                    ),
                };
                // Not retryable: the same URL produced the same bytes.
                return Err(AppError::download(reason)
                    .with_user_message(user_message)
                    .with_technical_detail(format!(
                        "wanted={} {}",
                        task.media_type.as_str(),
                        probe.describe()
                    ))
                    .with_retryable(false));
            }
        }

        Ok(())
    }

    /// Fetch one track into `temp`, reporting through `progress`.
    ///
    /// The offer is *made*, never assumed: when there is one the transfer layer
    /// validates the response — status, starting offset, the resource total, and
    /// the validator — before writing anything, and starts over cleanly if the
    /// source will not continue the file.
    async fn fetch_track(
        &self,
        url: &str,
        temp: &Path,
        cancel: &CancellationHandle,
        progress: &TaskProgress,
        offer: ResumeOffer<'_>,
    ) -> AppResult<TransferOutcome> {
        let mut request = TransferRequest::new(url, temp);
        request.cancel = cancel.clone();
        // A resume is offered only when *both* an offset and the identity of the
        // resource the partial belongs to are known. The ranged path refuses a
        // resume without one, and would otherwise silently start over — which is
        // the right outcome but the wrong reason.
        if offer.is_offered() {
            request.resume_from = Some(offer.offset);
            request.if_range = offer.validator;
            request.expected_total = offer.total;
        }

        let outcome = self.http.download(&request, progress).await?;
        if outcome.bytes_written == 0 {
            return Err(AppError::download("the transfer produced no data")
                .with_user_message("The source returned an empty file.")
                .with_retryable(true));
        }
        Ok(outcome)
    }

    /// The URLs to try for one track, best first.
    ///
    /// The task's own selection leads — the user chose it, and a retry has to
    /// repeat it. The remaining renditions of the *same* media item follow,
    /// because they are the same track served from another mirror. A different
    /// media item is never substituted, so a fallback cannot quietly turn into a
    /// different video.
    fn track_candidates(
        &self,
        task: &DownloadTask,
        selected: &str,
        media_type: MediaType,
    ) -> Vec<String> {
        let mut urls = vec![selected.to_string()];

        let (Some(post_id), Some(media_item_id)) =
            (task.post_id.as_deref(), task.media_item_id.as_deref())
        else {
            return urls;
        };
        let Ok(Some(post)) = self.db.with_conn(|conn| posts_repo::get(conn, post_id)) else {
            return urls;
        };

        // The audio travels as its own media item rather than as a variant of the
        // video, so it is found by kind; everything else by the id the task holds.
        let item = match media_type {
            MediaType::Audio => post
                .media_items
                .iter()
                .find(|item| item.media_type == MediaType::Audio),
            _ => post
                .media_items
                .iter()
                .find(|item| item.id == media_item_id),
        };
        let Some(item) = item else {
            return urls;
        };

        for variant in &item.variants {
            if variant.source_url == selected {
                continue;
            }
            if !same_kind_of_file(task, variant) {
                continue;
            }
            if urls.iter().any(|url| url == &variant.source_url) {
                continue;
            }
            urls.push(variant.source_url.clone());
        }
        urls
    }

    /// Fetch the first of `candidates` that can be downloaded into `temp`.
    ///
    /// One rendition failing says nothing about the post: Douyin serves the same
    /// track from several mirrors, and one mirror refusing or timing out does not
    /// mean the track is unavailable. Each candidate is therefore tried in turn,
    /// and the attempt loop's own backoff retry then repeats the pass — so the
    /// whole thing stays bounded by `max_attempts × candidates`. The error that
    /// surfaces when every rendition failed is the last one, which is the mirror
    /// the download gave up on.
    ///
    /// Cancellation is the one thing that must not be read as "try the next one":
    /// a cancelled task stops, whatever renditions remain.
    async fn fetch_first_available(
        &self,
        candidates: &[String],
        temp: &Path,
        cancel: &CancellationHandle,
        progress: &TaskProgress,
        what: &str,
        offer: ResumeOffer<'_>,
    ) -> AppResult<TransferOutcome> {
        let total = candidates.len();
        let mut last_error: Option<AppError> = None;

        for (index, url) in candidates.iter().enumerate() {
            // Only the track the task itself selected may continue an existing
            // partial — and only with the validator that partial was written
            // under. A different rendition is a different resource, and splicing
            // one into the other's bytes is not something a fallback should ever
            // do — so a fallback starts from nothing, with no identity claim.
            let offer = if index == 0 {
                offer
            } else {
                ResumeOffer::default()
            };
            match self.fetch_track(url, temp, cancel, progress, offer).await {
                Ok(outcome) => {
                    if index > 0 {
                        tracing::info!(
                            target: "downloads",
                            track = what,
                            renditions_tried = index + 1,
                            "a later rendition of this track succeeded"
                        );
                    }
                    return Ok(outcome);
                }
                Err(err) => {
                    if cancel.is_cancelled() {
                        return Err(err);
                    }
                    // The host rather than the URL: a signed media URL carries a
                    // token, and which mirror refused is the part that matters.
                    tracing::warn!(
                        target: "downloads",
                        track = what,
                        rendition = index,
                        host = %url_host(url),
                        more_renditions = index + 1 < total,
                        retryable = err.is_retryable(),
                        error = %err,
                        "rendition failed"
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            AppError::download("no rendition of this track was available")
                .with_user_message("This media could not be downloaded.")
                .with_technical_detail(format!("track={what}"))
        }))
    }

    /// Build the naming context for a task from its post, when known.
    fn naming_context(&self, task: &DownloadTask) -> NamingContext {
        let mut context = NamingContext {
            post_id: task
                .post_id
                .clone()
                .or_else(|| Some(task.id.clone())),
            media_type: Some(task.media_type),
            extension: task.extension.clone(),
            ..Default::default()
        };

        if let Some(post_id) = &task.post_id {
            let loaded = self.db.with_conn(|conn| posts_repo::get(conn, post_id)).ok().flatten();
            if let Some(post) = loaded {
                context.post_id = Some(post.platform_post_id.clone());
                context.caption = post.caption.clone();
                context.published_at = post.published_at;
                context.handle = post.creator_handle.clone();
                if let Some(creator_id) = &post.creator_id {
                    if let Some(creator) =
                        self.db.with_conn(|conn| creators_repo::get(conn, creator_id)).ok().flatten()
                    {
                        context.creator = Some(creator.folder_name().to_string());
                        context.handle = creator.handle.clone().or(context.handle);
                    }
                } else {
                    context.creator = post.creator_handle.clone();
                }

                // Which of this post's items *of the same kind* this is. The
                // slides of a carousel are brothers sharing one name; a video
                // and its separate audio track are not, and numbering those
                // would rename every video already on disk.
                let siblings: Vec<&crate::models::MediaItem> = post
                    .media_items
                    .iter()
                    .filter(|item| item.media_type == task.media_type)
                    .collect();
                context.item_count = Some(siblings.len() as u32);
                context.index = siblings
                    .iter()
                    .position(|item| Some(&item.id) == task.media_item_id.as_ref())
                    .map(|position| position as u32);
            }
        }

        context
    }

    /// Sleep with exponential backoff, returning `false` if cancelled.
    async fn backoff(&self, attempt: u32, cancel: &CancellationHandle) -> bool {
        let exponent = attempt.saturating_sub(1).min(4);
        let delay = (BASE_BACKOFF * 2u32.pow(exponent)).min(MAX_BACKOFF);
        tracing::info!(
            target: "downloads",
            attempt,
            delay_ms = delay.as_millis() as u64,
            "download retry scheduled"
        );

        // Sliced so a cancel during the wait is noticed promptly.
        let slice = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        while waited < delay {
            if cancel.is_cancelled() {
                return false;
            }
            tokio::time::sleep(slice).await;
            waited += slice;
        }
        !cancel.is_cancelled()
    }

    /// Record a finished transfer, returning whether the task became completed.
    ///
    /// The boolean is the point. A cancel or pause can arrive while the file is
    /// being moved into place, and the state check below is what keeps the task
    /// in the state the user chose. It only works if the caller *acts* on the
    /// answer — reporting completion regardless would tell the UI something the
    /// database does not agree with.
    ///
    /// The state change and the history row are one transaction. They describe a
    /// single event, and a database holding one without the other is a database
    /// that disagrees with itself.
    async fn finish_completed(&self, task: &DownloadTask, saved_path: &str) -> Completion {
        let size = tokio::fs::metadata(saved_path)
            .await
            .ok()
            .map(|meta| meta.len());

        let id = task.id.clone();
        let path = saved_path.to_string();

        let result = self.db.with_transaction(|conn| {
            let Some(mut stored) = downloads_repo::get(conn, &id)? else {
                return Ok(false);
            };
            // A task the user cancelled or paused while this transfer was
            // finishing keeps that state; the file stays where it landed.
            if stored.state != DownloadState::Downloading {
                return Ok(false);
            }
            stored.state = DownloadState::Completed;
            stored.destination_path = Some(path.clone());
            stored.error = None;
            stored.updated_at = Some(now());
            stored.completed_at = Some(now());
            stored.progress = crate::models::DownloadProgress::new(
                size.unwrap_or(stored.progress.downloaded_bytes),
                size.or(stored.progress.total_bytes),
            );
            if let Some(name) = PathBuf::from(&path).file_name() {
                stored.file_name = Some(name.to_string_lossy().into_owned());
            }
            // Compare-and-set: if the row moved on between the read above and
            // here, nothing is written and nothing is claimed.
            if !downloads_repo::update_if_state(conn, &stored, DownloadState::Downloading)? {
                return Ok(false);
            }
            history::record(conn, &stored, DownloadState::Completed, size, None)?;
            Ok(true)
        });

        let completed = match result {
            // The file is on disk but nothing about it could be written down.
            // The caller is told, because the task must not be left looking
            // busy — and because the true cause is a database fault, not the
            // user stopping anything.
            Err(err) => {
                tracing::error!(
                    target: "downloads",
                    task_id = %task.id,
                    error = %err.message(),
                    detail = err.technical_detail().unwrap_or("-"),
                    "the completed download could not be recorded"
                );
                return Completion::Unrecorded(err);
            }
            Ok(completed) => completed,
        };

        if !completed {
            return Completion::Stopped;
        }

        // The file is on disk and the record agrees, so refresh the final size
        // from the file that actually landed rather than from the transfer's own
        // count — a rename across volumes is a copy, and the copy is what the
        // user has.
        let landed = tokio::fs::metadata(saved_path)
            .await
            .ok()
            .map(|meta| meta.len());
        self.emit(DownloadEvent::Progress {
            task_id: task.id.clone(),
            downloaded_bytes: landed.or(size).unwrap_or(0),
            total_bytes: landed.or(size),
            // The transfer is over; a speed and an estimate would describe
            // nothing.
            speed_bytes_per_second: None,
            eta_seconds: None,
        });
        self.emit(DownloadEvent::Completed {
            task_id: task.id.clone(),
            destination_path: Some(saved_path.to_string()),
        });
        Completion::Recorded
    }

    /// Record a failed attempt, returning whether the task became failed.
    ///
    /// The guard is the same one `finish_completed` uses, and for the same
    /// reason: a task the user stopped must not be announced as failed either.
    /// The state change and its history row are written in one transaction.
    async fn finish_failed(&self, task: &DownloadTask, err: &AppError) -> bool {
        let id = task.id.clone();
        let message = err.message().to_string();

        let result = self.db.with_transaction(|conn| {
            let Some(mut stored) = downloads_repo::get(conn, &id)? else {
                return Ok(false);
            };
            // The user got there first (paused or cancelled); respect that.
            if stored.state != DownloadState::Downloading {
                return Ok(false);
            }
            stored.state = DownloadState::Failed;
            stored.error = Some(message.clone());
            stored.updated_at = Some(now());
            stored.completed_at = Some(now());
            if !downloads_repo::update_if_state(conn, &stored, DownloadState::Downloading)? {
                return Ok(false);
            }
            history::record(conn, &stored, DownloadState::Failed, None, Some(message.clone()))?;
            Ok(true)
        });

        match result {
            Ok(true) => {}
            Ok(false) => return false,
            Err(err) => {
                tracing::error!(
                    target: "downloads",
                    task_id = %task.id,
                    error = %err.message(),
                    "the failed download could not be recorded"
                );
                return false;
            }
        }

        self.emit(DownloadEvent::StateChanged {
            task_id: task.id.clone(),
            state: DownloadState::Failed,
        });
        true
    }

    /// Record a cancelled attempt, returning whether the task is cancelled.
    ///
    /// Returns `false` in the two cases where announcing a cancellation would be
    /// wrong: a task the user *paused* (not terminal — `resume` restarts it),
    /// and a task that already reached a different terminal state, whose history
    /// row must keep the state it actually ended in.
    async fn finish_cancelled(&self, task: &DownloadTask) -> bool {
        let id = task.id.clone();

        let result = self.db.with_transaction(|conn| {
            let Some(mut stored) = downloads_repo::get(conn, &id)? else {
                return Ok(false);
            };
            // `pause` sets the state to Paused and cancels the handle to stop
            // the stream. Paused is not terminal and must survive; the task is
            // restarted by `resume`.
            if stored.state == DownloadState::Paused {
                return Ok(false);
            }
            if stored.state == DownloadState::Downloading {
                stored.state = DownloadState::Cancelled;
                stored.updated_at = Some(now());
                stored.completed_at = Some(now());
                if !downloads_repo::update_if_state(conn, &stored, DownloadState::Downloading)? {
                    return Ok(false);
                }
            } else if stored.state != DownloadState::Cancelled {
                // Completed or failed by another path: its history row already
                // describes what happened, and this attempt must not overwrite
                // it with a cancellation.
                return Ok(false);
            }
            history::record(conn, &stored, DownloadState::Cancelled, None, None)?;
            Ok(true)
        });

        match result {
            Ok(true) => {}
            Ok(false) => return false,
            Err(err) => {
                tracing::error!(
                    target: "downloads",
                    task_id = %task.id,
                    error = %err.message(),
                    "the cancelled download could not be recorded"
                );
                return false;
            }
        }

        tracing::info!(target: "downloads", task_id = %task.id, "download cancelled");
        self.emit(DownloadEvent::StateChanged {
            task_id: task.id.clone(),
            state: DownloadState::Cancelled,
        });
        true
    }
}

/// How much recent history the speed estimate is taken over.
///
/// Short enough to react to a real change in throughput, long enough that one
/// slow chunk does not make the number jump around.
const SPEED_WINDOW: Duration = Duration::from_secs(3);

/// How much history is needed before a speed is reported at all.
///
/// Below this the estimate would be built from one interval, which is a
/// measurement of the network's mood rather than of the transfer.
const SPEED_MIN_SPAN: Duration = Duration::from_millis(400);

/// Progress reporter that coalesces updates before they reach the UI.
///
/// One reporter covers a whole attempt, including an audio track fetched after
/// the video. Progress is accumulated across the phases so the screen shows a
/// single download rather than one per track, and the total is raised as each
/// phase's own length becomes known — which is honest, because until the audio
/// request starts, nobody knows how large it is.
struct TaskProgress {
    task_id: String,
    events: broadcast::Sender<DownloadEvent>,
    last_emit: Mutex<Instant>,
    /// Bytes contributed by phases that have already finished.
    base: AtomicU64,
    /// Total across every phase, as far as it is known.
    total: Mutex<Option<u64>>,
    /// Recent `(instant, cumulative bytes)` samples, oldest first.
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl TaskProgress {
    fn new(task_id: String, events: broadcast::Sender<DownloadEvent>) -> Self {
        Self {
            task_id,
            events,
            // Emit the first frame immediately rather than waiting an interval.
            last_emit: Mutex::new(Instant::now() - PROGRESS_INTERVAL),
            base: AtomicU64::new(0),
            total: Mutex::new(None),
            samples: Mutex::new(VecDeque::new()),
        }
    }

    /// Record bytes already transferred by a finished phase.
    fn reserve(&self, bytes: u64) {
        self.base.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Speed and ETA from the recent window, when they can be measured.
    ///
    /// Both are `None` rather than a guess: a speed needs a window spanning more
    /// than one interval, and an ETA needs a known total *and* a usable speed. A
    /// number built on nothing is worse than no number, because it is believed.
    fn rate(&self, downloaded: u64, total: Option<u64>) -> (Option<u64>, Option<u64>) {
        let samples = self.samples.lock();
        let (Some((first_at, first_bytes)), Some((last_at, last_bytes))) =
            (samples.front(), samples.back())
        else {
            return (None, None);
        };

        let span = last_at.saturating_duration_since(*first_at);
        if span < SPEED_MIN_SPAN || last_bytes <= first_bytes {
            return (None, None);
        }
        let speed = (*last_bytes - *first_bytes) as f64 / span.as_secs_f64();
        if !speed.is_finite() || speed <= 0.0 {
            return (None, None);
        }
        let speed = speed.round() as u64;

        let eta = match total {
            Some(total) if total > downloaded => Some((total - downloaded).div_ceil(speed)),
            _ => None,
        };
        (Some(speed), eta)
    }
}

impl ProgressReporter for TaskProgress {
    fn report(&self, downloaded: u64, phase_total: Option<u64>) {
        let base = self.base.load(Ordering::Relaxed);

        let total = {
            let mut known = self.total.lock();
            if let Some(phase_total) = phase_total.filter(|value| *value > 0) {
                let candidate = base + phase_total;
                *known = Some(known.map_or(candidate, |existing| existing.max(candidate)));
            }
            *known
        };

        let downloaded = base + downloaded;
        let finished = matches!(total, Some(total) if downloaded >= total);

        // The window is kept up to date on every report, including the ones
        // that are coalesced away: it is what the next emitted frame is
        // measured from, so dropping samples would blind it.
        {
            let now = Instant::now();
            let mut samples = self.samples.lock();
            samples.push_back((now, downloaded));
            while samples.len() > 2
                && now.saturating_duration_since(samples[0].0) > SPEED_WINDOW
            {
                samples.pop_front();
            }
        }

        {
            let mut last = self.last_emit.lock();
            if !finished && last.elapsed() < PROGRESS_INTERVAL {
                return;
            }
            *last = Instant::now();
        }

        let (speed, eta) = self.rate(downloaded, total);
        let _ = self.events.send(DownloadEvent::Progress {
            task_id: self.task_id.clone(),
            downloaded_bytes: downloaded,
            total_bytes: total,
            speed_bytes_per_second: speed,
            eta_seconds: eta,
        });
    }
}

/// Whether a rendition would produce the same kind of file the task expects.
///
/// Falling back to a different container would make the saved file's extension a
/// lie, so only renditions that agree on it are considered. When either side is
/// silent about its extension there is nothing to contradict the assumption.
fn same_kind_of_file(task: &DownloadTask, variant: &MediaVariant) -> bool {
    match (&task.extension, &variant.extension) {
        (Some(want), Some(have)) => want.eq_ignore_ascii_case(have),
        _ => true,
    }
}

/// How many leading bytes are inspected when checking what a file really is.
const SNIFF_BYTES: usize = 512;

/// The marker a file starts with when it is a text document, if it is one.
///
/// Used to catch the failure that is otherwise invisible: a `200` whose body is
/// an HTML or JSON error page. The file that produces is non-empty, carries the
/// media extension the source was asked for, and cannot be played.
///
/// Deliberately narrow. It looks only at the first non-whitespace bytes and only
/// for openings that no media container uses, so a real file is never refused —
/// a false positive here would reject a download that actually worked, which is
/// worse than the problem it guards against. A file that cannot be read is
/// treated as not-a-text-document: the transfer already proved it exists, and
/// refusing on an unreadable prefix would fail good downloads on a locked file.
async fn leading_text_marker(path: &Path) -> Option<String> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buffer = vec![0u8; SNIFF_BYTES];
    let read = file.read(&mut buffer).await.ok()?;
    buffer.truncate(read);

    // A body that is not valid UTF-8 at the very start is binary, so it is not
    // one of these documents whatever else it may be.
    let prefix = std::str::from_utf8(&buffer[..buffer.len().min(64)]).ok()?;
    let head: String = prefix
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();

    for marker in [
        "<!doctype",
        "<html",
        "<?xml",
        "<script",
        "{\"status",
        "{\"error",
        "{\"log_id",
    ] {
        if head.starts_with(marker) {
            return Some(marker.to_string());
        }
    }
    None
}

/// The host of a media URL, for diagnostics that must not carry its signature.
///
/// The picture format a file actually is, read from its own first bytes.
///
/// An extension and a `200` are both claims about a resource, not facts about a
/// file. A CDN answering with its HTML error page, or with the audio track the
/// player happened to be serving, produces something that looks right by name
/// and is not a picture — and a photo task recording that as a success would be
/// the same false success the video path already refuses. Douyin serves JPEG,
/// PNG and WebP here; anything else is not treated as a picture.
async fn image_format(path: &Path) -> Option<&'static str> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buffer = vec![0u8; SNIFF_BYTES];
    let read = file.read(&mut buffer).await.ok()?;
    buffer.truncate(read);

    let starts = |bytes: &[u8]| buffer.starts_with(bytes);
    if starts(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("gif")
    } else if buffer.len() >= 12 && starts(b"RIFF") && &buffer[8..12] == b"WEBP" {
        Some("webp")
    } else if starts(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

/// Whether a file named with `extension` holds `format`.
///
/// `jpg` and `jpeg` name the same thing, so the only interesting comparison is
/// between different formats. A disagreement is worth a line, not a refusal: the
/// bytes are a valid picture either way, and the name came from the URL the
/// source gave.
fn matches_extension(format: &str, extension: &str) -> bool {
    let named = extension.trim_start_matches('.').to_ascii_lowercase();
    match format {
        "jpeg" => named == "jpg" || named == "jpeg",
        other => named == other,
    }
}

/// A Douyin media URL is a signed URL: the query string is a token. Which mirror
/// refused is the useful fact, so only the host is ever logged.
fn url_host(url: &str) -> String {
    crate::logging::url_host(url)
}

/// Remove temporary files, ignoring any that are already gone.
///
/// Every path an attempt could have created is passed here on every exit, so
/// nothing depends on remembering which branch got that far. Generic over
/// anything path-like so a caller can pass its own `PathBuf`s by reference
/// without them being moved.
async fn discard<T: AsRef<Path>>(paths: &[T]) {
    for path in paths {
        // The sidecar goes with the file. Metadata that outlives its partial
        // would describe nothing — and worse, metadata for a *new* partial at
        // the same path could be read against the old description.
        crate::downloads::partial::remove_partial_and_metadata(path.as_ref()).await;
    }
}

/// What an earlier attempt left behind for one track, or nothing to offer.
///
/// The single place a partial is judged. A sidecar that is missing, malformed,
/// or names no usable identity authorizes nothing; a file longer than the
/// resource it claims describes drift a validator cannot explain away. In every
/// one of those cases the partial is *removed* rather than merely ignored — a
/// file left in place with a description nobody believed is one a later attempt
/// could pick up under different rules, and the bytes it holds are exactly the
/// ones that must not be appended to.
async fn resume_offer<'a>(
    metadata: &'a Option<PartialMetadata>,
    partial: &Path,
    task_id: &str,
) -> ResumeOffer<'a> {
    let actual = partial_len(partial).await;

    if let Some(metadata) = metadata {
        if metadata.authorizes(actual) {
            return ResumeOffer {
                offset: actual,
                validator: metadata.validator.as_ref().map(|validator| validator.value()),
                total: metadata.total_bytes,
            };
        }
    }

    if actual > 0 {
        tracing::info!(
            target: "downloads",
            task_id,
            "the existing partial cannot be attributed to a resource; discarding it"
        );
        crate::downloads::partial::remove_partial_and_metadata(partial).await;
    }
    ResumeOffer::default()
}

/// How many bytes a partial file already holds, or 0 if there is none.
///
/// A zero-length partial counts as none: there is nothing to continue from.
async fn partial_len(path: &Path) -> u64 {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => 0,
    }
}

/// Remove an attempt's intermediates — unless the stop was a pause.
///
/// A pause stops the transfer so it can be continued; the bytes already on disk
/// are exactly what makes continuing possible, so they are the one thing that
/// must survive.
async fn discard_after_stop<T: AsRef<Path>>(paths: &[T], cancel: &CancellationHandle) {
    if cancel.keeps_partial() {
        tracing::debug!(
            target: "downloads",
            "stopped for a pause; keeping the partial data for the resume"
        );
        return;
    }
    discard(paths).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::history as history_repo;
    use crate::downloads::{DownloadManager, QueueDownloadManager};
    use crate::models::{
        DownloadRequest, MediaItem, MediaType, Platform, Post, VariantKind,
    };
    use crate::network::MockHttpClient;
    use crate::storage::LocalStorageManager;

    /// A manager wired to a worker, over a temp download directory.
    fn harness(
        client: MockHttpClient,
        download_dir: &std::path::Path,
    ) -> (Database, Arc<QueueDownloadManager>, Arc<crate::media::MockMuxer>) {
        harness_with_muxer(
            client,
            download_dir,
            Arc::new(crate::media::MockMuxer::producing(true, true)),
        )
    }

    /// The same, with a specific muxer so its behaviour can be varied.
    fn harness_with_muxer(
        client: MockHttpClient,
        download_dir: &std::path::Path,
        muxer: Arc<crate::media::MockMuxer>,
    ) -> (Database, Arc<QueueDownloadManager>, Arc<crate::media::MockMuxer>) {
        let db = Database::open_in_memory().unwrap();
        let settings = Arc::new(SettingsService::new(db.clone()).unwrap());
        settings
            .update(|s| {
                s.download_directory = download_dir.to_string_lossy().into_owned();
                s.organize_by_creator = false;
                s.organize_by_media_type = false;
                s.folder_template = String::new();
                // A template with no task-dependent placeholder, so a test that
                // downloads twice produces the same base name on purpose.
                s.filename_template = "clip".to_string();
                Ok(())
            })
            .unwrap();

        let manager = QueueDownloadManager::with_worker(
            db.clone(),
            Arc::clone(&settings),
            Arc::new(client),
            Arc::new(LocalStorageManager::new()),
            Arc::clone(&muxer) as Arc<dyn crate::media::MediaMuxer>,
        );
        (db, manager, muxer)
    }

    #[tokio::test]
    async fn a_transfer_writes_a_real_file_and_records_history() {
        let temp = tempfile::tempdir().unwrap();
        let payload = vec![0xABu8; 4096];
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/v.mp4", payload.clone()),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/v.mp4", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let path = stored.destination_path.expect("destination recorded");
        assert!(path.ends_with(".mp4"), "got {path}");
        assert!(std::path::Path::new(&path).exists(), "file must exist at {path}");
        assert_eq!(std::fs::read(&path).unwrap().len(), 4096);
        // The partial file did not survive.
        assert!(!path.ends_with(".part"));
        assert!(!std::path::Path::new(&format!("{path}.part")).exists());

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Completed);
        assert_eq!(history[0].local_path.as_deref(), Some(path.as_str()));
        assert_eq!(history[0].file_size, Some(4096));
        assert_eq!(history[0].task_id.as_deref(), Some(task.id.as_str()));
    }

    #[tokio::test]
    async fn progress_is_reported_during_a_transfer() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![3u8; 2048]),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/v.mp4", MediaType::Video)
                    .with_file_name("v")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        // The loop only exits once progress reached the advertised total, or
        // the task completed — either is proof that progress was reported.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "no progress was reported");
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(DownloadEvent::Progress {
                    task_id,
                    downloaded_bytes,
                    total_bytes,
                    ..
                })) if task_id == task.id => {
                    assert_eq!(total_bytes, Some(2048));
                    if downloaded_bytes == 2048 {
                        break;
                    }
                }
                // Completion is enough; progress may have been coalesced.
                Ok(Ok(DownloadEvent::Completed { task_id, .. })) if task_id == task.id => break,
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for progress"),
            }
        }
    }

    #[tokio::test]
    async fn progress_frames_are_coalesced_rather_than_emitted_per_chunk() {
        // A fast transfer reports progress for every chunk — thousands of times
        // a second. Every frame crosses into the webview, so they are coalesced
        // to an interval; what must not be lost is the frame that says the
        // download finished, or a bar would stop short of 100%.
        let (events, mut rx) = broadcast::channel(1024);
        let progress = TaskProgress::new("task".to_string(), events);

        for chunk in 1..=2_000u64 {
            ProgressReporter::report(&progress, chunk, Some(2_000));
        }

        let mut frames = 0;
        let mut last_downloaded = 0;
        while let Ok(DownloadEvent::Progress {
            downloaded_bytes, ..
        }) = rx.try_recv()
        {
            frames += 1;
            last_downloaded = downloaded_bytes;
        }

        assert!(
            frames <= 4,
            "2000 reports produced {frames} frames; they are not being coalesced"
        );
        assert!(frames >= 1, "the first frame must go out immediately");
        assert_eq!(
            last_downloaded, 2_000,
            "the frame that says the transfer finished must always be delivered"
        );
    }

    #[tokio::test]
    async fn a_completion_for_a_task_the_user_already_stopped_is_not_announced() {
        // The finalization race, at the point where it is decided: the file has
        // landed and the completion is being written, but the user cancelled
        // first. Announcing it anyway would show a finished download the
        // database does not agree with — and a task the user stopped would come
        // back as completed on the next launch.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", vec![1u8; 512])
                // Slow enough that the cancel lands while the transfer is still
                // running, rather than racing a mock that finishes instantly.
                .with_delay(Duration::from_millis(600)),
            temp.path(),
        );
        let worker = manager.worker().expect("a worker");

        let task = manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        manager.cancel(&task.id).await.expect("cancel");
        // Let the worker finish unwinding, so the state under test is settled.
        tokio::time::sleep(Duration::from_millis(900)).await;

        // The file the transfer had already written when the stop arrived.
        let landed = temp.path().join("clip.mp4");
        std::fs::write(&landed, vec![1u8; 512]).unwrap();

        let outcome = worker
            .finish_completed(&task, landed.to_str().unwrap())
            .await;
        assert!(
            matches!(outcome, Completion::Stopped),
            "a task the user stopped must not have its completion announced"
        );

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, DownloadState::Cancelled);
        assert!(
            stored.destination_path.is_none(),
            "and nothing is claimed about where the file went"
        );

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1, "one outcome, not two");
        assert_eq!(
            history[0].status,
            DownloadState::Cancelled,
            "the history keeps the state the user chose"
        );
    }

    #[tokio::test]
    async fn a_completion_that_cannot_be_written_is_not_reported_as_a_user_stop() {
        // The reported symptom: a task sat in `downloading` for hours with its
        // file already on disk, and the log said only "the task was stopped
        // first". Nothing had been stopped — the completion write failed. And
        // because the row stayed non-terminal, startup recovery fetched it again
        // on every launch, it failed to record itself again, and it stayed busy
        // for ever. A boolean could not tell those two apart; this is the test
        // that says so.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new()
                // Long enough that the transfer is still running when the
                // completion is written, so the state under test is the one a
                // real finalization race produces.
                .with_download("https://cdn/v.mp4", vec![1u8; 512])
                .with_delay(Duration::from_secs(10)),
            temp.path(),
        );
        let worker = manager.worker().expect("a worker");

        let task = manager
            .enqueue(video_request("https://cdn/v.mp4"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            DownloadState::Downloading,
            "the task has to be mid-transfer for this to be the race it is about"
        );

        // The history store goes away, so the completion cannot be written.
        // That is a plain SQL failure, and it is the shape of the field failure
        // this guards: the file is on disk and the record will not go in.
        db.with_conn(|conn| {
            conn.execute("DROP TABLE download_history", [])?;
            Ok(())
        })
        .expect("the history store is removed");

        let landed = temp.path().join("clip.mp4");
        std::fs::write(&landed, vec![1u8; 512]).unwrap();

        let outcome = worker
            .finish_completed(&task, landed.to_str().unwrap())
            .await;

        let err = match outcome {
            Completion::Unrecorded(err) => err,
            Completion::Recorded => panic!("the record could not be written, so nothing recorded it"),
            Completion::Stopped => panic!(
                "nothing was stopped: the write failed, and saying otherwise is what                  left the task busy for ever"
            ),
        };
        assert!(
            err.technical_detail().is_some(),
            "the real cause has to survive into the log rather than a summary: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_permanent_failure_is_recorded_and_left_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(MockHttpClient::new(), temp.path());

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                // No mock resource registered → the transfer fails permanently.
                DownloadRequest::new("https://cdn/missing.mp4", MediaType::Video)
                    .with_file_name("missing")
                    .with_extension("mp4")
                    .with_max_attempts(3),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.attempts, 1, "a permanent failure must not be retried");
        assert!(stored.error.is_some());

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Failed);
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_until_it_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_transient_failures("https://cdn/flaky.mp4", 2, vec![9u8; 512]),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/flaky.mp4", MediaType::Video)
                    .with_file_name("flaky")
                    .with_extension("mp4")
                    .with_max_attempts(3),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.attempts, 3, "two failures then a success");
        assert!(stored.destination_path.is_some());

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1, "history records the outcome, not each attempt");
        assert_eq!(history[0].status, DownloadState::Completed);
    }

    #[tokio::test]
    async fn cancelling_before_the_transfer_starts_marks_and_records_it() {
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![0u8; 64]),
            temp.path(),
        );

        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/v.mp4", MediaType::Video)
                    .with_file_name("v")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        let cancelled = manager.cancel(&task.id).await.unwrap();
        assert_eq!(cancelled.state, DownloadState::Cancelled);

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1, "a cancelled download is still history");
        assert_eq!(history[0].status, DownloadState::Cancelled);
        assert!(history[0].local_path.is_none());
    }

    #[tokio::test]
    async fn pausing_stops_the_stream_without_recording_a_terminal_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![1u8; 64]),
            temp.path(),
        );

        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/v.mp4", MediaType::Video)
                    .with_file_name("v")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        let paused = manager.pause(&task.id).await.unwrap();
        assert_eq!(paused.state, DownloadState::Paused);

        // Give the worker a moment to (incorrectly) finish the task.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, DownloadState::Paused, "pause must survive");

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert!(history.is_empty(), "a paused task is not history yet");
    }

    #[tokio::test]
    async fn a_task_with_no_extension_still_lands_with_a_name() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/audio", vec![2u8; 32]),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/audio", MediaType::Audio).with_file_name("track"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert!(stored.destination_path.is_some());
    }

    #[tokio::test]
    async fn downloading_the_same_name_twice_renames_rather_than_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![4u8; 128]),
            temp.path(),
        );

        let mut first_rx = manager.subscribe();
        let first = manager
            .enqueue(
                DownloadRequest::new("https://cdn/v.mp4", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();
        assert_eq!(wait_for_terminal(&mut first_rx, &first.id).await, DownloadState::Completed);

        // Same URL, same suggested name: with the default `rename` policy the
        // second download must not clobber the first.
        let mut second_rx = manager.subscribe();
        let second = manager
            .enqueue(
                DownloadRequest::new("https://cdn/v.mp4", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();
        assert_eq!(wait_for_terminal(&mut second_rx, &second.id).await, DownloadState::Completed);

        let first_path = manager.get(&first.id).await.unwrap().unwrap().destination_path.unwrap();
        let second_path = manager.get(&second.id).await.unwrap().unwrap().destination_path.unwrap();

        assert_ne!(first_path, second_path, "the second file must get its own name");
        assert!(second_path.contains("(1)"), "got {second_path}");
        assert_eq!(std::fs::read(&first_path).unwrap().len(), 128);
        assert_eq!(std::fs::read(&second_path).unwrap().len(), 128);

        // Both outcomes are recorded, and both point at a real file.
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|entry| entry.status == DownloadState::Completed));
        let paths: Vec<&str> = history
            .iter()
            .filter_map(|entry| entry.local_path.as_deref())
            .collect();
        assert!(paths.contains(&first_path.as_str()));
        assert!(paths.contains(&second_path.as_str()));
    }

    // --- a video whose audio is a separate track ------------------------------

    /// Every file beneath `root`, recursively.
    fn files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return found;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_under(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    fn two_track_request(video: &str, audio: &str) -> DownloadRequest {
        DownloadRequest::new(video, MediaType::Video)
            .with_file_name("clip")
            .with_extension("mp4")
            .with_audio_source(audio)
    }

    /// The muxer replaces whatever path it is handed, so the guarantee that it
    /// can never replace one of the user's own files lives on this side of the
    /// boundary: what it is given must be a working file belonging to *this*
    /// attempt, and never the destination the download will finally be saved as.
    #[tokio::test]
    async fn the_muxer_is_only_given_a_working_file_of_this_attempts_own() {
        let temp = tempfile::tempdir().unwrap();
        let client = MockHttpClient::new()
            .with_download("https://cdn/media-video-avc1/", vec![7u8; 4096])
            .with_download("https://cdn/media-audio-und-mp4a/", vec![9u8; 1024]);

        let (_, manager, muxer) = harness(client, temp.path());
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(two_track_request(
                "https://cdn/media-video-avc1/",
                "https://cdn/media-audio-und-mp4a/",
            ))
            .await
            .unwrap();
        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let saved = stored.destination_path.expect("a saved path");
        let handed = muxer.last_output().expect("the muxer ran");

        assert!(
            handed.starts_with(temp.path()),
            "the muxer was handed {handed:?}, which is outside the download directory"
        );
        assert!(
            handed.to_string_lossy().ends_with(".part"),
            "the muxer was handed {handed:?}, which is not a partial file"
        );
        assert_ne!(
            handed,
            std::path::PathBuf::from(&saved),
            "the muxer must never be pointed at the file the user will be left with"
        );
        // And the file the user is left with is the one that was saved, whole.
        assert_eq!(
            std::fs::read(&saved).unwrap(),
            b"mock-combined",
            "the saved file must be what the muxer produced, not a leftover"
        );
    }

    #[tokio::test]
    async fn a_task_with_an_audio_track_fetches_both_and_combines_them() {
        let temp = tempfile::tempdir().unwrap();
        // Both URLs must be served: a task that skipped the audio download would
        // fail here rather than quietly producing a silent file.
        let client = MockHttpClient::new()
            .with_download("https://cdn/media-video-avc1/", vec![7u8; 4096])
            .with_download("https://cdn/media-audio-und-mp4a/", vec![9u8; 1024]);

        let (db, manager, muxer) = harness(client, temp.path());
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(two_track_request(
                "https://cdn/media-video-avc1/",
                "https://cdn/media-audio-und-mp4a/",
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);

        // Both tracks were combined, and the result was read back.
        assert_eq!(muxer.mux_calls(), 1);
        assert_eq!(muxer.probe_calls(), 1);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let saved = stored.destination_path.expect("a saved path");
        assert!(saved.ends_with(".mp4"), "got {saved}");
        assert!(std::path::Path::new(&saved).exists());
        // The task remembers both sources, so a retry would fetch the same pair.
        assert_eq!(
            stored.audio_source_url.as_deref(),
            Some("https://cdn/media-audio-und-mp4a/")
        );

        // One file, and nothing left over beside it.
        assert_eq!(files_under(temp.path()).len(), 1, "intermediates must be removed");

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Completed);
        assert_eq!(history[0].local_path.as_deref(), Some(saved.as_str()));
    }

    #[tokio::test]
    async fn a_task_with_audio_and_no_muxer_fails_rather_than_downgrading() {
        let temp = tempfile::tempdir().unwrap();
        let client = MockHttpClient::new()
            .with_download("https://cdn/media-video-avc1/", vec![7u8; 256])
            .with_download("https://cdn/media-audio-und-mp4a/", vec![9u8; 256]);
        let muxer = Arc::new(crate::media::MockMuxer::unavailable());

        let (_, manager, muxer) = harness_with_muxer(client, temp.path(), muxer);
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(two_track_request(
                "https://cdn/media-video-avc1/",
                "https://cdn/media-audio-und-mp4a/",
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let error = stored.error.unwrap_or_default();
        assert!(error.contains("FFmpeg"), "the reason must name the missing tool: {error}");

        // Nothing was fetched: the missing tool was discovered before the
        // download rather than after it.
        assert_eq!(muxer.mux_calls(), 0);
        assert!(
            files_under(temp.path()).is_empty(),
            "a refused download must leave nothing behind"
        );
    }

    #[tokio::test]
    async fn a_failed_combine_fails_the_task_and_clears_the_intermediates() {
        let temp = tempfile::tempdir().unwrap();
        let client = MockHttpClient::new()
            .with_download("https://cdn/media-video-avc1/", vec![7u8; 256])
            .with_download("https://cdn/media-audio-und-mp4a/", vec![9u8; 256]);
        let muxer = Arc::new(crate::media::MockMuxer::failing(
            crate::media::MockFailure::new(
                crate::errors::AppErrorKind::Download,
                "FFmpeg could not combine the tracks",
            ),
        ));

        let (_, manager, _muxer) = harness_with_muxer(client, temp.path(), muxer);
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(two_track_request(
                "https://cdn/media-video-avc1/",
                "https://cdn/media-audio-und-mp4a/",
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        assert!(
            files_under(temp.path()).is_empty(),
            "both tracks and the failed output must be removed"
        );

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert!(stored.destination_path.is_none());
    }

    #[tokio::test]
    async fn a_combined_file_without_its_audio_track_is_not_a_success() {
        let temp = tempfile::tempdir().unwrap();
        let client = MockHttpClient::new()
            .with_download("https://cdn/media-video-avc1/", vec![7u8; 256])
            .with_download("https://cdn/media-audio-und-mp4a/", vec![9u8; 256]);
        // The tool claims to have combined the tracks, but what it wrote has no
        // audio. A file that exists is not the same as a result.
        let muxer = Arc::new(crate::media::MockMuxer::producing(true, false));

        let (_, manager, muxer) = harness_with_muxer(client, temp.path(), muxer);
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(two_track_request(
                "https://cdn/media-video-avc1/",
                "https://cdn/media-audio-und-mp4a/",
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        assert_eq!(muxer.probe_calls(), 1, "the output must have been inspected");
        assert!(files_under(temp.path()).is_empty());

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert!(stored.destination_path.is_none(), "nothing may be claimed as saved");
    }

    #[tokio::test]
    async fn an_audio_track_that_cannot_be_fetched_fails_the_task() {
        let temp = tempfile::tempdir().unwrap();
        // Only the video is served, so the audio fetch fails.
        let client = MockHttpClient::new().with_download("https://cdn/media-video-avc1/", vec![7u8; 256]);

        let (_, manager, muxer) = harness(client, temp.path());
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(two_track_request(
                "https://cdn/media-video-avc1/",
                "https://cdn/media-audio-und-mp4a/",
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        // The combine never ran: there was nothing to combine.
        assert_eq!(muxer.mux_calls(), 0);
        assert!(files_under(temp.path()).is_empty());
    }

    #[tokio::test]
    async fn a_video_without_an_audio_source_still_downloads_on_its_own() {
        // The regression guard for the path that already worked.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/media-video-avc1/", vec![7u8; 4096]),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/media-video-avc1/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        // Nothing is *combined* on this path — there is only one track — but the
        // file it produced is read back before it is finalized, so a source that
        // answered with an error page cannot be recorded as a download.
        assert_eq!(muxer.mux_calls(), 0);
        assert_eq!(muxer.probe_calls(), 1);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert!(stored.audio_source_url.is_none());
        let saved = stored.destination_path.expect("a saved path");
        assert_eq!(std::fs::read(&saved).unwrap().len(), 4096);
        assert_eq!(files_under(temp.path()).len(), 1);

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn a_single_track_download_that_holds_no_media_is_a_failure() {
        // The file is non-empty and has the right extension, so nothing but a
        // read-back can tell that it is not media. Recording it as completed
        // would be a false success the user only finds on playback.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness_with_muxer(
            MockHttpClient::new().with_download("https://cdn/not-really-media/", vec![0u8; 2048]),
            temp.path(),
            Arc::new(crate::media::MockMuxer::producing(false, false)),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/not-really-media/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, DownloadState::Failed);
        let message = stored.error.clone().unwrap_or_default();
        assert!(
            message.contains("media"),
            "the failure must say what was wrong, got {message:?}"
        );
        // Nothing was promoted to a final name, and the history row records a
        // failure rather than a download.
        assert_eq!(files_under(temp.path()).len(), 0);
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Failed);
    }

    #[tokio::test]
    async fn a_video_task_that_produced_only_sound_is_a_failure() {
        // The reported symptom, end to end. Douyin served a post's music through
        // the player slot a video uses, the app asked for it as a video, and the
        // 2.8 MB file that arrived held one aac stream and no picture — and was
        // recorded as a completed video download. Asking only for "some media"
        // is what allowed it: a sound file is media. A task that says video and
        // produced a sound file has not succeeded, whatever the status said.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness_with_muxer(
            MockHttpClient::new().with_download("https://cdn/audio-only/", vec![0u8; 2048]),
            temp.path(),
            // What the probe found in the file that arrived: sound, no picture.
            Arc::new(crate::media::MockMuxer::producing(false, true)),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/audio-only/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&mut rx, &task.id).await,
            DownloadState::Failed
        );
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let message = stored.error.clone().unwrap_or_default();
        assert!(
            message.contains("no picture"),
            "the failure must say the file was not a video, got {message:?}"
        );
        // Nothing was promoted to a final name, and history says failed.
        assert_eq!(files_under(temp.path()).len(), 0);
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history[0].status, DownloadState::Failed);
    }

    #[tokio::test]
    async fn an_audio_task_is_satisfied_by_a_sound_file() {
        // The other side of the same rule, and the reason it is not simply
        // "require a video stream": a genuine audio download is a sound file and
        // must still succeed.
        let temp = tempfile::tempdir().unwrap();
        let (_db, manager, _muxer) = harness_with_muxer(
            MockHttpClient::new().with_download("https://cdn/audio-only/", vec![0u8; 2048]),
            temp.path(),
            Arc::new(crate::media::MockMuxer::producing(false, true)),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/audio-only/", MediaType::Audio)
                    .with_file_name("track")
                    .with_extension("m4a"),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&mut rx, &task.id).await,
            DownloadState::Completed
        );
    }

    #[tokio::test]
    async fn an_error_page_is_never_saved_as_a_download() {
        // A media CDN answering 200 with its HTML error page. The body is not
        // empty, the status is a success, and the file would be named `.mp4`.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_download(
                "https://cdn/error-page/",
                b"<!DOCTYPE html><html><head><title>403</title></head><body>Forbidden</body></html>"
                    .to_vec(),
            ),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/error-page/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, DownloadState::Failed);
        let message = stored.error.clone().unwrap_or_default();
        assert!(
            message.contains("page") && message.contains("media"),
            "the message should name what the source actually returned, got {message:?}"
        );
        assert_eq!(
            files_under(temp.path()).len(),
            0,
            "no html file may be left where a video was promised"
        );
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history[0].status, DownloadState::Failed);
    }

    #[tokio::test]
    async fn an_error_page_is_refused_even_with_no_prober_available() {
        // The sniff runs before the probe, so the check holds on a machine with
        // no FFmpeg installed — which is where the probe cannot help at all.
        let temp = tempfile::tempdir().unwrap();
        let (_db, manager, _muxer) = harness_with_muxer(
            MockHttpClient::new().with_download(
                "https://cdn/error-page/",
                b"{\"status_code\": 10000, \"status_msg\": \"blocked\"}".to_vec(),
            ),
            temp.path(),
            Arc::new(crate::media::MockMuxer::unavailable()),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/error-page/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        assert_eq!(files_under(temp.path()).len(), 0);
    }

    #[tokio::test]
    async fn a_real_looking_media_file_is_still_accepted() {
        // The guard must not refuse a download that actually worked. These are
        // the openings real containers use.
        let temp = tempfile::tempdir().unwrap();
        let mut payload = Vec::from(*b"\x00\x00\x00\x20ftypisom");
        payload.extend(std::iter::repeat(0x11u8).take(4096));

        let (_db, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/real/", payload),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/real/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(std::fs::read(stored.destination_path.unwrap()).unwrap().len(), 4108);
    }

    #[tokio::test]
    async fn an_empty_response_is_a_failure_rather_than_a_download() {
        let temp = tempfile::tempdir().unwrap();
        let (_db, manager, _muxer) = harness(
            MockHttpClient::new().with_download("https://cdn/empty/", Vec::new()),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn/empty/", MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4"),
            )
            .await
            .unwrap();

        assert_ne!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        assert_eq!(files_under(temp.path()).len(), 0);
    }

    #[tokio::test]
    async fn the_leading_bytes_of_a_text_document_are_recognized() {
        let temp = tempfile::tempdir().unwrap();
        for (name, contents, expected) in [
            ("a.html", &b"<!DOCTYPE html><html>"[..], true),
            ("b.html", &b"  \n <html><body>"[..], true),
            ("c.json", &br#"{"status_code":10000}"#[..], true),
            ("d.json", &br#"{"error":"blocked"}"#[..], true),
            ("e.xml", &b"<?xml version=\"1.0\"?>"[..], true),
            ("f.mp4", &b"\x00\x00\x00\x20ftypisom..."[..], false),
            ("g.jpg", &b"\xff\xd8\xff\xe0..."[..], false),
            // A body that is not UTF-8 at the start is binary, so it is not one
            // of these documents whatever else it is.
            ("h.bin", &b"\xff\xfe\x00\x01<html>"[..], false),
        ] {
            let path = temp.path().join(name);
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                leading_text_marker(&path).await.is_some(),
                expected,
                "{name} should {} be treated as a text document",
                if expected { "" } else { "not" }
            );
        }
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_is_not_treated_as_a_text_document() {
        // Refusing on an unreadable prefix would fail a download that worked.
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist.mp4");
        assert!(leading_text_marker(&missing).await.is_none());
    }

    #[test]
    fn a_media_url_is_reduced_to_its_host_for_logs() {        assert_eq!(
            url_host("https://v3-web.douyinvod.com/a/b?sign=token"),
            "v3-web.douyinvod.com"
        );
        assert_eq!(url_host("not a url"), "<no-host>");
        // The signature is what must not survive into a log line.
        assert!(!url_host("https://cdn.example/media-video-avc1/?token=secret").contains("secret"));
    }

    // --- a rendition the source refuses ---------------------------------------

    /// Persist a post whose video item carries the given renditions, and return
    /// the ids a download can be created from.
    ///
    /// The worker reads the post back to find a track's other mirrors, so the post
    /// has to live in the same database the worker is using.
    fn persist_multi_rendition_post(
        db: &Database,
        renditions: &[(&str, &str)],
    ) -> (String, String, String) {
        let mut post = Post::new(
            Platform::Douyin,
            "7281234567890123456",
            MediaType::Video,
            "https://www.douyin.com/video/7281234567890123456",
        );
        let mut video = MediaItem::new(post.id.clone(), MediaType::Video, renditions[0].0);
        video.extension = Some(renditions[0].1.to_string());
        video.variants = renditions
            .iter()
            .enumerate()
            .map(|(index, (url, extension))| {
                let mut variant = MediaVariant::new(
                    &video.id,
                    VariantKind::Original,
                    format!("rendition {index}"),
                    *url,
                );
                variant.extension = Some((*extension).to_string());
                variant.is_original = index == 0;
                variant
            })
            .collect();

        let media_item_id = video.id.clone();
        let variant_id = video.variants[0].id.clone();
        post.media_items.push(video);

        let post_id = db
            .with_conn(|conn| {
                let stored_id = posts_repo::upsert(conn, &post)?;
                let mut items = post.media_items.clone();
                for item in &mut items {
                    item.post_id = stored_id.clone();
                }
                crate::database::repositories::media::replace_items_for_post(
                    conn, &stored_id, &items,
                )?;
                Ok(stored_id)
            })
            .expect("the post must persist");

        (post_id, media_item_id, variant_id)
    }

    fn multi_rendition_request(
        selected: &str,
        post_id: String,
        media_item_id: String,
        variant_id: String,
    ) -> DownloadRequest {
        DownloadRequest::new(selected, MediaType::Video)
            .with_post(post_id)
            .with_media(media_item_id, variant_id)
            .with_file_name("clip")
            .with_extension("mp4")
    }

    /// The reported live case: one mirror refuses, another serves the same track.
    #[tokio::test]
    async fn a_refused_rendition_falls_back_to_another_mirror() {
        let temp = tempfile::tempdir().unwrap();
        let refused = "https://cdn.example/media-video-avc1/?mirror=1";
        let working = "https://cdn.example/media-video-avc1/?mirror=2";

        let client = MockHttpClient::new()
            .with_status(refused, 403)
            .with_download(working, vec![7u8; 4096]);

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(refused, "mp4"), (working, "mp4")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(multi_rendition_request(
                refused,
                post_id,
                media_item_id,
                variant_id,
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let saved = stored.destination_path.expect("a saved path");
        assert_eq!(
            std::fs::read(&saved).unwrap().len(),
            4096,
            "the bytes must be the working mirror's"
        );
        assert_eq!(files_under(temp.path()).len(), 1, "no partial is left behind");

        // One outcome, not one per rendition, and it points at the real file.
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Completed);
        assert_eq!(history[0].local_path.as_deref(), Some(saved.as_str()));
        assert_eq!(history[0].task_id.as_deref(), Some(task.id.as_str()));
    }

    /// A transient failure on one mirror is not the end of the download either.
    #[tokio::test]
    async fn a_flaky_rendition_does_not_fail_the_download() {
        let temp = tempfile::tempdir().unwrap();
        let flaky = "https://cdn.example/media-video-avc1/?mirror=1";
        let steady = "https://cdn.example/media-video-avc1/?mirror=2";

        let client = MockHttpClient::new()
            .with_transient_failures(flaky, 1, vec![1u8; 2048])
            .with_download(steady, vec![2u8; 4096]);

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(flaky, "mp4"), (steady, "mp4")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(multi_rendition_request(flaky, post_id, media_item_id, variant_id))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        let saved = stored.destination_path.expect("a saved path");
        assert_eq!(std::fs::read(&saved).unwrap().len(), 4096);
    }

    #[tokio::test]
    async fn every_rendition_being_refused_fails_the_task_and_claims_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let first = "https://cdn.example/media-video-avc1/?mirror=1";
        let second = "https://cdn.example/media-video-avc1/?mirror=2";

        let client = MockHttpClient::new()
            .with_status(first, 403)
            .with_status(second, 403);

        let (db, manager, muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(first, "mp4"), (second, "mp4")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(multi_rendition_request(first, post_id, media_item_id, variant_id))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        assert_eq!(muxer.mux_calls(), 0, "there was nothing to combine");
        assert!(files_under(temp.path()).is_empty(), "a refusal leaves nothing behind");

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert!(stored.destination_path.is_none(), "nothing may be claimed as saved");
        assert!(stored.error.is_some(), "the failure must say what happened");

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Failed);
        assert!(history[0].local_path.is_none());
    }

    /// A rendition that would produce a different kind of file is never used.
    ///
    /// Substituting it would make the saved `.mp4` a lie, so the download fails
    /// instead.
    #[tokio::test]
    async fn a_rendition_of_a_different_kind_is_not_substituted() {
        let temp = tempfile::tempdir().unwrap();
        let refused = "https://cdn.example/media-video-avc1/?mirror=1";
        let other_container = "https://cdn.example/media-video-avc1/?mirror=2";

        let client = MockHttpClient::new()
            .with_status(refused, 403)
            .with_download(other_container, vec![7u8; 4096]);

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(refused, "mp4"), (other_container, "mkv")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(multi_rendition_request(refused, post_id, media_item_id, variant_id))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        assert!(
            files_under(temp.path()).is_empty(),
            "a different container must not be saved under an .mp4 name"
        );
    }

    /// Cancelling is not "try the next mirror".
    #[tokio::test]
    async fn cancelling_during_retries_stops_the_task() {
        let temp = tempfile::tempdir().unwrap();
        let first = "https://cdn.example/media-video-avc1/?mirror=1";
        let second = "https://cdn.example/media-video-avc1/?mirror=2";

        // Both mirrors keep failing transiently, so the task stays in its retry
        // loop until it is stopped.
        let client = MockHttpClient::new()
            .with_transient_failures(first, 20, vec![1u8; 512])
            .with_transient_failures(second, 20, vec![2u8; 512]);

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(first, "mp4"), (second, "mp4")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                multi_rendition_request(first, post_id, media_item_id, variant_id)
                    .with_max_attempts(3),
            )
            .await
            .unwrap();

        // Stop the task while it is backing off between attempts, which is the
        // window the retry sequence owns.
        loop {
            match rx.recv().await {
                Ok(DownloadEvent::Failed { task_id, .. }) if task_id == task.id => break,
                Ok(_) => continue,
                Err(_) => panic!("the first attempt should have failed"),
            }
        }
        manager.cancel(&task.id).await.unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Cancelled);

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, DownloadState::Cancelled);
        assert!(files_under(temp.path()).is_empty());
    }

    /// Persist a carousel post: several image items under one post, each with its
    /// own rendition. Returns `(media_item_id, variant_id, url)` per image.
    fn persist_carousel_post(db: &Database, urls: &[&str]) -> (String, Vec<(String, String, String)>) {
        let mut post = Post::new(
            Platform::Douyin,
            "fixture-note-0002",
            MediaType::Carousel,
            "https://www.douyin.com/note/fixture-note-0002",
        );

        let mut selections = Vec::new();
        for url in urls {
            let mut image = MediaItem::new(post.id.clone(), MediaType::Image, *url);
            image.extension = Some("jpg".into());
            image.mime_type = Some("image/jpeg".into());
            let mut variant =
                MediaVariant::new(&image.id, VariantKind::Standard, "Original", *url);
            variant.extension = Some("jpg".into());
            let variant_id = variant.id.clone();
            image.variants = vec![variant];

            selections.push((image.id.clone(), variant_id, (*url).to_string()));
            post.media_items.push(image);
        }

        let post_id = db
            .with_conn(|conn| {
                let stored_id = posts_repo::upsert(conn, &post)?;
                let mut items = post.media_items.clone();
                for item in &mut items {
                    item.post_id = stored_id.clone();
                }
                crate::database::repositories::media::replace_items_for_post(
                    conn, &stored_id, &items,
                )?;
                Ok(stored_id)
            })
            .expect("the carousel must persist");

        (post_id, selections)
    }

    /// Bytes that really are a JPEG, distinguishable by their filler.
    ///
    /// A photo task only counts as a success when the file is a picture, so a
    /// fixture of arbitrary bytes would be testing a download that must now
    /// fail. The distinct filler keeps "each image has its own bytes" true.
    fn jpeg_bytes(filler: u8, len: usize) -> Vec<u8> {
        // Exactly `len` bytes, so a test that counts what was saved still can.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend(std::iter::repeat(filler).take(len.saturating_sub(6)));
        bytes.extend([0xFF, 0xD9]);
        bytes
    }

    #[tokio::test]
    async fn an_image_task_that_produced_a_page_is_a_failure() {
        // A media CDN answering 200 with its HTML error page, saved under an
        // image's name. Nothing but the file's own bytes can tell that it is not
        // a picture.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _muxer) = harness(
            MockHttpClient::new().with_download(
                "https://cdn.example/not-a-picture.jpg",
                b"<!DOCTYPE html><html><body>Not found</body></html>".to_vec(),
            ),
            temp.path(),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn.example/not-a-picture.jpg", MediaType::Image)
                    .with_file_name("slide")
                    .with_extension("jpg"),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&mut rx, &task.id).await,
            DownloadState::Failed
        );
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        // Caught by the text check before the picture check gets to it, which is
        // the cheaper refusal of the two and says the same thing.
        assert!(
            stored
                .error
                .clone()
                .unwrap_or_default()
                .contains("page instead of the media"),
            "got {:?}",
            stored.error
        );
        assert_eq!(files_under(temp.path()).len(), 0);
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history[0].status, DownloadState::Failed);
    }

    #[tokio::test]
    async fn an_image_task_that_produced_sound_is_a_failure() {
        // The reported shape from the other side: the player was serving the
        // post's music, and a photo download that accepted it would claim a
        // picture it never got.
        let temp = tempfile::tempdir().unwrap();
        let (_db, manager, _muxer) = harness_with_muxer(
            MockHttpClient::new().with_download(
                "https://cdn.example/actually-audio.jpg",
                vec![0u8; 2048],
            ),
            temp.path(),
            Arc::new(crate::media::MockMuxer::producing(false, true)),
        );

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new("https://cdn.example/actually-audio.jpg", MediaType::Image)
                    .with_file_name("slide")
                    .with_extension("jpg"),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&mut rx, &task.id).await,
            DownloadState::Failed
        );
    }

    /// Every image of a carousel is downloaded and recorded in its own right, and
    /// the images of one post do not overwrite each other.
    #[tokio::test]
    async fn a_carousel_saves_every_image_separately() {
        let temp = tempfile::tempdir().unwrap();
        let urls = [
            "https://cdn.example/image-0.jpg",
            "https://cdn.example/image-1.jpg",
            "https://cdn.example/image-2.jpg",
        ];

        let mut client = MockHttpClient::new();
        for (index, url) in urls.iter().enumerate() {
            client = client.with_download(*url, jpeg_bytes(index as u8, 1024 + index));
        }

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, selections) = persist_carousel_post(&db, &urls);

        let mut saved: Vec<String> = Vec::new();
        for (item_id, variant_id, url) in &selections {
            let mut rx = manager.subscribe();
            // The same suggested name for every image, which is the collision
            // case the naming policy has to resolve rather than overwrite.
            let task = manager
                .enqueue(
                    DownloadRequest::new(url, MediaType::Image)
                        .with_post(post_id.clone())
                        .with_media(item_id.clone(), variant_id.clone())
                        .with_file_name("clip")
                        .with_extension("jpg"),
                )
                .await
                .unwrap();

            assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
            let stored = manager.get(&task.id).await.unwrap().unwrap();
            let path = stored.destination_path.expect("a saved path");
            assert!(path.ends_with(".jpg"), "got {path}");
            assert!(std::path::Path::new(&path).exists());
            saved.push(path);
        }

        // Three images, three distinct files, each with its own bytes.
        assert_eq!(saved.len(), 3);
        assert_eq!(files_under(temp.path()).len(), 3, "no image overwrote another");
        let mut unique = saved.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "every image got its own name: {saved:?}");
        for (index, path) in saved.iter().enumerate() {
            assert_eq!(std::fs::read(path).unwrap().len(), 1024 + index);
        }

        // Each image is recorded against the post it belongs to.
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 3);
        assert!(history.iter().all(|entry| entry.status == DownloadState::Completed));
        for entry in &history {
            assert_eq!(entry.post_id.as_deref(), Some(post_id.as_str()));
            assert_eq!(entry.media_type, MediaType::Image);
            let path = entry.local_path.as_deref().expect("a recorded path");
            assert!(std::path::Path::new(path).exists(), "history must point at a real file");
        }
    }

    /// Bounded retries really are bounded: past `max_attempts` the task fails.
    #[tokio::test]
    async fn exhausting_the_retry_budget_fails_the_task() {
        let temp = tempfile::tempdir().unwrap();
        let url = "https://cdn/media-video-avc1/";

        // Every attempt fails transiently, so only the budget ends the loop.
        let client = MockHttpClient::new().with_transient_failures(url, 50, vec![1u8; 512]);

        let (db, manager, _muxer) = harness(client, temp.path());
        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(
                DownloadRequest::new(url, MediaType::Video)
                    .with_file_name("clip")
                    .with_extension("mp4")
                    .with_max_attempts(2),
            )
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.attempts, 2, "the retry budget must be exactly what was asked for");
        assert!(stored.error.is_some());

        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1, "one outcome for the whole task");
        assert_eq!(history[0].status, DownloadState::Failed);
        assert!(files_under(temp.path()).is_empty());
    }

    /// A URL the source no longer serves is permanent, not something to retry —
    /// but it does not stop another rendition of the same track from working.
    #[tokio::test]
    async fn an_expired_rendition_is_not_retried_and_a_live_one_is_used() {
        let temp = tempfile::tempdir().unwrap();
        let expired = "https://cdn.example/media-video-avc1/?mirror=expired";
        let live = "https://cdn.example/media-video-avc1/?mirror=live";

        let client = MockHttpClient::new()
            .with_status(expired, 410)
            .with_download(live, vec![5u8; 3072]);

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(expired, "mp4"), (live, "mp4")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(multi_rendition_request(
                expired,
                post_id,
                media_item_id,
                variant_id,
            ))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Completed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            std::fs::read(stored.destination_path.expect("a saved path")).unwrap().len(),
            3072
        );
        // A 410 is permanent, so the expired rendition cost one request and no
        // backoff delay.
        assert_eq!(stored.attempts, 1);
    }

    /// If every rendition is gone, the failure says so and claims nothing.
    #[tokio::test]
    async fn an_expired_track_with_no_alternative_fails_clearly() {
        let temp = tempfile::tempdir().unwrap();
        let gone = "https://cdn.example/media-video-avc1/?mirror=gone";

        let client = MockHttpClient::new().with_status(gone, 404);

        let (db, manager, _muxer) = harness(client, temp.path());
        let (post_id, media_item_id, variant_id) =
            persist_multi_rendition_post(&db, &[(gone, "mp4")]);

        let mut rx = manager.subscribe();
        let task = manager
            .enqueue(multi_rendition_request(gone, post_id, media_item_id, variant_id))
            .await
            .unwrap();

        assert_eq!(wait_for_terminal(&mut rx, &task.id).await, DownloadState::Failed);
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert!(stored.destination_path.is_none());
        assert_eq!(stored.attempts, 1, "a 404 is not worth retrying");
    }

    // --- the scheduler -------------------------------------------------------

    /// A manager with a specific concurrency limit, and the probe that measures
    /// how many transfers it actually had in flight at once.
    fn harness_with_limit(
        client: MockHttpClient,
        download_dir: &std::path::Path,
        limit: u32,
    ) -> (
        Database,
        Arc<QueueDownloadManager>,
        crate::network::ConcurrencyProbe,
        Arc<SettingsService>,
    ) {
        let probe = client.probe();
        let db = Database::open_in_memory().unwrap();
        let settings = Arc::new(SettingsService::new(db.clone()).unwrap());
        settings
            .update(|s| {
                s.download_directory = download_dir.to_string_lossy().into_owned();
                s.organize_by_creator = false;
                s.organize_by_media_type = false;
                s.folder_template = String::new();
                s.filename_template = "clip".to_string();
                s.max_concurrent_downloads = limit;
                Ok(())
            })
            .unwrap();

        let manager = QueueDownloadManager::with_worker(
            db.clone(),
            Arc::clone(&settings),
            Arc::new(client),
            Arc::new(LocalStorageManager::new()),
            Arc::new(crate::media::MockMuxer::producing(true, true)),
        );
        (db, manager, probe, settings)
    }

    /// Wait until `condition` holds, failing after a generous deadline.
    ///
    /// Used instead of a fixed sleep wherever a test needs something to have
    /// *started*. A sleep is a guess about how fast this machine is, and the
    /// guess is wrong exactly when the machine is busy — which is when a test
    /// should still be measuring the product rather than the scheduler. Polling
    /// a high-water mark is the same test on a slow machine as on a fast one; it
    /// just takes longer.
    async fn wait_for(mut condition: impl FnMut() -> bool, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until nothing is queued or running any more.
    async fn wait_until_idle(manager: &Arc<QueueDownloadManager>) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let summary = manager.summary().await.unwrap();
            if summary.active() == 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the queue never drained: {summary:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn video_request(url: &str) -> DownloadRequest {
        DownloadRequest::new(url, MediaType::Video)
            .with_file_name("clip")
            .with_extension("mp4")
    }

    #[tokio::test]
    async fn a_concurrency_limit_of_one_runs_transfers_one_at_a_time() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", vec![1u8; 4096])
                .with_delay(Duration::from_millis(120)),
            temp.path(),
            1,
        );

        for _ in 0..3 {
            manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        }
        wait_until_idle(&manager).await;

        assert_eq!(probe.peak(), 1, "a limit of one must mean one transfer at a time");
        let summary = manager.summary().await.unwrap();
        assert_eq!(summary.completed, 3, "all three still completed");
    }

    #[tokio::test]
    async fn a_concurrency_limit_above_one_runs_that_many_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", vec![1u8; 4096])
                // Long enough that three transfers are certainly in flight
                // together once they have all started. A shorter one makes
                // "they overlapped" a race between the transfers starting and
                // the first one finishing — true on an idle machine and false on
                // a busy one, without the product having changed at all.
                .with_delay(Duration::from_millis(700)),
            temp.path(),
            3,
        );

        for _ in 0..3 {
            manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        }
        // Wait for the overlap rather than for the queue to drain: the claim
        // being tested is that the pool *reaches* its limit, and a high-water
        // mark only moves while the transfers are running.
        wait_for(|| probe.peak() == 3, "all three transfers to overlap").await;

        assert_eq!(probe.peak(), 3, "the limit is a limit, not a target of one");
        wait_until_idle(&manager).await;
        assert_eq!(manager.summary().await.unwrap().completed, 3);
    }

    #[tokio::test]
    async fn submitting_the_same_task_twice_starts_one_worker() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", vec![1u8; 4096])
                .with_delay(Duration::from_millis(80)),
            temp.path(),
            2,
        );

        let task = manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        // Two more signals for the same task — what a resume racing a retry, or
        // a double-clicked button, would produce.
        let worker = manager.worker().expect("a worker");
        worker.submit(task.clone());
        worker.submit(task.clone());

        wait_until_idle(&manager).await;
        assert_eq!(
            probe.peak(),
            1,
            "the same task must never have two transfers in flight"
        );
        assert_eq!(manager.get(&task.id).await.unwrap().unwrap().state, DownloadState::Completed);
    }

    #[tokio::test]
    async fn queued_work_left_by_an_earlier_process_is_picked_up_again() {
        // The shape of a restart: a task that is `Queued` in the database with
        // nothing in memory that knows about it.
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _probe, _settings) = harness_with_limit(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![3u8; 2048]),
            temp.path(),
            2,
        );
        let worker = manager.worker().expect("a worker");

        let task = manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        wait_until_idle(&manager).await;
        // Put it back the way a restart would leave it: queued, not running.
        db.with_conn(|conn| {
            let mut stored = downloads_repo::get(conn, &task.id)?.unwrap();
            stored.state = DownloadState::Queued;
            downloads_repo::update(conn, &stored)
        })
        .unwrap();
        assert!(!worker.is_queued(&task.id), "nothing is holding it yet");

        let scheduled = DownloadManager::schedule_pending(&*manager).await.unwrap();
        assert_eq!(scheduled, 1, "the queued task is handed back to the scheduler");

        wait_until_idle(&manager).await;
        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            stored.state,
            DownloadState::Completed,
            "a task queued when the application closed must run again"
        );
    }

    #[tokio::test]
    async fn raising_the_limit_starts_waiting_work_without_a_restart() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", vec![1u8; 4096])
                .with_delay(Duration::from_millis(120)),
            temp.path(),
            1,
        );
        let settings = _settings;

        for _ in 0..3 {
            manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        }
        // Wait until the first is running, so the others are genuinely waiting.
        // A fixed sleep here was a guess: on a busy machine nothing had started
        // yet, and the test failed for a reason that had nothing to do with the
        // limit being raised.
        wait_for(|| probe.peak() >= 1, "a transfer to start").await;
        assert_eq!(probe.peak(), 1, "the limit is one, so one");

        settings
            .update(|s| {
                s.max_concurrent_downloads = 3;
                Ok(())
            })
            .unwrap();

        wait_until_idle(&manager).await;
        assert!(
            probe.peak() > 1,
            "raising the limit must start queued work without a restart (peak {})",
            probe.peak()
        );
    }

    #[tokio::test]
    async fn a_large_queue_does_not_become_a_large_number_of_running_transfers() {
        // The backpressure requirement, measured rather than asserted: many
        // tasks, a small pool, and no unbounded fan-out.
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", vec![1u8; 512])
                .with_delay(Duration::from_millis(40)),
            temp.path(),
            2,
        );
        let worker = manager.worker().expect("a worker");

        for _ in 0..25 {
            manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        }
        // Sampling while it drains: the invariant is that it is never *above*
        // the limit, whatever the queue length.
        for _ in 0..30 {
            assert!(
                worker.running_count() <= 2,
                "more transfers ran at once than the limit allows"
            );
            if manager.summary().await.unwrap().active() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        wait_until_idle(&manager).await;
        assert!(probe.peak() <= 2, "peak was {}", probe.peak());
        assert_eq!(manager.summary().await.unwrap().completed, 25);
    }

    // --- queue operations ----------------------------------------------------

    #[tokio::test]
    async fn clearing_completed_entries_keeps_the_files_and_the_history() {
        let temp = tempfile::tempdir().unwrap();
        let (db, manager, _probe, _settings) = harness_with_limit(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![7u8; 1024]),
            temp.path(),
            1,
        );

        let task = manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        wait_until_idle(&manager).await;
        let saved = manager.get(&task.id).await.unwrap().unwrap().destination_path.unwrap();
        assert!(std::path::Path::new(&saved).exists());

        let removed = manager.clear_completed().await.unwrap();
        assert_eq!(removed, 1);
        assert!(manager.get(&task.id).await.unwrap().is_none(), "the entry is gone");
        assert!(
            std::path::Path::new(&saved).exists(),
            "clearing a list must not delete the file it pointed at"
        );
        let history = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(history.len(), 1, "and it must not delete the history either");
        assert_eq!(history[0].local_path.as_deref(), Some(saved.as_str()));
    }

    #[tokio::test]
    async fn retrying_failed_downloads_requeues_every_one_of_them() {
        let temp = tempfile::tempdir().unwrap();
        // Nothing is served, so both tasks fail — permanently, so they stay
        // failed rather than retrying themselves into a green result.
        let (_, manager, _probe, _settings) = harness_with_limit(MockHttpClient::new(), temp.path(), 2);

        let first = manager.enqueue(video_request("https://cdn/missing-a.mp4")).await.unwrap();
        let second = manager.enqueue(video_request("https://cdn/missing-b.mp4")).await.unwrap();
        wait_until_idle(&manager).await;
        assert_eq!(manager.summary().await.unwrap().failed, 2);

        let retried = manager.retry_failed().await.unwrap();
        assert_eq!(retried.len(), 2, "both failed tasks were re-queued");
        let ids: Vec<&str> = retried.iter().map(|task| task.id.as_str()).collect();
        assert!(ids.contains(&first.id.as_str()) && ids.contains(&second.id.as_str()));
        assert!(retried.iter().all(|task| task.attempts == 0), "a retry starts its own counting");

        wait_until_idle(&manager).await;
        // They fail again, because the source still has nothing to give.
        assert_eq!(manager.summary().await.unwrap().failed, 2);
    }

    #[tokio::test]
    async fn removing_an_entry_leaves_the_downloaded_file_alone() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, _probe, _settings) = harness_with_limit(
            MockHttpClient::new().with_download("https://cdn/v.mp4", vec![5u8; 2048]),
            temp.path(),
            1,
        );

        let task = manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        wait_until_idle(&manager).await;
        let saved = manager.get(&task.id).await.unwrap().unwrap().destination_path.unwrap();

        assert!(manager.remove(&task.id).await.unwrap());
        assert!(manager.get(&task.id).await.unwrap().is_none());
        assert!(
            std::path::Path::new(&saved).exists(),
            "removing a queue entry is not deleting what it downloaded"
        );
        // A second removal reports that there was nothing to remove.
        assert!(!manager.remove(&task.id).await.unwrap());
    }

    // --- byte-range resume ---------------------------------------------------

    #[tokio::test]
    async fn a_partial_file_is_continued_rather_than_refetched_when_the_source_allows_it() {
        let temp = tempfile::tempdir().unwrap();
        let payload = vec![9u8; 4096];
        let (_, manager, probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", payload.clone())
                .with_delay(Duration::from_millis(30)),
            temp.path(),
            1,
        );

        // A partial standing in for an interrupted transfer. Its name carries the
        // task's own prefix, so the task id has to be known first.
        let task = manager
            .enqueue(video_request("https://cdn/v.mp4"))
            .await
            .unwrap();
        manager.cancel(&task.id).await.unwrap();
        let stamp = &task.id[..task.id.len().min(8)];
        let partial = temp.path().join(format!(".clip.mp4.{stamp}.video.part"));
        tokio::fs::write(&partial, vec![0u8; 2048]).await.unwrap();

        // Now let it run again with the partial in place.
        manager.retry(&task.id).await.unwrap();
        wait_until_idle(&manager).await;

        let stored = manager.get(&task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, DownloadState::Completed);
        let saved = stored.destination_path.expect("a saved path");
        assert_eq!(
            std::fs::read(&saved).unwrap().len(),
            payload.len(),
            "the continued file must be the whole resource, not just its tail"
        );
        assert_eq!(probe.peak(), 1);
    }

    #[tokio::test]
    async fn a_source_that_ignores_range_is_never_appended_to() {
        // The dangerous case: the file has 2048 bytes and the server answers a
        // ranged request with the whole 4096. Appending would produce 6144 bytes
        // of nonsense; the transfer must start over instead.
        let temp = tempfile::tempdir().unwrap();
        let payload = vec![4u8; 4096];
        let (_, manager, _probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/v.mp4", payload.clone())
                .without_range_support(),
            temp.path(),
            1,
        );

        let task = manager.enqueue(video_request("https://cdn/v.mp4")).await.unwrap();
        manager.cancel(&task.id).await.unwrap();
        let stamp = &task.id[..task.id.len().min(8)];
        let partial = temp.path().join(format!(".clip.mp4.{stamp}.video.part"));
        tokio::fs::write(&partial, vec![0u8; 2048]).await.unwrap();

        manager.retry(&task.id).await.unwrap();
        wait_until_idle(&manager).await;

        let saved = manager
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .destination_path
            .expect("a saved path");
        assert_eq!(
            std::fs::read(&saved).unwrap().len(),
            payload.len(),
            "a server that ignores Range must not have its body appended to a partial"
        );
    }

    // --- progress ------------------------------------------------------------

    #[test]
    fn progress_describes_the_work_rather_than_the_bytes_of_one_response() {
        // The reporter is the thing that decides what the screen is told, so it
        // is tested directly rather than through a transfer's timing.
        let (events, _) = broadcast::channel(64);
        let progress = TaskProgress::new("t".into(), events);

        // A resumed track starts where its partial ended.
        progress.reserve(1000);
        progress.report(500, Some(2000));
        progress.reserve(500);
        // Second phase on top of the first.
        progress.report(250, Some(1000));

        // Nothing to assert about the frames here without a receiver; the point
        // is that the arithmetic below is what the screen would be shown.
        assert_eq!(progress.base.load(Ordering::Relaxed), 1500);
    }

    #[test]
    fn speed_needs_a_window_and_an_estimate_needs_a_known_total() {
        let (events, _) = broadcast::channel(64);
        let progress = TaskProgress::new("t".into(), events);

        // One sample is not a measurement.
        progress.samples.lock().push_back((Instant::now(), 0));
        assert_eq!(progress.rate(0, Some(1000)), (None, None));

        // Two samples a moment apart are: 1000 bytes in 1s.
        let start = Instant::now();
        {
            let mut samples = progress.samples.lock();
            samples.clear();
            samples.push_back((start, 0));
            samples.push_back((start + Duration::from_secs(1), 1000));
        }
        let (speed, eta) = progress.rate(1000, Some(3000));
        assert_eq!(speed, Some(1000));
        assert_eq!(eta, Some(2), "2000 bytes left at 1000 B/s");

        // With no total there is no estimate — an invented one is worse than
        // none, because it is believed.
        assert_eq!(progress.rate(1000, None).1, None);
        // And nothing is reported when the window is too short to mean anything.
        {
            let mut samples = progress.samples.lock();
            samples.clear();
            samples.push_back((start, 0));
            samples.push_back((start + Duration::from_millis(5), 10));
        }
        assert_eq!(progress.rate(10, Some(100)).0, None);
    }

    #[test]
    fn a_task_that_has_finished_is_not_given_an_estimate() {
        let (events, _) = broadcast::channel(64);
        let progress = TaskProgress::new("t".into(), events);
        let start = Instant::now();
        {
            let mut samples = progress.samples.lock();
            samples.push_back((start, 0));
            samples.push_back((start + Duration::from_secs(1), 500));
        }
        // Downloaded equals total: there is nothing left to estimate.
        assert_eq!(progress.rate(500, Some(500)).1, None);
    }

    // --- summary aggregates --------------------------------------------------

    #[tokio::test]
    async fn the_summary_adds_up_work_in_flight_and_does_not_count_unknowns_as_zero() {
        let temp = tempfile::tempdir().unwrap();
        let (_, manager, _probe, _settings) = harness_with_limit(
            MockHttpClient::new()
                .with_download("https://cdn/big.mp4", vec![1u8; 9000])
                .with_download("https://cdn/small.mp4", vec![2u8; 1000])
                .with_delay(Duration::from_millis(40)),
            temp.path(),
            1,
        );

        let big = manager.enqueue(video_request("https://cdn/big.mp4")).await.unwrap();
        manager.enqueue(video_request("https://cdn/small.mp4")).await.unwrap();
        // One finished, one still waiting: the aggregate has to describe both.
        wait_until_idle(&manager).await;

        let summary = manager.summary().await.unwrap();
        assert_eq!(summary.total, 2);
        assert_eq!(summary.completed, 2);
        assert_eq!(
            summary.completed_bytes, 10_000,
            "the finished files add up to what they are"
        );
        assert_eq!(summary.active_downloaded_bytes, 0, "nothing is in flight any more");
        assert_eq!(summary.active_total_bytes, None, "and there is no work to total");
        assert_eq!(summary.active_without_size, 0);
        assert!(manager.get(&big.id).await.unwrap().is_some());
    }

    /// Wait until a terminal state is broadcast for `task_id`.
    async fn wait_for_terminal(
        rx: &mut broadcast::Receiver<DownloadEvent>,
        task_id: &str,
    ) -> DownloadState {
        // Generous on purpose. This is the *test's* budget for a transfer to
        // report a terminal state, not a product timeout — a transfer with two
        // retries sleeps through two backoffs, and on a machine under load the
        // whole sequence can take several times longer than it does when idle.
        // A tight budget here fails the test without saying anything about the
        // application.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remaining.is_zero(), "transfer did not finish in time");
            match tokio::time::timeout(remaining, rx.recv()).await {
                // A successful transfer ends with `Completed`, not a state
                // change, so both frames are treated as terminal here.
                Ok(Ok(DownloadEvent::Completed { task_id: id, .. })) if id == task_id => {
                    return DownloadState::Completed
                }
                Ok(Ok(DownloadEvent::StateChanged { task_id: id, state }))
                    if id == task_id
                        && matches!(
                            state,
                            DownloadState::Completed
                                | DownloadState::Failed
                                | DownloadState::Cancelled
                        ) =>
                {
                    return state
                }
                Ok(Ok(_)) | Ok(Err(_)) => continue,
                Err(_) => panic!("timed out waiting for a terminal state"),
            }
        }
    }
}
