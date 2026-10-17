//! Whether a downloaded file is still where the database says it is.
//!
//! Three answers, and the third is the one that matters. A file is **present**,
//! it is **missing**, or it **cannot be established** — and those are genuinely
//! different things. A folder the user deleted is missing. A drive that is not
//! plugged in, a directory the process may not read, a network share that timed
//! out: none of those say anything about whether the file exists, and reporting
//! them as "missing" would tell the user their download is gone when it may be
//! sitting on a disk that simply is not attached.
//!
//! Nothing here writes, creates, moves or deletes anything. It reads metadata
//! and reports.

use std::path::Path;

use crate::models::Timestamp;

/// What is known about one recorded destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// The file is there. The size and modification time come from the same
    /// metadata call that established it, so they cannot disagree with it.
    Present {
        size_bytes: u64,
        modified: Option<Timestamp>,
    },
    /// The file is not there, and the place it should be *is* reachable — so its
    /// absence is a fact and not a failure to look.
    Missing,
    /// Nothing can be concluded, with a short reason that is safe to log and to
    /// show. The reason is from a fixed set rather than the operating system's
    /// own text, which can carry a path.
    Unavailable { reason: &'static str },
}

/// The same three answers, without the detail.
///
/// [`FileState`] carries what the filesystem reported — a size, a modification
/// time, a reason. A decision about what to *show* needs only which of the three
/// answers it got, and taking this rather than the payload keeps that decision a
/// pure function of the state rather than of a file's numbers.
///
/// Derived from [`FileState`] by [`FileState::presence`], never produced by a
/// second inspection: one reading of the filesystem, one vocabulary for talking
/// about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The file is there.
    Present,
    /// Its absence has been established.
    Missing,
    /// Nothing can be concluded.
    Unavailable,
}

impl Presence {
    /// The wire form, and the form the window already uses.
    ///
    /// The same three words the file-state badge on the Downloads screen matches
    /// on, so a condition described on one screen is described identically on
    /// the other.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
            Self::Unavailable => "unavailable",
        }
    }
}

impl FileState {
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// Whether the file's absence has been established, as opposed to unknown.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    /// Which of the three answers this is.
    pub fn presence(&self) -> Presence {
        match self {
            Self::Present { .. } => Presence::Present,
            Self::Missing => Presence::Missing,
            Self::Unavailable { .. } => Presence::Unavailable,
        }
    }

    /// Why the answer could not be established, when it could not.
    ///
    /// Carried alongside [`Presence::Unavailable`] so a screen can say *why*
    /// rather than only *that* — "the drive refused access" and "this was saved
    /// under a different download folder" call for different words.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Unavailable { reason } => Some(reason),
            _ => None,
        }
    }

    /// A short phrase for a log line or a tooltip.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Present { .. } => "present",
            Self::Missing => "missing",
            Self::Unavailable { reason } => reason,
        }
    }
}

/// Establish the state of one recorded path.
///
/// `root` is the download directory as it is configured *now*. A path that is
/// not inside it is reported as unavailable rather than checked: if the user has
/// changed the download folder, an old recorded path belongs to a different
/// tree, and looking it up under the new one would answer a question nobody
/// asked — and would call a file missing because it is somewhere the user moved
/// the folder away from.
pub fn inspect(root: &Path, recorded: &Path) -> FileState {
    if !crate::storage::filenames::is_within(root, recorded) {
        return FileState::Unavailable {
            reason: "outside-configured-root",
        };
    }

    // Is the tree the file lives in reachable at all? A missing file inside a
    // reachable directory is missing; a directory that cannot be looked at says
    // nothing about the file.
    match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return FileState::Unavailable {
                reason: "configured-root-is-not-a-directory",
            }
        }
        Err(err) => return FileState::Unavailable { reason: reason_for(&err) },
    }

    match std::fs::metadata(recorded) {
        Ok(meta) if meta.is_file() => FileState::Present {
            size_bytes: meta.len(),
            modified: meta.modified().ok().map(Timestamp::from),
        },
        // A directory where a file should be is not this file.
        Ok(_) => FileState::Missing,
        Err(err) => match err.kind() {
            // The path is absent and its parent is reachable: it really is gone.
            std::io::ErrorKind::NotFound => {
                if directory_reachable(recorded.parent()) {
                    FileState::Missing
                } else {
                    FileState::Unavailable {
                        reason: "directory-unavailable",
                    }
                }
            }
            _ => FileState::Unavailable { reason: reason_for(&err) },
        },
    }
}

/// Whether the directory a file should be in can be looked at.
///
/// Walks up until something exists, and reports whether *that* is a readable
/// directory. A missing file whose directory is also missing is still `Missing`
/// — that is the ordinary "the user deleted the folder" case — but a directory
/// that exists and cannot be read is `Unavailable`, because the file inside it
/// cannot be ruled in or out.
fn directory_reachable(mut dir: Option<&Path>) -> bool {
    while let Some(path) = dir {
        match std::fs::metadata(path) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return true;
                }
                // Existing: reading it is the only way to know whether its
                // absence is knowable.
                return std::fs::read_dir(path).is_ok();
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                dir = path.parent();
            }
            // Something is there but cannot be examined.
            Err(_) => return false,
        }
    }
    true
}

/// A fixed, path-free reason for a filesystem failure.
///
/// The operating system's own message carries the path, and a path is the
/// user's business rather than a log's. These names say what kind of failure it
/// was, which is all a reader needs.
fn reason_for(err: &std::io::Error) -> &'static str {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => "permission-denied",
        std::io::ErrorKind::NotFound => "not-found",
        std::io::ErrorKind::TimedOut => "timed-out",
        std::io::ErrorKind::WouldBlock => "busy",
        _ => "io-error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn a_file_that_is_there_is_present_with_its_size() {
        let root = temp();
        let file = root.path().join("clip.mp4");
        std::fs::write(&file, b"1234567890").expect("write");

        match inspect(root.path(), &file) {
            FileState::Present { size_bytes, .. } => assert_eq!(size_bytes, 10),
            other => panic!("expected present, got {other:?}"),
        }
    }

    #[test]
    fn a_file_the_user_deleted_is_missing() {
        let root = temp();
        let file = root.path().join("clip.mp4");

        assert_eq!(inspect(root.path(), &file), FileState::Missing);
    }

    #[test]
    fn a_file_whose_whole_folder_is_gone_is_still_missing() {
        // The ordinary case: the user removed the creator's folder. The file is
        // absent and so is everything above it, which is a fact, not a failure
        // to look.
        let root = temp();
        let file = root.path().join("Creator").join("Videos").join("clip.mp4");

        assert_eq!(inspect(root.path(), &file), FileState::Missing);
    }

    #[test]
    fn a_path_outside_the_configured_root_is_not_called_missing() {
        // The user changed their download folder. An old row's path belongs to
        // the tree they moved away from, and looking it up under the new one
        // would answer a question nobody asked.
        let root = temp();
        let elsewhere = temp();
        let file = elsewhere.path().join("clip.mp4");
        std::fs::write(&file, b"x").expect("write");

        assert_eq!(
            inspect(root.path(), &file),
            FileState::Unavailable {
                reason: "outside-configured-root"
            }
        );
    }

    #[test]
    fn a_root_that_is_not_there_says_so_rather_than_calling_files_missing() {
        // A drive that is not attached. Every file under it is unknown, not
        // gone.
        let root = temp();
        let gone = root.path().join("not-mounted");
        let file = gone.join("Creator").join("Videos").join("clip.mp4");

        assert!(matches!(
            inspect(&gone, &file),
            FileState::Unavailable { .. }
        ));
    }

    #[test]
    fn a_directory_where_a_file_should_be_is_missing() {
        let root = temp();
        let file = root.path().join("clip.mp4");
        std::fs::create_dir(&file).expect("dir");

        assert_eq!(inspect(root.path(), &file), FileState::Missing);
    }

    #[test]
    fn a_root_that_is_itself_a_file_is_reported_rather_than_searched() {
        let root = temp();
        let not_a_dir = root.path().join("here");
        std::fs::write(&not_a_dir, b"x").expect("write");

        assert_eq!(
            inspect(&not_a_dir, &not_a_dir.join("clip.mp4")),
            FileState::Unavailable {
                reason: "configured-root-is-not-a-directory"
            }
        );
    }

    #[test]
    fn the_three_answers_carry_a_presence_a_decision_can_use() {
        // What a screen needs to know is which of the three it got, and this is
        // the only place that mapping is written. A decision that took the
        // payload would have to care how big a file was to know it was there.
        let root = temp();
        let file = root.path().join("clip.mp4");
        std::fs::write(&file, b"1234567890").expect("write");

        assert_eq!(inspect(root.path(), &file).presence(), Presence::Present);
        assert_eq!(
            inspect(root.path(), &root.path().join("gone.mp4")).presence(),
            Presence::Missing
        );
        assert_eq!(
            inspect(&root.path().join("not-mounted"), &root.path().join("not-mounted/gone.mp4"))
                .presence(),
            Presence::Unavailable
        );
    }

    #[test]
    fn only_an_answer_that_could_not_be_established_carries_a_reason() {
        let root = temp();
        let file = root.path().join("clip.mp4");
        std::fs::write(&file, b"x").expect("write");

        assert_eq!(inspect(root.path(), &file).reason(), None);
        assert_eq!(inspect(root.path(), &root.path().join("gone.mp4")).reason(), None);

        // The reason is what lets a screen say *why* rather than only *that*.
        let unreachable = inspect(&root.path().join("absent"), &root.path().join("absent/x.mp4"));
        assert!(unreachable.reason().is_some());
        assert_eq!(unreachable.presence(), Presence::Unavailable);
    }

    #[test]
    fn a_file_that_is_no_longer_known_to_be_missing_keeps_a_reason_free_answer() {
        // The counterpart: absence that *has* been established says nothing
        // about why, because there is no why — the file is simply not there.
        let root = temp();
        assert_eq!(
            inspect(root.path(), &root.path().join("never-existed.mp4")),
            FileState::Missing
        );
    }
}
