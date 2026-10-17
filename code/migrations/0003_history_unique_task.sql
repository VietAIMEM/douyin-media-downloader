-- Migration 0003 — one history row per download task.
--
-- History is written by whichever side of the pipeline finishes a task: the
-- transfer worker when a transfer stops, or the manager when a task never
-- started. Both can reach that moment at once, and "check then insert" loses
-- that race — two writers each see no existing row and each add one.
--
-- A unique index makes the invariant a property of the data rather than of the
-- code that writes it, so the insert itself can be made idempotent. Nullable by
-- design: a history row may outlive the task it came from, and SQLite treats
-- NULLs as distinct, so rows without a task id are unaffected.
--
-- Any duplicate a previous build produced is collapsed onto the earliest row
-- before the index is created.

DELETE FROM download_history
WHERE task_id IS NOT NULL
  AND rowid NOT IN (
      SELECT MIN(rowid) FROM download_history WHERE task_id IS NOT NULL GROUP BY task_id
  );

CREATE UNIQUE INDEX idx_history_task_id ON download_history (task_id);
