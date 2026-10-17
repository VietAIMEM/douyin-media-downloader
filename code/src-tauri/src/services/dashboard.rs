//! Browsing what is actually in the download folder.
//!
//! The Library screen answers "what did the application download", from the
//! database. This answers "what is on this disk", from the filesystem — so a
//! file the user put there themselves appears, and a database row whose file is
//! gone does not masquerade as a playable one.
//!
//! # What it will not do
//!
//! It reads. It never creates, moves, renames or deletes anything. Every path it
//! is given is resolved and checked against the configured download root before
//! it is opened, and a directory reached through a junction that leads outside
//! that root is refused rather than followed — the same containment rule the
//! storage layer already applies when it writes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::database::repositories::{downloads as downloads_repo, posts as posts_repo};
use crate::errors::{AppError, AppResult};
use crate::models::{Id, Timestamp};
use crate::storage::filenames::resolved_is_within;

use super::AppCore;

/// How far below the download root a browser may go.
///
/// Deep enough for the layouts this application produces — `Creator/Videos` is
/// two — and shallow enough that a directory which links back to itself cannot
/// turn a listing into a walk of the whole disk.
const MAX_DEPTH: usize = 4;

/// The most entries one folder listing will return.
///
/// A folder with more than this is shown truncated rather than made to hang the
/// screen; the count reported alongside is the true one, so the truncation is
/// visible rather than silent.
const MAX_ENTRIES: usize = 2000;

/// How many previews may be extracted at once.
///
/// A folder of two hundred videos asks for two hundred thumbnails as it scrolls.
/// Without a bound that is two hundred copies of ffmpeg, which is how a
/// responsive window becomes an unresponsive machine. Two keeps a fast disk busy
/// and the window ahead of it.
static EXTRACTIONS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// What kind of media a file is, judged by its extension.
///
/// The extension is the only thing a directory listing gives, and for the
/// formats this application writes it is reliable. A file it does not recognise
/// is `Other` rather than being forced into a kind it may not be — the screen
/// says so and offers to open it externally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Other,
}

impl MediaKind {
    pub(crate) fn of(path: &Path) -> Self {
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match extension.as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "avif" => Self::Image,
            "mp4" | "webm" | "mov" | "m4v" | "mkv" => Self::Video,
            "mp3" | "m4a" | "aac" | "wav" | "flac" | "opus" | "ogg" => Self::Audio,
            _ => Self::Other,
        }
    }

    /// Whether this application can show it in its own window.
    fn is_playable(self) -> bool {
        matches!(self, Self::Image | Self::Video | Self::Audio)
    }
}

/// One folder inside the download root.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFolder {
    /// The folder's own name, exactly as the filesystem spells it, so a creator
    /// whose name is not ASCII reads correctly.
    pub name: String,
    /// Absolute path, for the next request.
    pub path: String,
    /// Media files in this folder and everything below it.
    pub media_count: usize,
    pub image_count: usize,
    pub video_count: usize,
    pub audio_count: usize,
    /// Total size of those files.
    pub total_bytes: u64,
    /// An image inside it, if there is one, to stand for the folder.
    ///
    /// Falls back to a video when the folder holds no picture, so a folder of
    /// downloaded videos has something to show rather than a grey square. The
    /// picture for it is made by [`crate::thumbnails`], which is also what takes
    /// the frame out of the video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_path: Option<String>,
    /// What `preview_path` is, so the card can mark a video as one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_kind: Option<MediaKind>,
    /// `ok`, `empty`, or `unreadable`.
    pub state: &'static str,
}

/// One media file on disk.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFile {
    pub name: String,
    pub path: String,
    pub kind: MediaKind,
    pub extension: String,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<Timestamp>,
    /// Whether the window can show this itself, or should offer to open it.
    pub playable: bool,
    /// What the download recorded this as — `image`, `video`, `carousel`, …
    ///
    /// The filesystem records no such thing: a carousel's pictures are ordinary
    /// image files and nothing on disk says which post they came from. When a
    /// record matches the path this says what the record said, so a carousel
    /// frame can be described as one; when nothing matches, the card falls back
    /// to what the extension shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    // --- what the application knows about it, when it knows anything ---------
    //
    // Matched by the path recorded when the file was saved, never by its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_item_id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at: Option<Timestamp>,
}

/// The top level: the folders inside the download root.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderListing {
    pub root: String,
    pub folders: Vec<MediaFolder>,
    /// Files sitting directly in the root, which the folders below do not
    /// account for.
    pub loose_media_count: usize,
    /// Why the root itself could not be read, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<&'static str>,
}

/// Inside one folder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderContents {
    pub path: String,
    /// The folder to go up to, or nothing at the root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub folders: Vec<MediaFolder>,
    pub files: Vec<MediaFile>,
    /// True when the folder holds more media than one listing returns.
    pub truncated: bool,
}

/// The download root, resolved and checked.
fn configured_root(core: &AppCore) -> AppResult<PathBuf> {
    let configured = core.settings().current().download_directory;
    let root = PathBuf::from(&configured);
    let metadata = std::fs::metadata(&root).map_err(|err| {
        AppError::unavailable("the download folder could not be read")
            .with_user_message("The download folder could not be opened.")
            .with_technical_detail(format!("{}: {:?}", root.display(), err.kind()))
    })?;
    if !metadata.is_dir() {
        return Err(AppError::unavailable("the download folder is not a folder")
            .with_user_message("The configured download location is not a folder."));
    }
    Ok(root)
}

/// A path the caller named, checked to be inside the root.
///
/// Resolved before it is compared, so a path that reaches the root through a
/// junction leading somewhere else is refused rather than followed.
fn checked_path(root: &Path, requested: &str) -> AppResult<PathBuf> {
    let path = PathBuf::from(requested);
    if !resolved_is_within(root, &path) {
        return Err(AppError::validation("folder")
            .with_user_message("That folder is not inside the download folder.")
            .with_technical_detail(resolved_shape(root, &path)));
    }
    Ok(path)
}

/// A path-free description of a refusal, for the log.
fn resolved_shape(root: &Path, candidate: &Path) -> String {
    let depth = candidate.components().count().saturating_sub(root.components().count());
    format!("depth_below_root={depth}")
}

/// How deep below `root` a path sits.
fn depth_below(root: &Path, path: &Path) -> usize {
    path.components().count().saturating_sub(root.components().count())
}

/// Everything this application can show, counted for one folder and its
/// descendants.
#[derive(Default)]
struct Tally {
    media: usize,
    images: usize,
    videos: usize,
    audio: usize,
    bytes: u64,
    preview: Option<String>,
    /// The first video seen, kept separately so a picture always wins and a
    /// folder with no picture can still show something.
    video_preview: Option<String>,
    truncated: bool,
    /// Something in this folder, or below it, could not be listed.
    ///
    /// Kept separate from the counts because the two answer different questions.
    /// Zero media is a fact; zero media *and* a failed read is not a fact at all,
    /// and reporting it as "nothing here" would tell the user a folder is empty
    /// when the truth is that nobody looked inside it.
    unreadable: bool,
}

impl Tally {
    fn add_file(&mut self, path: &Path, kind: MediaKind, size: u64) {
        if !kind.is_playable() {
            return;
        }
        self.media += 1;
        self.bytes += size;
        match kind {
            MediaKind::Image => {
                self.images += 1;
                if self.preview.is_none() {
                    self.preview = Some(path.to_string_lossy().into_owned());
                }
            }
            MediaKind::Video => {
                self.videos += 1;
                if self.video_preview.is_none() {
                    self.video_preview = Some(path.to_string_lossy().into_owned());
                }
            }
            MediaKind::Audio => self.audio += 1,
            MediaKind::Other => {}
        }
    }
}

/// Walk a folder and its descendants, staying inside the root.
fn walk_folder(
    root: &Path,
    folder: &Path,
    depth: usize,
    tally: &mut Tally,
    seen: &mut HashSet<PathBuf>,
) {
    if tally.media >= MAX_ENTRIES {
        tally.truncated = true;
        return;
    }
    if depth > MAX_DEPTH {
        return;
    }
    // A directory whose resolved form has been visited already is a link back
    // into itself. Following it again would count the same files twice.
    let identity = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    if !seen.insert(identity) {
        return;
    }

    let Ok(entries) = std::fs::read_dir(folder) else {
        // Not reachable, not readable, or gone between the metadata call and
        // this one. Whatever the cause, nothing here was counted — and saying
        // "nothing here" would be a claim this cannot support.
        tally.unreadable = true;
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        // Refused before it is opened: a junction out of the root is not this
        // application's to walk.
        if !resolved_is_within(root, &path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            walk_folder(root, &path, depth + 1, tally, seen);
        } else if metadata.is_file() {
            tally.add_file(&path, MediaKind::of(&path), metadata.len());
        }
    }
}

/// The folders directly inside `folder`, each with a tally of what it holds.
fn folders_in(root: &Path, folder: &Path, depth: usize) -> Vec<MediaFolder> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !resolved_is_within(root, &path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }

        let mut tally = Tally::default();
        let mut seen = HashSet::new();
        walk_folder(root, &path, depth + 1, &mut tally, &mut seen);

        folders.push(MediaFolder {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            media_count: tally.media,
            image_count: tally.images,
            video_count: tally.videos,
            audio_count: tally.audio,
            total_bytes: tally.bytes,
            preview_path: tally.preview.clone().or(tally.video_preview.clone()),
            preview_kind: match (&tally.preview, &tally.video_preview) {
                (Some(_), _) => Some(MediaKind::Image),
                (None, Some(_)) => Some(MediaKind::Video),
                (None, None) => None,
            },
            // `empty` is a claim that the folder was looked into and holds
            // nothing. A failed read cannot make that claim, so it gets its own
            // answer rather than being bundled with it.
            state: if tally.unreadable {
                "unreadable"
            } else if tally.media == 0 {
                "empty"
            } else {
                "ok"
            },
        });
    }
    // A stable order, by the name the user sees.
    // Cached because the key allocates: comparing a thousand names should not
    // build a thousand lowercase strings per comparison.
    folders.sort_by_cached_key(|folder| folder.name.to_lowercase());
    folders
}

impl AppCore {
    /// The folders inside the download root.
    pub fn browse_media_folders(&self) -> AppResult<FolderListing> {
        let root = configured_root(self)?;
        let folders = folders_in(&root, &root, 0);

        // Media sitting loose in the root itself, which no folder accounts for.
        let mut loose = Tally::default();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !resolved_is_within(&root, &path) {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else { continue };
                if metadata.is_file() {
                    loose.add_file(&path, MediaKind::of(&path), metadata.len());
                }
            }
        }
        Ok(FolderListing {
            root: root.to_string_lossy().into_owned(),
            folders,
            loose_media_count: loose.media,
            problem: None,
        })
    }

    /// What is inside one folder: the folders below it, and the media in it.
    pub fn browse_media_folder(&self, requested: &str) -> AppResult<FolderContents> {
        let root = configured_root(self)?;
        let folder = checked_path(&root, requested)?;
        let metadata = std::fs::metadata(&folder).map_err(|err| {
            AppError::not_found("folder", requested)
                .with_user_message("That folder is not there any more.")
                .with_technical_detail(format!("{:?}", err.kind()))
        })?;
        if !metadata.is_dir() {
            return Err(AppError::validation("folder")
                .with_user_message("That is a file, not a folder."));
        }

        let depth = depth_below(&root, &folder);
        let folders = folders_in(&root, &folder, depth);

        // The files in this folder only — the folders above cover what is below.
        //
        // A folder that cannot be listed is an error, not an empty folder.
        // Returning nothing here would put "This folder is empty" on the screen
        // for a folder nobody was able to look inside, which is the one thing
        // this screen must never do: it exists to say what is really there.
        let entries = std::fs::read_dir(&folder).map_err(|err| {
            AppError::storage("folder")
                .with_user_message("That folder could not be read, so what is inside it is unknown.")
                .with_technical_detail(format!("{:?}", err.kind()))
        })?;

        let mut paths: Vec<PathBuf> = Vec::new();
        let mut truncated = false;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !resolved_is_within(&root, &path) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else { continue };
            if !metadata.is_file() {
                continue;
            }
            if paths.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            paths.push(path);
        }
        paths.sort_by_cached_key(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
        });

        let files = self.describe_media_files(&paths)?;

        Ok(FolderContents {
            path: folder.to_string_lossy().into_owned(),
            parent: if depth == 0 {
                None
            } else {
                folder.parent().map(|parent| parent.to_string_lossy().into_owned())
            },
            folders,
            files,
            truncated,
        })
    }

    /// A small preview picture of one file in the download folder.
    ///
    /// Returns the path of a JPEG in this application's own cache, or `None`
    /// when there is no preview to be had — no FFmpeg, a format that will not
    /// decode, audio, anything that is not a picture. `None` is an ordinary
    /// answer, not a failure, and the card draws a placeholder for it.
    ///
    /// Everything before the extraction is a refusal, and each refusal is
    /// deliberate: a path outside the configured folder, a path that reaches
    /// outside it through a link, a file that is no longer there. The same
    /// containment rule as opening a file, applied before a byte is read.
    /// Extraction is blocking and starts a process, so it runs on a blocking
    /// thread — and only a couple at a time, so a folder of two hundred videos
    /// does not start two hundred copies of ffmpeg.
    pub async fn thumbnail_for(&self, requested: &str) -> AppResult<Option<String>> {
        let root = configured_root(self)?;
        let candidate = Path::new(requested);

        let inside = candidate.is_file()
            && std::fs::canonicalize(candidate)
                .ok()
                .zip(std::fs::canonicalize(&root).ok())
                .map(|(real, real_root)| crate::storage::filenames::is_within(&real_root, &real))
                .unwrap_or(false);
        if !inside {
            return Err(AppError::validation("path")
                .with_user_message("That file is not inside the download folder.")
                .with_technical_detail("outside-root"));
        }

        let source = match MediaKind::of(candidate) {
            MediaKind::Image => crate::thumbnails::Source::Image,
            MediaKind::Video => crate::thumbnails::Source::Video,
            // Audio and everything else has no picture, and making one up would
            // be a claim about the file that is not true.
            _ => return Ok(None),
        };

        let thumbnailer = self.thumbnails.clone();
        let owner = candidate.to_path_buf();
        let Ok(_permit) = EXTRACTIONS.acquire().await else {
            return Ok(None);
        };
        let made = tokio::task::spawn_blocking(move || thumbnailer.ensure(&owner, source))
            .await
            .unwrap_or(None);

        Ok(made.map(|thumbnail| thumbnail.to_string_lossy().into_owned()))
    }

    /// Describe each file, attaching what the database knows about it.
    ///
    /// One lookup for all of them rather than one each: a folder of two hundred
    /// pictures should cost one query, not two hundred.
    fn describe_media_files(&self, paths: &[PathBuf]) -> AppResult<Vec<MediaFile>> {
        // The repository hands the map back already keyed by the comparison
        // form, so the two spellings of one path meet here without this having
        // to know how they were folded.
        let (by_path, posts) = self.database().with_conn(|conn| {
            let by_path = downloads_repo::by_destination_paths(conn, paths)?;
            let mut posts = std::collections::HashMap::new();
            for task in by_path.values() {
                let Some(post_id) = task.post_id.as_deref() else {
                    continue;
                };
                if posts.contains_key(post_id) {
                    continue;
                }
                if let Some(post) = posts_repo::get(conn, post_id)? {
                    posts.insert(post_id.to_string(), post);
                }
            }
            Ok((by_path, posts))
        })?;

        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let Ok(metadata) = std::fs::metadata(path) else {
                continue;
            };
            let recorded_path = path.to_string_lossy().into_owned();
            let kind = MediaKind::of(path);
            let task = by_path.get(&crate::storage::filenames::comparison_key(path));
            let post = task
                .and_then(|task| task.post_id.as_deref())
                .and_then(|post_id| posts.get(post_id));

            files.push(MediaFile {
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: recorded_path,
                kind,
                extension: path
                    .extension()
                    .map(|value| value.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default(),
                size_bytes: metadata.len(),
                modified: metadata.modified().ok().map(Timestamp::from),
                playable: kind.is_playable(),
                media_type: task.map(|task| task.media_type.as_str().to_owned()),
                task_id: task.map(|task| task.id.clone()),
                post_id: task.and_then(|task| task.post_id.clone()),
                media_item_id: task.and_then(|task| task.media_item_id.clone()),
                caption: post.and_then(|post| post.caption.clone()),
                creator: post.and_then(|post| post.creator_handle.clone()),
                source_url: task.map(|task| task.source_url.clone()),
                downloaded_at: task.and_then(|task| task.completed_at),
            });
        }
        Ok(files)
    }
}
