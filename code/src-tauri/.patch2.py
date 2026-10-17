import io

p = "src/downloads/transfer.rs"
s = io.open(p, encoding="utf-8").read()

# ------------------------------------------------------------------ claim ---
old_claim = """    fn claim(&self, task_id: &str) -> Option<DownloadTask> {
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
                task.updated_at = now();
                task.started_at = Some(task.started_at.unwrap_or_else(now));
                downloads_repo::update(conn, &task)?;
                Ok(Some(task))
            })
            .ok()
            .flatten()
    }"""
new_claim = """    fn claim(&self, task_id: &str) -> Option<DownloadTask> {
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
                task.updated_at = now();
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
    }"""
assert old_claim in s, "claim not found"
s = s.replace(old_claim, new_claim, 1)

# --------------------------------------------------------- finish_failed ----
old_ff = """    async fn finish_failed(&self, task: &DownloadTask, err: &AppError) {
        let id = task.id.clone();
        let message = err.message().to_string();
        let _ = self.db.with_conn(|conn| {
            let Some(mut stored) = downloads_repo::get(conn, &id)? else {
                return Ok(());
            };
            // The user got there first (paused or cancelled); respect that.
            if stored.state != DownloadState::Downloading {
                return Ok(());
            }
            stored.state = DownloadState::Failed;
            stored.error = Some(message.clone());
            stored.updated_at = now();
            stored.completed_at = Some(now());
            crate::database::repositories::downloads::update(conn, &stored)?;
            history::record(conn, &stored, DownloadState::Failed, None, Some(message.clone()))?;
            Ok(())
        });

        self.emit(DownloadEvent::StateChanged {
            task_id: task.id.clone(),
            state: DownloadState::Failed,
        });
    }"""
new_ff = """    /// Record a failed attempt, returning whether the task became failed.
    ///
    /// The guard is the same one `finish_completed` uses, and for the same
    /// reason: a task the user stopped must not be announced as failed either.
    /// The state change and its history row are one transaction.
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
            stored.updated_at = now();
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
    }"""
assert old_ff in s, "finish_failed not found"
s = s.replace(old_ff, new_ff, 1)

# ------------------------------------------------------ finish_cancelled ----
old_fc = """    async fn finish_cancelled(&self, task: &DownloadTask) {
        let id = task.id.clone();
        let _ = self.db.with_conn(|conn| {
            let Some(mut stored) = downloads_repo::get(conn, &id)? else {
                return Ok(());
            };
            // `pause` sets the state to Paused and cancels the handle to stop
            // the stream. Paused is not terminal and must survive; the task is
            // restarted by `resume`.
            if stored.state == DownloadState::Paused {
                return Ok(());
            }
            if stored.state == DownloadState::Downloading {
                stored.state = DownloadState::Cancelled;
                stored.updated_at = now();
                stored.completed_at = Some(now());
                crate::database::repositories::downloads::update(conn, &stored)?;
            }
            history::record(conn, &stored, DownloadState::Cancelled, None, None)?;
            Ok(())
        });

        tracing::info!(target: "downloads", task_id = %task.id, "download cancelled");
        self.emit(DownloadEvent::StateChanged {
            task_id: task.id.clone(),
            state: DownloadState::Cancelled,
        });
    }"""
new_fc = """    /// Record a cancelled attempt, returning whether the task is cancelled.
    ///
    /// Returns `false` for the two cases where announcing a cancellation would
    /// be wrong: a task the user *paused* (not terminal — `resume` restarts it)
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
                stored.updated_at = now();
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
    }"""
assert old_fc in s, "finish_cancelled not found"
s = s.replace(old_fc, new_fc, 1)

io.open(p, "w", encoding="utf-8", newline="\n").write(s)
print("ok")
