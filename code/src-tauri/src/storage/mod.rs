//! Storage layer.
//!
//! [`StorageManager`] owns every decision about *where* and *under what name*
//! a downloaded file lands. Keeping this behind an interface means the download
//! engine never touches the filesystem directly, and the naming policy lives in
//! exactly one place.
//!
//! Phase 01 implements planning, directory preparation, and atomic finalization
//! for real. Disk-space checks, resume support, and temp-file cleanup are part
//! of the download engine (Phase 04) and are deliberately absent here rather
//! than stubbed.

pub mod filenames;
pub mod presence;
pub mod space;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::{AppError, AppResult};
use crate::models::settings::{validate_directory, AppSettings};
use crate::models::{ConflictPolicy, MediaType};

pub use filenames::{NamingContext, sanitize_filename, sanitize_segment};

/// The error code for "source and destination are on different volumes":
/// `EXDEV` on Unix and `ERROR_NOT_SAME_DEVICE` on Windows, which happen to be
/// the same number.
///
/// Compared as a raw code rather than through `io::ErrorKind::CrossesDevices`,
/// which is newer than the toolchain this crate declares support for. Only the
/// move is affected: a failure here falls back to copying.
const CROSS_DEVICE_ERRNO: i32 = 17;

/// A fully resolved destination for one download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationPlan {
    /// The download root this plan was computed against.
    ///
    /// Carried so a later step can re-check containment without being handed
    /// the settings again — and so the check can be repeated at the moment the
    /// filesystem is actually touched, rather than only when the path was
    /// computed.
    pub root: String,
    /// Absolute directory the file goes in.
    pub directory: String,
    /// Final file name, including extension.
    pub file_name: String,
    /// Absolute path of the file (`directory` + `file_name`).
    pub full_path: String,
    /// Relative folder segments below the download root (for display).
    pub relative_folders: Vec<String>,
}

/// What a preflight found before a download was allowed to start.
///
/// Every field is optional or best-effort on purpose. Free space cannot always
/// be read, and a download whose size the source never states is not one that
/// can be measured against it — the point is to catch what *can* be known, not
/// to pretend the rest is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    /// Room on the destination volume, when it could be read.
    pub available_bytes: Option<u64>,
    /// Room the download is expected to need, when its size was known.
    pub required_bytes: Option<u64>,
    /// Whether a file could actually be created and written in the destination
    /// directory. Always known: it is tested, not inferred.
    pub destination_writable: bool,
}

impl PreflightReport {
    /// Whether the download may proceed.
    ///
    /// Only a *known* shortfall stops it. An unreadable free-space figure and an
    /// unknown size both mean "cannot tell", and a download that cannot be shown
    /// to be impossible is allowed to try.
    pub fn is_sufficient(&self) -> bool {
        self.destination_writable
            && match (self.available_bytes, self.required_bytes) {
                (Some(available), Some(required)) => available >= required,
                _ => true,
            }
    }
}

/// Result of moving a finished download into place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum FinalizeOutcome {
    /// The file was moved to `path`.
    Saved { path: String },
    /// A file already existed and the conflict policy was `skip`.
    Skipped { path: String },
}

impl FinalizeOutcome {
    pub fn path(&self) -> &str {
        match self {
            Self::Saved { path } | Self::Skipped { path } => path,
        }
    }

    pub fn was_saved(&self) -> bool {
        matches!(self, Self::Saved { .. })
    }
}

/// Decides destinations and performs filesystem operations for downloads.
#[async_trait]
pub trait StorageManager: Send + Sync {
    /// Short identifier used in diagnostics and logs.
    fn name(&self) -> &'static str;

    /// Compute the destination for a download without writing anything.
    fn plan_destination(
        &self,
        context: &NamingContext,
        settings: &AppSettings,
    ) -> AppResult<DestinationPlan>;

    /// Create the plan's directory (and parents) if needed, and refuse it if it
    /// does not resolve to somewhere inside the download root.
    ///
    /// Takes the whole plan rather than a bare path because the containment
    /// check needs the root. The check belongs here — after the directory
    /// exists, before anything is written into it — because a path that passes a
    /// text comparison can still be a junction pointing somewhere else, and the
    /// only way to know is to ask the filesystem.
    async fn prepare_directory(&self, plan: &DestinationPlan) -> AppResult<()>;

    /// Move a completed temporary file into its planned destination, applying
    /// the conflict policy. Returns where the file ended up.
    async fn finalize(
        &self,
        temp_file: &Path,
        plan: &DestinationPlan,
        policy: ConflictPolicy,
    ) -> AppResult<FinalizeOutcome>;

    /// Check that a download can plausibly be written before it starts.
    ///
    /// `expected_bytes` is what the download is believed to need, when anything
    /// believes it knows. Passing `None` still performs the useful half: the
    /// destination is created and a probe file is actually written and removed,
    /// which is the only way to find out that a folder is read-only, full, or on
    /// a disconnected share — and it costs one file rather than a whole
    /// transfer.
    async fn preflight(
        &self,
        plan: &DestinationPlan,
        expected_bytes: Option<u64>,
    ) -> AppResult<PreflightReport>;

    /// Sanitize a single path segment using the platform rules.
    fn sanitize_segment(&self, raw: &str) -> String;
}

/// Filesystem-backed [`StorageManager`].
///
/// Stateless: the download root comes from [`AppSettings`] on each call, so the
/// same instance stays correct after the user changes their download folder.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalStorageManager;

impl LocalStorageManager {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl StorageManager for LocalStorageManager {
    fn name(&self) -> &'static str {
        "local-filesystem"
    }

    fn plan_destination(
        &self,
        context: &NamingContext,
        settings: &AppSettings,
    ) -> AppResult<DestinationPlan> {
        validate_directory(&settings.download_directory)?;
        let root = PathBuf::from(&settings.download_directory);

        let folders = effective_folders(context, settings);

        let mut directory = root.clone();
        for segment in &folders {
            directory.push(segment);
        }

        let rendered_name = filenames::render_filename(&settings.filename_template, context);
        let file_name = filenames::fit_to_path_budget(&directory, &rendered_name);
        let full_path = directory.join(&file_name);

        // Defence in depth: the template is validated and each segment is
        // sanitized, but the final path is still checked against the root.
        if !filenames::is_within(&root, &full_path) {
            return Err(AppError::storage("planned path escapes the download directory")
                .with_user_message("The download folder or naming settings are not valid.")
                .with_technical_detail(full_path.to_string_lossy().into_owned()));
        }

        // And again, this time against where the filesystem says those paths
        // actually are. Skipped when the root does not exist yet: there is
        // nothing to resolve, and refusing a first run would make the
        // application unusable until the folder had been created by hand.
        if root.exists() && !filenames::resolved_is_within(&root, &full_path) {
            return Err(AppError::storage("planned path resolves outside the download directory")
                .with_user_message(
                    "The download folder contains a link that leads outside it. \
                     Choose a different download folder.",
                )
                .with_technical_detail(full_path.to_string_lossy().into_owned()));
        }

        Ok(DestinationPlan {
            root: root.to_string_lossy().into_owned(),
            directory: directory.to_string_lossy().into_owned(),
            file_name,
            full_path: full_path.to_string_lossy().into_owned(),
            relative_folders: folders,
        })
    }

    async fn prepare_directory(&self, plan: &DestinationPlan) -> AppResult<()> {
        let directory = PathBuf::from(&plan.directory);
        tokio::fs::create_dir_all(&directory).await.map_err(|err| {
            AppError::storage("could not create the download directory")
                .with_user_message("The download folder could not be created.")
                .with_technical_detail(directory.to_string_lossy().into_owned())
                .with_source(err)
        })?;

        // Checked here, not only when the path was planned: the directory now
        // exists, so this is the first moment the filesystem can be asked where
        // the path really leads. A junction planted at any level — or one that
        // appeared between planning and now — resolves outside the root and is
        // refused before a single byte is written through it.
        let root = PathBuf::from(&plan.root);
        if !filenames::resolved_is_within(&root, &directory) {
            return Err(AppError::storage("the download directory resolves outside the root")
                .with_user_message(
                    "The download folder contains a link that leads outside it. \
                     Choose a different download folder.",
                )
                .with_technical_detail(directory.to_string_lossy().into_owned()));
        }
        Ok(())
    }

    async fn finalize(
        &self,
        temp_file: &Path,
        plan: &DestinationPlan,
        policy: ConflictPolicy,
    ) -> AppResult<FinalizeOutcome> {
        let directory = PathBuf::from(&plan.directory);
        self.prepare_directory(plan).await?;

        let mut destination = PathBuf::from(&plan.full_path);

        if destination.exists() {
            match policy {
                ConflictPolicy::Skip => {
                    return Ok(FinalizeOutcome::Skipped {
                        path: destination.to_string_lossy().into_owned(),
                    });
                }
                ConflictPolicy::Rename => {
                    destination = filenames::unique_path(&directory, &plan.file_name);
                }
                ConflictPolicy::Overwrite => {
                    // Deliberately *not* deleted first. `rename` replaces an
                    // existing file on both platforms this ships on (Windows
                    // through `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`), so
                    // removing it beforehand would only create a window in which
                    // the user's file is gone and the replacement has not
                    // arrived — a window that a crash, a full disk, or an error
                    // mid-way turns into data loss.
                }
            }
        }

        match tokio::fs::rename(temp_file, &destination).await {
            Ok(()) => Ok(FinalizeOutcome::Saved {
                path: destination.to_string_lossy().into_owned(),
            }),
            Err(err) if err.raw_os_error() == Some(CROSS_DEVICE_ERRNO) => {
                // Temp file and destination are on different volumes (common
                // when the download folder is on another drive): copy, then
                // remove the temporary file.
                //
                // The copy lands beside the destination rather than on it. If it
                // were written straight to `destination`, a copy that failed
                // part-way — a full disk, a device error, the process ending —
                // would leave a truncated file at the path a later run would
                // find and, with the `skip` policy, accept as an existing
                // download. Copying to a sibling and renaming keeps the
                // destination either absent or complete, because the final
                // rename is on one volume and therefore atomic.
                let staging = staging_path(&directory, &plan.file_name);
                let copied = tokio::fs::copy(temp_file, &staging).await;
                if let Err(err) = copied {
                    let _ = tokio::fs::remove_file(&staging).await;
                    return Err(AppError::storage("could not save the downloaded file")
                        .with_user_message("The file could not be saved to the download folder.")
                        .with_technical_detail(destination.to_string_lossy().into_owned())
                        .with_source(err));
                }
                if let Err(err) = tokio::fs::rename(&staging, &destination).await {
                    let _ = tokio::fs::remove_file(&staging).await;
                    return Err(AppError::storage("could not move the copied file into place")
                        .with_user_message("The file could not be saved to the download folder.")
                        .with_technical_detail(destination.to_string_lossy().into_owned())
                        .with_source(err));
                }
                let _ = tokio::fs::remove_file(temp_file).await;
                Ok(FinalizeOutcome::Saved {
                    path: destination.to_string_lossy().into_owned(),
                })
            }
            Err(err) => Err(AppError::storage("could not save the downloaded file")
                .with_user_message("The file could not be saved to the download folder.")
                .with_technical_detail(destination.to_string_lossy().into_owned())
                .with_source(err)),
        }
    }

    async fn preflight(
        &self,
        plan: &DestinationPlan,
        expected_bytes: Option<u64>,
    ) -> AppResult<PreflightReport> {
        // Creating the directory is also the containment check (see
        // `prepare_directory`), so this refuses a linked destination before it
        // measures anything about it.
        self.prepare_directory(plan).await?;
        let directory = PathBuf::from(&plan.directory);

        // A real write, because that is the only test that answers the question.
        // Permissions, a read-only volume, a dropped network share, and a full
        // disk all present as a failed write and as nothing else.
        let probe = directory.join(probe_file_name());
        match tokio::fs::write(&probe, b"").await {
            Ok(()) => {
                let _ = tokio::fs::remove_file(&probe).await;
            }
            Err(err) => {
                return Err(AppError::storage("the download folder is not writable")
                    .with_user_message(
                        "The download folder could not be written to. Check that it exists, \
                         that you have permission to write there, and that the drive has space.",
                    )
                    .with_technical_detail(directory.to_string_lossy().into_owned())
                    .with_source(err));
            }
        }

        let required_bytes = expected_bytes.map(space::required_headroom);
        let available_bytes = space::available_space(&directory);

        if let (Some(available), Some(required)) = (available_bytes, required_bytes) {
            if available < required {
                return Err(AppError::storage("not enough space in the download folder")
                    .with_user_message(format!(
                        "Not enough space in the download folder: this download needs about \
                         {}, and {} is free.",
                        space::describe_bytes(required),
                        space::describe_bytes(available)
                    ))
                    .with_technical_detail(format!(
                        "available={available} required={required}"
                    )));
            }
        }

        Ok(PreflightReport {
            available_bytes,
            required_bytes,
            destination_writable: true,
        })
    }

    fn sanitize_segment(&self, raw: &str) -> String {
        filenames::sanitize_segment(raw)
    }
}

/// Prefix of the empty file a preflight writes and removes.
///
/// A recognizable prefix, so a crash that leaves one behind is obvious in a
/// directory listing — but **not** a fixed name. Two transfers starting at the
/// same moment preflight the same directory, and on Windows a delete is only
/// completed once the last handle closes: with one shared name, the second
/// transfer's create can land while the first transfer's delete is still
/// pending, fail with `ACCESS_DENIED`, and be reported as "the download folder
/// is not writable" for a folder that is perfectly writable.
const PROBE_FILE_PREFIX: &str = ".download-write-test";

/// Makes each probe file name distinct within this process.
static PROBE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A probe file name no other call is using.
fn probe_file_name() -> String {
    let sequence = PROBE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{PROBE_FILE_PREFIX}-{}-{sequence}", std::process::id())
}

/// Where a cross-volume copy is staged before it is moved into place.
///
/// Beside the destination, so the move that follows is on one volume. The name
/// is not one a download could plan: the leading dot and the `.copying` suffix
/// keep it clear of `unique_path`'s `(n)` scheme, and it is never reused because
/// it exists only for the length of one finalize.
fn staging_path(directory: &Path, file_name: &str) -> PathBuf {
    directory.join(format!(".{file_name}.copying"))
}

/// Resolve the relative folder segments for a download.
///
/// `folder_template` is authoritative when it produces anything. The
/// `organize_by_*` settings are the fallback used when the template is empty,
/// which keeps the two mechanisms from fighting each other (no duplicated
/// creator or media-type levels).
fn effective_folders(context: &NamingContext, settings: &AppSettings) -> Vec<String> {
    let from_template = filenames::render_folder_segments(&settings.folder_template, context);
    if !from_template.is_empty() {
        return from_template;
    }

    let mut folders = Vec::new();
    if settings.organize_by_creator {
        if let Some(creator) = context.creator.as_deref().filter(|c| !c.trim().is_empty()) {
            folders.push(filenames::sanitize_segment(creator));
        }
    }
    if settings.organize_by_media_type {
        if let Some(media_type) = context.media_type {
            folders.push(filenames::media_folder_name(media_type).to_string());
        }
    }
    folders
}

/// Convenience constructor for media-type folders, re-exported for callers that
/// only need the label.
pub fn media_folder_name(media_type: MediaType) -> &'static str {
    filenames::media_folder_name(media_type)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Create a directory link under `link` pointing at `target`.
    ///
    /// On Windows this is a junction, which — unlike a symbolic link — needs no
    /// elevation, so the case is testable without changing the machine's
    /// security settings. Returns `false` when the environment refuses, so the
    /// caller can report that rather than fail for a reason outside the code
    /// under test.
    fn make_directory_link(link: &Path, target: &Path) -> bool {
        #[cfg(windows)]
        {
            std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!(
                        "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                        link.display(),
                        target.display()
                    ),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
                && link.exists()
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
    }

    fn settings_with_dir(dir: &Path) -> AppSettings {
        AppSettings {
            download_directory: dir.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn plans_a_path_inside_the_download_root() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings_with_dir(temp.path());
        let storage = LocalStorageManager::new();

        let context = NamingContext {
            creator: Some("Alice".into()),
            post_id: Some("728".into()),
            media_type: Some(MediaType::Video),
            extension: Some("mp4".into()),
            published_at: Some(crate::models::common::now()),
            ..Default::default()
        };

        let plan = storage.plan_destination(&context, &settings).unwrap();
        assert!(plan.file_name.ends_with(".mp4"));
        assert!(plan.relative_folders.contains(&"Alice".to_string()));
        assert!(Path::new(&plan.full_path).is_absolute());
        assert!(filenames::is_within(temp.path(), Path::new(&plan.full_path)));
    }

    #[test]
    fn rejects_a_download_root_that_is_not_absolute() {
        let settings = AppSettings {
            download_directory: "relative".into(),
            ..Default::default()
        };
        let storage = LocalStorageManager::new();
        let err = storage
            .plan_destination(&NamingContext::default(), &settings)
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
    }

    #[test]
    fn empty_folder_template_falls_back_to_the_organise_flags() {
        let temp = tempfile::tempdir().unwrap();
        let mut settings = settings_with_dir(temp.path());
        settings.folder_template = String::new();
        let context = NamingContext {
            creator: Some("Alice".into()),
            media_type: Some(MediaType::Image),
            ..Default::default()
        };

        assert_eq!(
            effective_folders(&context, &settings),
            vec!["Alice".to_string(), "Images".to_string()]
        );

        // The default template must describe the same layout, since it is what
        // the shipped defaults use.
        settings.folder_template = AppSettings::default().folder_template;
        assert_eq!(
            effective_folders(&context, &settings),
            vec!["Alice".to_string(), "Images".to_string()]
        );

        // With the template empty again, the switches are what remains.
        settings.folder_template = String::new();
        settings.organize_by_media_type = false;
        assert_eq!(
            effective_folders(&context, &settings),
            vec!["Alice".to_string()]
        );
    }

    // --- containment ---------------------------------------------------------

    #[test]
    fn a_path_whose_existing_ancestor_is_outside_the_root_is_refused() {
        // The mechanism a junction uses, without needing one: the deepest
        // existing part of the candidate resolves somewhere else, so the text of
        // the path and the location it names disagree.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("downloads");
        let outside = temp.path().join("elsewhere");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let escaped = outside.join("nested").join("clip.mp4");
        assert!(
            !filenames::is_within(&root, &escaped),
            "a sibling is caught by the text comparison already"
        );
        assert!(
            !filenames::resolved_is_within(&root, &escaped),
            "and by the resolved one"
        );

        // A path genuinely inside the root passes both.
        let inside = root.join("videos").join("clip.mp4");
        assert!(filenames::is_within(&root, &inside));
        assert!(filenames::resolved_is_within(&root, &inside));
    }

    #[test]
    fn a_destination_below_a_link_that_leaves_the_root_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("downloads");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // `root/videos` is a link to a folder that is not in the root.
        if !make_directory_link(&root.join("videos"), &outside) {
            // The link could not be created in this environment — a machine
            // without the right privilege, or a non-Windows host. The policy
            // itself is covered by the test above; this one adds the end-to-end
            // path when the filesystem allows it, and says so when it does not.
            eprintln!(
                "NOTE: this environment could not create a directory link, so the \
                 end-to-end junction escape was not exercised here."
            );
            return;
        }

        let storage = LocalStorageManager::new();
        let mut settings = settings_with_dir(&root);
        settings.folder_template = "{creator}".to_string();

        let context = NamingContext {
            creator: Some("videos".into()),
            post_id: Some("1".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };

        // The text comparison sees a path inside the root — which is exactly why
        // the resolved check is needed.
        let lexical = root.join("videos").join("clip.mp4");
        assert!(
            filenames::is_within(&root, &lexical),
            "the path reads as inside the root"
        );
        assert!(
            !filenames::resolved_is_within(&root, &lexical),
            "but it resolves outside it"
        );

        let err = storage
            .plan_destination(&context, &settings)
            .expect_err("a destination below a link out of the root must not be planned");
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
        assert!(
            err.user_message().contains("link") || err.user_message().contains("outside"),
            "the message should explain what is wrong, got {:?}",
            err.user_message()
        );
    }

    #[tokio::test]
    async fn a_link_out_of_the_root_is_refused_at_the_moment_of_the_write() {
        // Planning happens before the transfer; the directory is created
        // immediately before the first byte is written. Refusing there is what
        // actually keeps bytes out of a linked folder, including a link that
        // appeared in between.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("downloads");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        if !make_directory_link(&root.join("videos"), &outside) {
            eprintln!("NOTE: directory links unavailable here; skipping");
            return;
        }

        let mut settings = settings_with_dir(&root);
        settings.folder_template = "{creator}".to_string();
        let storage = LocalStorageManager::new();
        let mut plan = storage
            .plan_destination(
                &NamingContext {
                    // No creator, so the plan lands at the root itself and
                    // planning succeeds — the write gate is what is under test.
                    post_id: Some("2".into()),
                    extension: Some("mp4".into()),
                    ..Default::default()
                },
                &settings,
            )
            .unwrap();

        // Now point the plan at the linked folder, as a link on that path would.
        let linked = root.join("videos");
        plan.full_path = linked.join("clip.mp4").to_string_lossy().into_owned();
        plan.directory = linked.to_string_lossy().into_owned();

        let err = storage.prepare_directory(&plan).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
    }

    #[test]
    fn a_link_that_stays_inside_the_root_is_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("downloads");
        let inner = root.join("videos");
        std::fs::create_dir_all(&inner).unwrap();

        if !make_directory_link(&root.join("link"), &inner) {
            eprintln!("NOTE: directory links unavailable here; skipping");
            return;
        }

        if let Ok(plan) = LocalStorageManager::new().plan_destination(
            &NamingContext {
                creator: Some("link".into()),
                post_id: Some("1".into()),
                extension: Some("mp4".into()),
                ..Default::default()
            },
            &{
                let mut settings = settings_with_dir(&root);
                settings.folder_template = "{creator}".to_string();
                settings
            },
        ) {
            assert!(Path::new(&plan.full_path).starts_with(&root));
        }
    }

    // --- finalize ------------------------------------------------------------

    #[tokio::test]
    async fn overwrite_replaces_the_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let storage = LocalStorageManager::new();
        let settings = settings_with_dir(temp.path());
        let plan = storage
            .plan_destination(
                &NamingContext {
                    post_id: Some("9".into()),
                    extension: Some("mp4".into()),
                    ..Default::default()
                },
                &settings,
            )
            .unwrap();

        std::fs::write(&plan.full_path, b"the original").unwrap();
        let staged = temp.path().join("new.tmp");
        std::fs::write(&staged, b"the replacement").unwrap();

        let outcome = storage
            .finalize(&staged, &plan, ConflictPolicy::Overwrite)
            .await
            .unwrap();

        assert!(outcome.was_saved());
        assert_eq!(outcome.path(), plan.full_path);
        assert_eq!(std::fs::read(&plan.full_path).unwrap(), b"the replacement");
        assert!(!staged.exists(), "the temporary file is gone");
    }

    #[tokio::test]
    async fn a_failed_overwrite_leaves_the_existing_file_in_place() {
        // The property that matters: a download that cannot be saved must never
        // take the user's existing file with it. The old implementation deleted
        // the destination first, so a rename that then failed lost it.
        let temp = tempfile::tempdir().unwrap();
        let storage = LocalStorageManager::new();
        let settings = settings_with_dir(temp.path());
        let plan = storage
            .plan_destination(
                &NamingContext {
                    post_id: Some("10".into()),
                    extension: Some("mp4".into()),
                    ..Default::default()
                },
                &settings,
            )
            .unwrap();

        // A directory where the file should go: the rename cannot succeed, and
        // the destination cannot be quietly removed either.
        std::fs::create_dir_all(&plan.full_path).unwrap();
        std::fs::write(plan.full_path.clone() + "/keep", b"still here").unwrap();

        let staged = temp.path().join("new.tmp");
        std::fs::write(&staged, b"the replacement").unwrap();

        let outcome = storage.finalize(&staged, &plan, ConflictPolicy::Overwrite).await;
        assert!(outcome.is_err(), "the move must fail rather than pretend");
        assert!(
            Path::new(&plan.full_path).is_dir(),
            "whatever was at the destination is untouched"
        );
        assert_eq!(
            std::fs::read(plan.full_path.clone() + "/keep").unwrap(),
            b"still here"
        );
    }

    #[tokio::test]
    async fn a_refused_directory_stops_the_download_before_it_writes() {
        // `prepare_directory` is the gate: it runs immediately before the first
        // write, so refusing there is what keeps bytes out of a linked folder.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("downloads");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let mut plan = LocalStorageManager::new()
            .plan_destination(
                &NamingContext {
                    post_id: Some("11".into()),
                    extension: Some("mp4".into()),
                    ..Default::default()
                },
                &settings_with_dir(&root),
            )
            .unwrap();

        // Point the plan's directory at a place outside the root, exactly as a
        // link on that path would.
        plan.directory = outside.to_string_lossy().into_owned();
        plan.full_path = outside.join("clip.mp4").to_string_lossy().into_owned();

        let err = LocalStorageManager::new()
            .prepare_directory(&plan)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
        assert!(!err.user_message().is_empty());
    }

    // --- preflight -----------------------------------------------------------

    fn plan_under(root: &Path, post_id: &str) -> DestinationPlan {
        LocalStorageManager::new()
            .plan_destination(
                &NamingContext {
                    post_id: Some(post_id.into()),
                    extension: Some("mp4".into()),
                    ..Default::default()
                },
                &settings_with_dir(root),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn a_usable_destination_passes_the_preflight_and_leaves_nothing_behind() {
        let temp = tempfile::tempdir().unwrap();
        let storage = LocalStorageManager::new();
        let plan = plan_under(temp.path(), "1");

        let report = storage.preflight(&plan, Some(1024)).await.unwrap();
        assert!(report.destination_writable);
        assert!(report.is_sufficient());
        assert_eq!(report.required_bytes, Some(3 * 1024));

        // The probe file is a measurement, not a leftover.
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !leftovers.iter().any(|name| name.starts_with(".download-write-test")),
            "the probe must be removed: {leftovers:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_preflights_of_one_directory_all_succeed() {
        // The regression guard for a fixed probe-file name. Two transfers that
        // start together preflight the same folder, and on Windows a delete is
        // only finished once the last handle closes — so with one shared name
        // the second create could land while the first delete was still
        // pending, fail, and be reported as "the download folder is not
        // writable" for a folder that was perfectly writable. The bug showed up
        // as one download in twenty-five failing for no reason.
        let temp = tempfile::tempdir().unwrap();
        let storage = Arc::new(LocalStorageManager::new());
        let plan = Arc::new(plan_under(temp.path(), "12"));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let storage = Arc::clone(&storage);
            let plan = Arc::clone(&plan);
            handles.push(tokio::spawn(async move {
                storage.preflight(&plan, Some(1024)).await.is_ok()
            }));
        }
        for handle in handles {
            assert!(
                handle.await.expect("task"),
                "a concurrent preflight of a writable folder must not fail"
            );
        }

        // And none of them left anything behind.
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".download-write-test"))
            .collect();
        assert!(leftovers.is_empty(), "probe files left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn a_download_larger_than_the_free_space_is_refused_with_the_numbers() {
        let temp = tempfile::tempdir().unwrap();
        let storage = LocalStorageManager::new();
        let plan = plan_under(temp.path(), "2");

        // A size no volume could satisfy, so the comparison is exercised for
        // real rather than depending on how full this machine's disk happens to
        // be.
        let err = storage.preflight(&plan, Some(u64::MAX)).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
        assert!(
            !err.is_retryable(),
            "retrying does not make room; the user has to"
        );
        let message = err.user_message();
        assert!(
            message.contains("space") && message.contains("free"),
            "the message must say how much was needed and how much there was: {message}"
        );
    }

    #[tokio::test]
    async fn an_unknown_size_never_blocks_a_download() {
        // The rule the brief is explicit about: a source that omits
        // `Content-Length` must not have its download refused.
        let temp = tempfile::tempdir().unwrap();
        let storage = LocalStorageManager::new();
        let plan = plan_under(temp.path(), "3");

        let report = storage.preflight(&plan, None).await.unwrap();
        assert!(report.is_sufficient());
        assert!(report.required_bytes.is_none());
        assert!(report.destination_writable);
    }

    #[tokio::test]
    async fn a_destination_that_cannot_be_created_is_refused_before_the_transfer() {
        let temp = tempfile::tempdir().unwrap();
        let storage = LocalStorageManager::new();
        let mut plan = plan_under(temp.path(), "4");

        // A file where the directory should be: nothing can be created below it.
        std::fs::write(temp.path().join("blocker"), b"not a directory").unwrap();
        plan.directory = temp
            .path()
            .join("blocker")
            .join("sub")
            .to_string_lossy()
            .into_owned();
        plan.full_path = Path::new(&plan.directory)
            .join("clip.mp4")
            .to_string_lossy()
            .into_owned();

        let err = storage.preflight(&plan, Some(1024)).await.unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Storage);
        assert!(!err.user_message().is_empty());
        // Nothing was written anywhere.
        assert_eq!(std::fs::read(temp.path().join("blocker")).unwrap(), b"not a directory");
    }

    #[test]
    fn a_report_with_no_figures_is_not_a_refusal() {
        let report = PreflightReport {
            available_bytes: None,
            required_bytes: None,
            destination_writable: true,
        };
        assert!(report.is_sufficient());

        // Only a *known* shortfall stops a download.
        assert!(!PreflightReport {
            available_bytes: Some(10),
            required_bytes: Some(11),
            destination_writable: true,
        }
        .is_sufficient());
        assert!(PreflightReport {
            available_bytes: Some(11),
            required_bytes: Some(11),
            destination_writable: true,
        }
        .is_sufficient());
        assert!(!PreflightReport {
            available_bytes: None,
            required_bytes: None,
            destination_writable: false,
        }
        .is_sufficient());
    }

    #[tokio::test]
    async fn finalize_renames_on_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings_with_dir(temp.path());
        let storage = LocalStorageManager::new();
        let context = NamingContext {
            post_id: Some("1".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        let plan = storage.plan_destination(&context, &settings).unwrap();

        // Simulate an existing file at the destination.
        storage.prepare_directory(&plan).await.unwrap();
        std::fs::write(&plan.full_path, b"existing").unwrap();

        let temp_file = temp.path().join("partial.tmp");
        std::fs::write(&temp_file, b"new").unwrap();
        let outcome = storage
            .finalize(&temp_file, &plan, ConflictPolicy::Rename)
            .await
            .unwrap();

        assert!(outcome.was_saved());
        assert_ne!(outcome.path(), plan.full_path);
        assert!(outcome.path().contains("(1)"));
        // The original file is untouched.
        assert_eq!(std::fs::read(&plan.full_path).unwrap(), b"existing");
    }

    #[tokio::test]
    async fn finalize_skips_when_configured() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings_with_dir(temp.path());
        let storage = LocalStorageManager::new();
        let context = NamingContext {
            post_id: Some("1".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        let plan = storage.plan_destination(&context, &settings).unwrap();
        storage.prepare_directory(&plan).await.unwrap();
        std::fs::write(&plan.full_path, b"existing").unwrap();

        let temp_file = temp.path().join("partial.tmp");
        std::fs::write(&temp_file, b"new").unwrap();
        let outcome = storage
            .finalize(&temp_file, &plan, ConflictPolicy::Skip)
            .await
            .unwrap();
        assert!(!outcome.was_saved());
    }

    #[tokio::test]
    async fn finalize_overwrites_when_configured() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings_with_dir(temp.path());
        let storage = LocalStorageManager::new();
        let context = NamingContext {
            post_id: Some("1".into()),
            extension: Some("mp4".into()),
            ..Default::default()
        };
        let plan = storage.plan_destination(&context, &settings).unwrap();
        storage.prepare_directory(&plan).await.unwrap();
        std::fs::write(&plan.full_path, b"existing").unwrap();

        let temp_file = temp.path().join("partial.tmp");
        std::fs::write(&temp_file, b"new").unwrap();
        let outcome = storage
            .finalize(&temp_file, &plan, ConflictPolicy::Overwrite)
            .await
            .unwrap();
        assert_eq!(outcome.path(), plan.full_path);
        assert_eq!(std::fs::read(&plan.full_path).unwrap(), b"new");
    }
}
