//! What is actually on disk, for the files the database says were downloaded.
//!
//! Reading, and nothing else. This module never creates, moves, renames or
//! deletes a file: it asks the filesystem whether something is there and reports
//! the answer. Reconciliation is a *question*, and a screen that answered it by
//! tidying up would be the exact behaviour a user cannot forgive.
//!
//! The result carries only what the filesystem knows. Everything else about a
//! task — its post, its media type, its name, its state — is already in the
//! shared task store the screens read, so the two are joined where they are
//! needed rather than copied into a second place that can drift.

use std::path::PathBuf;

use serde::Serialize;

use crate::database::repositories::downloads as downloads_repo;
use crate::errors::AppResult;
use crate::models::{Id, Timestamp};
use crate::storage::presence::{self, FileState};

use super::AppCore;

/// What the filesystem said about one finished download.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileScanEntry {
    pub task_id: Id,
    /// `present`, `missing`, or `unavailable`.
    pub state: &'static str,
    /// Why nothing could be established, when nothing could. One of a fixed set
    /// of names rather than the operating system's own text, which carries a
    /// path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// The file's size on disk, when it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// When it was last written, when the filesystem reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<Timestamp>,
}

/// One pass over every finished download, and where it looked.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileScan {
    /// The download folder as it is configured *now*. Reported so a screen can
    /// say which tree it checked, which is the first thing to know when the
    /// setting has changed.
    pub root: String,
    pub scanned_at: Timestamp,
    pub entries: Vec<FileScanEntry>,
}

impl AppCore {
    /// Ask the filesystem about every completed download, once.
    ///
    /// Deliberately not called on every render: this touches the disk for each
    /// recorded file, so it belongs behind a screen's own refresh action rather
    /// than behind a component that re-renders whenever progress arrives.
    ///
    /// Only *completed* tasks are looked at. A task that is still downloading has
    /// not finished writing, and reporting its file as missing would be a
    /// statement about a download that is working perfectly. A task that failed
    /// never produced a file at all, and saying "file missing" about it would
    /// dress a failed attempt up as a file the user has lost.
    pub fn scan_downloaded_files(&self) -> AppResult<FileScan> {
        let settings = self.settings().current();
        let root = PathBuf::from(&settings.download_directory);

        let tasks = self
            .database()
            .with_conn(|conn| downloads_repo::completed_with_a_destination(conn))?;

        let entries = tasks
            .into_iter()
            .filter_map(|task| {
                let recorded = task.destination_path.clone()?;
                let state = presence::inspect(&root, std::path::Path::new(&recorded));
                Some(match state {
                    FileState::Present {
                        size_bytes,
                        modified,
                    } => FileScanEntry {
                        task_id: task.id,
                        state: "present",
                        reason: None,
                        size_bytes: Some(size_bytes),
                        modified,
                    },
                    FileState::Missing => FileScanEntry {
                        task_id: task.id,
                        state: "missing",
                        reason: None,
                        size_bytes: None,
                        modified: None,
                    },
                    FileState::Unavailable { reason } => FileScanEntry {
                        task_id: task.id,
                        state: "unavailable",
                        reason: Some(reason),
                        size_bytes: None,
                        modified: None,
                    },
                })
            })
            .collect::<Vec<_>>();

        let present = entries.iter().filter(|entry| entry.state == "present").count();
        let missing = entries.iter().filter(|entry| entry.state == "missing").count();
        tracing::info!(
            target: "files",
            root = %root.display(),
            finished = entries.len(),
            present,
            missing,
            unknown = entries.len() - present - missing,
            "checked the recorded downloads against the disk"
        );

        Ok(FileScan {
            root: root.to_string_lossy().into_owned(),
            scanned_at: crate::models::common::now(),
            entries,
        })
    }
}
