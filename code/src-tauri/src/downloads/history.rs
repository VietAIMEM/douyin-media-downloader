//! Download history recording.
//!
//! A history row exists for every task that reached a terminal state, and there
//! is exactly one per task. Both halves of the download pipeline can finish a
//! task — the transfer worker when a task was in flight, the manager when a task
//! was cancelled before it ever started — so the write lives here rather than in
//! either one, and both call the same function.

use rusqlite::Connection;

use crate::database::repositories::creators as creators_repo;
use crate::database::repositories::history as history_repo;
use crate::database::repositories::posts as posts_repo;
use crate::errors::AppResult;
use crate::models::common::now;
use crate::models::{new_id, DownloadHistoryEntry, DownloadState, DownloadTask};

/// Record a task's terminal outcome against its history row.
///
/// One row per task, enforced by the unique index on `download_history.task_id`
/// rather than by a check here. Both halves of the pipeline can finish a task —
/// the transfer worker when a transfer stops, the manager when a task never
/// started — and a check-then-insert loses that race: two writers each see no
/// existing row and each add one. Letting the database settle the conflict makes
/// the outcome the same whichever gets there first.
///
/// The row then follows the task. A task is retried by resetting it, not by
/// replacing it, so the same task can reach a terminal state more than once, and
/// the row has to say what happened *last*: a download that failed and was then
/// retried to success is a download that succeeded.
///
/// The entry is enriched from the post and creator rows when they are still
/// present, so the History screen can show a title and creator without a join at
/// read time. A post that has since been removed simply leaves those fields
/// empty — the download itself is still history.
pub fn record(
    conn: &Connection,
    task: &DownloadTask,
    status: DownloadState,
    size: Option<u64>,
    error: Option<String>,
) -> AppResult<()> {
    let mut entry = DownloadHistoryEntry {
        id: new_id(),
        task_id: Some(task.id.clone()),
        post_id: task.post_id.clone(),
        post_url: None,
        creator_id: None,
        creator_name: None,
        media_type: task.media_type,
        source_url: task.source_url.clone(),
        local_path: task.destination_path.clone(),
        file_size: size,
        status,
        error,
        downloaded_at: Some(now()),
    };

    if let Some(post_id) = &task.post_id {
        if let Some(post) = posts_repo::get(conn, post_id)? {
            entry.post_url = Some(
                post.canonical_url
                    .clone()
                    .unwrap_or_else(|| post.url.clone()),
            );
            entry.creator_id = post.creator_id.clone();
            entry.creator_name = post.creator_handle.clone();

            if let Some(creator_id) = &post.creator_id {
                if let Some(creator) = creators_repo::get(conn, creator_id)? {
                    entry.creator_name = Some(creator.display_name.clone());
                }
            }
        }
    }

    history_repo::insert(conn, &entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::history as history_repo;
    use crate::database::Database;
    use crate::models::{DownloadRequest, MediaType};

    fn task() -> DownloadTask {
        let mut task = DownloadTask::new(
            &DownloadRequest::new("https://cdn.example/v.mp4", MediaType::Video)
                .with_file_name("clip"),
        );
        task.destination_path = Some("/downloads/clip.mp4".to_string());
        task
    }

    #[test]
    fn recording_the_same_task_twice_leaves_one_row() {
        let db = Database::open_in_memory().unwrap();
        let task = task();

        db.with_conn(|conn| {
            record(conn, &task, DownloadState::Completed, Some(10), None)?;
            record(conn, &task, DownloadState::Completed, Some(10), None)?;
            // A different status for the same task is still the same task.
            record(conn, &task, DownloadState::Cancelled, None, None)?;
            Ok(())
        })
        .unwrap();

        let rows = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(rows.len(), 1, "one task means one history row");
        // The row follows the task rather than the first writer: recording the
        // same task again says what happened to it most recently.
        assert_eq!(rows[0].status, DownloadState::Cancelled);
    }

    #[test]
    fn a_retried_task_history_says_what_happened_last() {
        // Measured live, and it is what this test exists for. Retrying a
        // download resets the task rather than replacing it, so the same task id
        // reaches a terminal state twice: once failed, once completed. While the
        // first write won, the History screen went on reporting that download as
        // failed — while the Downloads screen, the Library and the file on disk
        // all said it had been fetched.
        let db = Database::open_in_memory().unwrap();
        let task = task();

        db.with_conn(|conn| {
            record(conn, &task, DownloadState::Failed, None, Some("403".into()))?;
            // The retry, which succeeds.
            record(conn, &task, DownloadState::Completed, Some(4096), None)?;
            Ok(())
        })
        .unwrap();

        let rows = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(rows.len(), 1, "still one row for one task");
        assert_eq!(rows[0].status, DownloadState::Completed);
        assert!(rows[0].error.is_none(), "the old failure is not still reported");
        assert_eq!(rows[0].file_size, Some(4096));
    }

    #[test]
    fn concurrent_writers_still_produce_one_row() {
        // The uniqueness is a database guarantee, so it holds regardless of the
        // order two connections reach it — which a check-then-insert could not.
        let db = Database::open_in_memory().unwrap();
        let task = task();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            let task = task.clone();
            handles.push(std::thread::spawn(move || {
                db.with_conn(|conn| {
                    record(conn, &task, DownloadState::Cancelled, None, None)
                })
                .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let rows = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(rows.len(), 1, "concurrent writers must still produce one row");
    }

    #[test]
    fn a_history_row_without_a_task_id_is_not_constrained() {
        // Rows may outlive the task they came from; several may have no task id.
        let db = Database::open_in_memory().unwrap();
        db.with_conn(|conn| {
            for _ in 0..3 {
                let mut entry = DownloadHistoryEntry::completed(
                    "https://cdn.example/v.mp4",
                    MediaType::Video,
                    "/downloads/clip.mp4",
                );
                entry.task_id = None;
                history_repo::insert(conn, &entry)?;
            }
            Ok(())
        })
        .unwrap();

        let rows = db.with_conn(|conn| history_repo::list(conn, 10, 0)).unwrap();
        assert_eq!(rows.len(), 3);
    }
}
