//! Library workflow.
//!
//! The Library answers one question — *what do I have locally, and what is it
//! doing?* — by reconciling five things the application already stores: a post,
//! the creator who wrote it, the media resources it holds, the newest download
//! task for it, and whether the file that task produced is still on disk.
//!
//! # The five things are not interchangeable
//!
//! A post can exist having never been resolved (a profile listing gives no
//! media). A media resource can be known and never downloaded. A download task
//! can be queued, running, failed, or finished. A finished task can point at a
//! file that somebody deleted. And history is a record of what happened, not a
//! claim about what is on disk now.
//!
//! [`reconcile_status`] is the single place those are folded together, and it is
//! shared with the Profile Downloader so the two screens cannot disagree about
//! whether a post is downloaded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::database::repositories::library as library_repo;
use crate::errors::{AppError, AppResult};
use crate::models::{
    ContentStatus, DownloadState, LibraryCounts, LibraryEntry, LibraryFilter, LibraryPage,
    LibrarySort, LibraryView,
};
use crate::storage::presence::{self, FileState, Presence};

use super::AppCore;

/// What answering one query cost, in work rather than in milliseconds.
///
/// A test seam, not a metric the product reads. Latency is the thing a user
/// feels, but a latency assertion in a test suite is a coin toss on a busy
/// machine; the number of times the library was read and the number of times the
/// filesystem was asked about a file are exact, reproduce on any machine, and
/// are what the cost is actually made of. A test asserting these cannot pass by
/// accident and cannot fail because something else on the computer was busy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Work {
    /// How many times the matching rows were read and turned into entries.
    pub materialisations: usize,
    /// How many times the filesystem was actually asked about a file.
    pub inspections: usize,
}

thread_local! {
    /// Counted per thread, so tests running side by side cannot see each other's
    /// numbers. A query is answered on the thread that asked for it.
    static WORK: std::cell::Cell<Work> = const { std::cell::Cell::new(Work {
        materialisations: 0,
        inspections: 0,
    }) };
}

/// What the query answered on this thread has cost since the last reset.
pub fn work_done() -> Work {
    WORK.with(std::cell::Cell::get)
}

/// Start counting again, for the next thing a test wants to measure.
pub fn reset_work() {
    WORK.with(|work| work.set(Work::default()));
}

fn counted(work: impl FnOnce(&mut Work)) {
    WORK.with(|cell| {
        let mut current = cell.get();
        work(&mut current);
        cell.set(current);
    });
}

/// Rows a caller gets when it does not say.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Upper bound on one page, so a caller cannot ask for the whole library at once.
pub const MAX_PAGE_SIZE: usize = 500;

/// A query against the Library.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryQuery {
    /// Which slice to show. Defaults to everything.
    #[serde(default = "default_view")]
    pub view: LibraryView,
    /// Free text across creator, caption, post id, URL, and local path.
    #[serde(default)]
    pub search: Option<String>,
    /// Narrowing beyond what the view already implies.
    #[serde(default)]
    pub filter: LibraryFilter,
    #[serde(default)]
    pub sort: LibrarySort,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub limit: Option<usize>,
}

fn default_view() -> LibraryView {
    LibraryView::All
}

impl Default for LibraryQuery {
    fn default() -> Self {
        Self {
            view: LibraryView::All,
            search: None,
            filter: LibraryFilter::default(),
            sort: LibrarySort::default(),
            offset: 0,
            limit: None,
        }
    }
}

/// Which records an export covers.
///
/// Stated explicitly because "export" is ambiguous in a paginated screen, and
/// silently exporting only the page on screen is the kind of thing a user only
/// discovers after opening the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportScope {
    /// Everything the current view and filters match, ignoring pagination.
    Matching,
}

/// The format of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Json,
    Csv,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Csv => "csv",
        }
    }
}

/// What an export produced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    /// Where the file was written. Absolute.
    pub path: String,
    pub format: ExportFormat,
    /// How many records it holds.
    pub records: usize,
    pub bytes: usize,
    /// Which records it covers, echoed back so the screen can state it.
    pub scope: ExportScope,
}

/// The rule that turns stored facts into what the Library shows.
///
/// Shared with the Profile Downloader, so the two screens cannot disagree.
///
/// The order matters and encodes these decisions:
///
/// 1. **What the source says about availability outranks anything local.** A
///    post the source reports as unavailable is not made available by having
///    been downloaded once.
/// 2. **A finished download counts as downloaded only while its file is on
///    disk.** A row is not a file, and a post whose file was deleted reads as
///    `Ready` — something that can be downloaded again — rather than as a
///    download that exists.
/// 3. **Not being able to check is not the same as knowing it is gone.** A file
///    whose presence could not be established — an unreadable directory, a drive
///    that is not attached, a path stored under a download folder the user has
///    since changed — stays `Downloaded`, because there is a completed record
///    and no evidence against it, and carries a note saying it could not be
///    checked. Treating it as absent would empty a user's "Downloaded" tab
///    because they unplugged a disk, which is the alarming reading of a
///    condition that says nothing about the files.
/// 4. **A cancelled attempt says nothing about the content**, so it leaves the
///    status the post already had.
///
/// The asymmetry in (2) and (3) is deliberate. Absence that has been established
/// is a fact about the file; an answer that could not be obtained is a fact
/// about the check. Only the first justifies moving the post out of
/// "Downloaded".
pub fn reconcile_status(
    post_status: ContentStatus,
    latest_download: Option<DownloadState>,
    presence: Presence,
) -> (ContentStatus, Option<String>) {
    if matches!(
        post_status,
        ContentStatus::Unavailable | ContentStatus::Private | ContentStatus::Unsupported
    ) {
        return (post_status, None);
    }

    let Some(state) = latest_download else {
        return (post_status, None);
    };

    match state {
        DownloadState::Queued | DownloadState::Downloading | DownloadState::Paused => {
            (ContentStatus::Downloading, None)
        }
        DownloadState::Completed => match presence {
            Presence::Present => (ContentStatus::Downloaded, None),
            Presence::Missing => (
                ContentStatus::Ready,
                Some(
                    "The downloaded file is no longer on disk, so this can be downloaded \
                     again."
                        .to_string(),
                ),
            ),
            // Deliberately *not* the wording above. This says what was and was
            // not done; it does not claim anything about the file.
            Presence::Unavailable => (
                ContentStatus::Downloaded,
                Some(
                    "Whether the downloaded file is still on disk could not be checked, so \
                     nothing has been concluded about it."
                        .to_string(),
                ),
            ),
        },
        DownloadState::Failed => (
            ContentStatus::Failed,
            Some("The last download attempt failed; it can be retried.".to_string()),
        ),
        DownloadState::Cancelled => (
            post_status,
            Some("The last download attempt was cancelled.".to_string()),
        ),
    }
}

impl AppCore {
    /// Run a Library query.
    ///
    /// One read of the database, one pass of presence inspection, and every
    /// number on the screen derived from that single materialised set. What this
    /// used to be is worth recording, because the shape was the cost: the
    /// matching rows were read once for the page and then once more *for each of
    /// the seven view counts*, each pass repeating the database reads and asking
    /// the filesystem about every file again — eight materialisations and, on a
    /// two-thousand-post library, twelve thousand filesystem inspections to
    /// render fifty rows. The counts were also computed over a *wider* set than
    /// the page: the view's own media types were merged into the scan before
    /// counting, and then narrowed straight back out by
    /// [`matches_derived`], which re-checks them in memory. Merging them only
    /// ever widened the SQL and was narrowed away again; the predicate that
    /// decides a count is the one that was already there.
    pub fn query_library(&self, query: LibraryQuery) -> AppResult<LibraryPage> {
        let limit = query
            .limit
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE);

        // The caller's own filters — search, creator, dates, statuses — but not
        // the view's media types. Each view applies its own types through
        // [`matches_derived`] over the shared set below, which is the same
        // predicate the widened scans used to be narrowed to, and applying it
        // here instead would either cost seven scans or change what the counts
        // cover.
        let mut filter = query.filter.clone();
        filter.search = query.search.clone().or(filter.search);

        let (scan, completed) = self.database().with_conn(|conn| {
            let scan = library_repo::scan(conn, &filter, query.sort)?;
            // Completed downloads for exactly the posts that came back, in one
            // read rather than one per row.
            let ids: Vec<String> = scan.rows.iter().map(|row| row.post.id.clone()).collect();
            let completed = library_repo::completed_downloads(conn, &ids)?;
            Ok((scan, completed))
        })?;

        counted(|work| work.materialisations += 1);
        let root = PathBuf::from(self.settings().current().download_directory);

        // One set of presence answers for the whole request. The page, its item
        // counts and the tab counts all read the same files; asking the
        // filesystem once per distinct path is not only faster, it is what keeps
        // them from disagreeing if the disk changed between two of the old
        // passes. Scoped to this request: a file that moves afterwards is seen
        // to have moved by the next one.
        let mut seen = PresenceCache::default();
        let present_items = present_item_counts(&root, &completed, &mut seen);

        let entries: Vec<LibraryEntry> = scan.rows
            .into_iter()
            .map(|row| {
                let answer = file_answer(&root, row.latest_destination.as_deref(), &mut seen);
                let (status, note) =
                    reconcile_status(row.post.status, row.latest_download_state, answer.presence);
                LibraryEntry {
                    creator_name: row.creator_name,
                    creator_handle: row.creator_handle,
                    status,
                    download_state: row.latest_download_state,
                    file_state: answer.state,
                    file_state_reason: answer.reason,
                    local_path: row.latest_destination,
                    downloaded_at: row.latest_completed_at,
                    error: note.or(row.latest_error),
                    media_item_count: row.media_item_count,
                    downloaded_item_count: present_items.get(&row.post.id).copied().unwrap_or(0),
                    duration_seconds: row.duration_seconds,
                    width: row.width,
                    height: row.height,
                    post: row.post,
                }
            })
            .collect();

        // What the view's own scan used to return: the rows that survive the
        // view's media types, before any status narrowing. `scanned` is shown
        // beside the total as the honest size of what was read, so it keeps that
        // meaning — derived here rather than read off a second scan.
        let view_types = query.view.media_types();
        let scanned = entries
            .iter()
            .filter(|entry| view_types.is_empty() || view_types.contains(&entry.post.media_type))
            .count();

        // The per-view counts are taken over the search and the caller's own
        // filters, *before* the status and derived filters narrow further — so a
        // tab shows how much that tab holds, not how much survives the tab you
        // are already on. Derived from the set above rather than from seven more
        // scans of it.
        let counts = self.count_views_from(&entries, &filter);

        // Derived filters: things SQLite cannot answer, because they depend on
        // the file system or on an aggregate across a post's media.
        let mut entries = entries;
        entries.retain(|entry| matches_derived(entry, &filter, query.view));

        let total = entries.len();
        let offset = query.offset.min(total);
        let entries = entries.into_iter().skip(offset).take(limit).collect();

        Ok(LibraryPage {
            entries,
            total,
            offset,
            limit,
            counts,
            scanned,
            // Established by the read itself — one candidate beyond the cap was
            // fetched and found to exist — not inferred from the count, which
            // cannot distinguish "exactly full" from "more behind it".
            truncated: scan.truncated,
        })
    }

    /// The distinct creators the Library holds, for the filter menu.
    pub fn library_creators(&self) -> AppResult<Vec<(String, String)>> {
        self.database().with_conn(|conn| library_repo::creators(conn))
    }

    /// Count every view over an already-materialised set.
    ///
    /// Free-standing because it owns no reads: the set was read once, upstream,
    /// and counting views over it again would be reading it again. Each view is
    /// matched with its own media-type and status predicate — the same one the
    /// widened scans used to be narrowed to — which is what makes the tabs'
    /// numbers add up to something a user can trust.
    fn count_views_from(
        &self,
        entries: &[LibraryEntry],
        filter: &LibraryFilter,
    ) -> LibraryCounts {
        let mut counts = LibraryCounts::default();
        for view in [
            LibraryView::All,
            LibraryView::Videos,
            LibraryView::Images,
            LibraryView::Audio,
            LibraryView::Downloaded,
            LibraryView::Failed,
            LibraryView::New,
        ] {
            // The view's own types, merged into a scoped filter exactly as the
            // caller used to, so `matches_derived` sees what it has always seen.
            let mut scoped = filter.clone();
            for media_type in view.media_types() {
                if !scoped.media_types.contains(media_type) {
                    scoped.media_types.push(*media_type);
                }
            }
            let matching = entries
                .iter()
                .filter(|entry| matches_derived(entry, &scoped, view))
                .count();
            counts.set(view, matching);
        }
        counts
    }

    /// Every entry a filter matches, with statuses reconciled.
    ///
    /// The whole matching set, not a page: used by the counts and by the export,
    /// both of which must describe the *match* rather than the page.
    ///
    /// Not the path the page takes — that reads the set once and derives
    /// everything from it. This stands alone because the export asks for the
    /// whole match in the caller's own order, which the page's shared set has
    /// already had its view applied to. It keeps the per-request presence cache,
    /// so a large export asks the filesystem once per distinct file rather than
    /// once per row.
    fn materialise(&self, filter: &LibraryFilter) -> AppResult<Vec<LibraryEntry>> {
        counted(|work| work.materialisations += 1);
        let (rows, completed) = self.database().with_conn(|conn| {
            let scan = library_repo::scan(conn, filter, LibrarySort::default())?;
            let ids: Vec<String> = scan.rows.iter().map(|row| row.post.id.clone()).collect();
            let completed = library_repo::completed_downloads(conn, &ids)?;
            Ok((scan.rows, completed))
        })?;

        // Which media resources of each post are on disk, counted once each.
        let root = PathBuf::from(self.settings().current().download_directory);
        let mut seen = PresenceCache::default();
        let present_items = present_item_counts(&root, &completed, &mut seen);

        Ok(rows
            .into_iter()
            .map(|row| {
                let answer = file_answer(&root, row.latest_destination.as_deref(), &mut seen);
                let (status, note) =
                    reconcile_status(row.post.status, row.latest_download_state, answer.presence);
                LibraryEntry {
                    creator_name: row.creator_name,
                    creator_handle: row.creator_handle,
                    status,
                    download_state: row.latest_download_state,
                    file_state: answer.state,
                    file_state_reason: answer.reason,
                    local_path: row.latest_destination,
                    downloaded_at: row.latest_completed_at,
                    error: note.or(row.latest_error),
                    media_item_count: row.media_item_count,
                    downloaded_item_count: present_items.get(&row.post.id).copied().unwrap_or(0),
                    duration_seconds: row.duration_seconds,
                    width: row.width,
                    height: row.height,
                    post: row.post,
                }
            })
            .collect())
    }

    /// Every entry the view and filter match, in the requested order.
    ///
    /// This is what an export writes: the match, not the page.
    pub fn library_entries(
        &self,
        query: &LibraryQuery,
    ) -> AppResult<Vec<LibraryEntry>> {
        let mut filter = query.filter.clone();
        filter.search = query.search.clone().or(filter.search);
        for media_type in query.view.media_types() {
            if !filter.media_types.contains(media_type) {
                filter.media_types.push(*media_type);
            }
        }

        let mut entries = self.materialise(&filter)?;
        entries.retain(|entry| matches_derived(entry, &filter, query.view));
        // The database ordered the rows by the raw columns; the derived filters
        // have not reordered anything, so the order is still the one asked for.
        Ok(entries)
    }

    /// Export the matching records to a file.
    ///
    /// The destination is inside the configured download directory, under an
    /// `exports` folder: that is the one place this application is allowed to
    /// write, it is already subject to the containment check, and it needs no
    /// new dependency for a file picker. The path is returned so the screen can
    /// show exactly where the file went.
    pub fn export_library(
        &self,
        query: LibraryQuery,
        format: ExportFormat,
        scope: ExportScope,
    ) -> AppResult<ExportResult> {
        let entries = match scope {
            ExportScope::Matching => self.library_entries(&query)?,
        };

        let body = match format {
            ExportFormat::Json => to_json(&entries)?,
            ExportFormat::Csv => to_csv(&entries),
        };

        let settings = self.settings().current();
        let root = std::path::PathBuf::from(&settings.download_directory);
        let directory = root.join("exports");
        std::fs::create_dir_all(&directory).map_err(|err| {
            AppError::storage("could not create the export folder")
                .with_user_message("The export folder could not be created.")
                .with_source(err)
        })?;

        // Checked even though the directory was just built from the settings:
        // the containment rule is enforced at every write, not only at the ones
        // that look risky.
        //
        // Canonicalized on both sides because the plain check is lexical — it
        // refuses `..` but cannot see a junction or symlink. Resolving both
        // paths is what makes "inside the download folder" mean where the bytes
        // actually land, rather than what the path is spelled like.
        if !crate::storage::filenames::is_within(&root, &directory) {
            return Err(AppError::storage("the export folder is outside the download folder")
                .with_user_message("The export folder is not inside the download folder."));
        }
        let real_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        let real_directory =
            std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone());
        if !crate::storage::filenames::is_within(&real_root, &real_directory) {
            return Err(AppError::storage(
                "the export folder resolves outside the download folder",
            )
            .with_user_message(
                "The export folder is a link to somewhere outside the download folder.",
            ));
        }

        let stamp = crate::models::common::now().format("%Y%m%d-%H%M%S");
        let path = directory.join(format!("library-{stamp}.{}", format.extension()));
        std::fs::write(&path, body.as_bytes()).map_err(|err| {
            AppError::storage("could not write the export")
                .with_user_message("The export file could not be written.")
                .with_source(err)
        })?;

        tracing::info!(
            target: "library",
            path = %path.display(),
            records = entries.len(),
            format = ?format,
            "library exported"
        );

        Ok(ExportResult {
            path: path.to_string_lossy().into_owned(),
            format,
            records: entries.len(),
            bytes: body.len(),
            scope,
        })
    }
}

/// Presence answers for one request, keyed by the recorded path.
///
/// A file does not move while one query is being answered, and asking the
/// filesystem the same question twice inside a single request buys nothing.
/// Worse than nothing: if something changed between the two reads, the page and
/// its own tab counts would disagree about the same file in the same response.
///
/// Scoped to a request rather than to the process, so a file that appears or
/// disappears between two requests is seen to have done so. A cache that
/// outlived the request would be a way of saying "present" about a file that was
/// deleted an hour ago.
type PresenceCache = std::collections::HashMap<String, FileState>;

/// What one entry should say about the file a download produced.
struct FileAnswer {
    /// What the rule decides on.
    presence: Presence,
    /// What the window shows, or `None` when no question was asked.
    state: Option<String>,
    /// Why the answer could not be established, when it could not.
    reason: Option<String>,
}

/// Inspect the file one download produced, once per request, and describe it
/// three ways.
///
/// Written once and used by the page, the counts and the item tally. They read
/// the same rows, and two copies of this would be two chances for the screen and
/// its own tab numbers to disagree about the same file.
///
/// `destination` is `None` when the row recorded no path at all — a post never
/// downloaded, or a task that never got as far as one. That is not one of the
/// three answers: the question was never asked, so nothing is reported.
fn file_answer(root: &Path, destination: Option<&str>, seen: &mut PresenceCache) -> FileAnswer {
    let Some(path) = destination else {
        return FileAnswer {
            presence: Presence::Unavailable,
            state: None,
            reason: None,
        };
    };

    let state = match seen.get(path) {
        Some(known) => known.clone(),
        None => {
            counted(|work| work.inspections += 1);
            let state = presence::inspect(root, Path::new(path));
            seen.insert(path.to_owned(), state.clone());
            state
        }
    };

    FileAnswer {
        presence: state.presence(),
        state: Some(state.presence().as_str().to_owned()),
        reason: state.reason().map(str::to_owned),
    }
}

/// Which media resources of each post are present locally.
///
/// Keyed by media resource, not by download: a resource that was fetched twice
/// — a retry, a second submission — is one item on disk, and counting the rows
/// would make a carousel with two of its three images look complete.
///
/// Only a file that was *confirmed* present counts. A resource whose file could
/// not be checked is not counted, so this stays a floor rather than a guess —
/// "2 of 3 items on disk" understates rather than claims.
fn present_item_counts(
    root: &Path,
    completed: &[(String, String, Option<String>)],
    seen: &mut PresenceCache,
) -> HashMap<String, usize> {
    let mut present: HashMap<String, std::collections::HashSet<&str>> = HashMap::new();
    for (post_id, media_item_id, destination) in completed {
        let here = file_answer(root, destination.as_deref(), seen).presence == Presence::Present;
        if here {
            present
                .entry(post_id.clone())
                .or_default()
                .insert(media_item_id.as_str());
        }
    }
    present
        .into_iter()
        .map(|(post_id, items)| (post_id, items.len()))
        .collect()
}
///
/// Duration and resolution are aggregates over a post's media, and the view's
/// status predicate depends on whether a file exists — neither is a column.
///
/// Whether an entry survives the filters SQLite could not answer.
///
/// A record whose duration or resolution is *unknown* is kept by a filter that
/// asks for a minimum: an unmeasured duration is not a zero-second one, and
/// dropping it would silently hide items the source simply did not describe.
fn matches_derived(entry: &LibraryEntry, filter: &LibraryFilter, view: LibraryView) -> bool {
    let view_statuses = view.statuses();
    if !view_statuses.is_empty() && !view_statuses.contains(&entry.status) {
        return false;
    }
    if !filter.statuses.is_empty() && !filter.statuses.contains(&entry.status) {
        return false;
    }

    if let Some(minimum) = filter.min_duration_seconds {
        if let Some(actual) = entry.duration_seconds {
            if actual < minimum {
                return false;
            }
        }
    }
    if let Some(minimum) = filter.min_width {
        if let Some(actual) = entry.width {
            if actual < minimum {
                return false;
            }
        }
    }

    // Re-checked here rather than trusted to the caller: the view's own media
    // types are applied by the caller when it builds the filter, but an entry
    // that reached this point through some other route must still obey the tab
    // it is being counted for.
    let view_types = view.media_types();
    if !view_types.is_empty() && !view_types.contains(&entry.post.media_type) {
        return false;
    }

    // "Downloaded" means the post is *there*, not that one of its resources is.
    // A carousel with one of three images on disk is a partial download, and
    // listing it under Downloaded would tell a user they have something they
    // have a third of.
    if view == LibraryView::Downloaded && entry.is_partially_downloaded() {
        return false;
    }

    true
}

/// One exported record.
///
/// A stable, documented schema: these names are what an external tool reads, so
/// they are written out rather than derived from the domain model's shape.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportRecord<'a> {
    creator_name: Option<&'a str>,
    creator_id: Option<&'a str>,
    creator_handle: Option<&'a str>,
    post_id: &'a str,
    post_url: &'a str,
    caption: Option<&'a str>,
    media_type: &'a str,
    published_at: Option<String>,
    status: &'a str,
    download_state: Option<&'a str>,
    local_path: Option<&'a str>,
    downloaded_at: Option<String>,
    file_present: bool,
    media_item_count: usize,
    downloaded_item_count: usize,
    duration_seconds: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    error: Option<&'a str>,
}

impl<'a> From<&'a LibraryEntry> for ExportRecord<'a> {
    fn from(entry: &'a LibraryEntry) -> Self {
        Self {
            creator_name: entry.creator_name.as_deref(),
            creator_id: entry.post.creator_id.as_deref(),
            creator_handle: entry
                .creator_handle
                .as_deref()
                .or(entry.post.creator_handle.as_deref()),
            post_id: &entry.post.platform_post_id,
            // The canonical form when the resolver settled on one, otherwise
            // whatever the post was reached by. Both are post pages; neither is
            // a media URL, which is what must never be exported.
            post_url: entry
                .post
                .canonical_url
                .as_deref()
                .unwrap_or(&entry.post.url),
            caption: entry.post.caption.as_deref(),
            media_type: entry.post.media_type.as_str(),
            published_at: entry.post.published_at.map(|at| at.to_rfc3339()),
            status: entry.status.as_str(),
            download_state: entry.download_state.map(|state| state.as_str()),
            local_path: entry.local_path.as_deref(),
            downloaded_at: entry.downloaded_at.map(|at| at.to_rfc3339()),
            // The export's own column keeps its name, its type and its meaning:
            // "the file is confirmed to be there". A file that could not be
            // checked is not one this can assert, so it stays `false` — the same
            // answer the column gave before this change, and the reason the
            // schema does not have to move for it.
            file_present: entry.file_state.as_deref() == Some("present"),
            media_item_count: entry.media_item_count,
            downloaded_item_count: entry.downloaded_item_count,
            duration_seconds: entry.duration_seconds,
            width: entry.width,
            height: entry.height,
            error: entry.error.as_deref(),
        }
    }
}

/// The column order of a CSV export, fixed so the file is stable to read.
const CSV_COLUMNS: &[&str] = &[
    "creatorName",
    "creatorId",
    "creatorHandle",
    "postId",
    "postUrl",
    "caption",
    "mediaType",
    "publishedAt",
    "status",
    "downloadState",
    "localPath",
    "downloadedAt",
    "filePresent",
    "mediaItemCount",
    "downloadedItemCount",
    "durationSeconds",
    "width",
    "height",
    "error",
];

/// Serialize the records as a JSON array.
fn to_json(entries: &[LibraryEntry]) -> AppResult<String> {
    let records: Vec<ExportRecord<'_>> = entries.iter().map(ExportRecord::from).collect();
    // `to_string_pretty` rather than a hand-built string: the serializer is what
    // guarantees valid UTF-8 JSON with correct escaping, including of captions
    // that contain quotes, backslashes, or newlines.
    serde_json::to_string_pretty(&records).map_err(|err| {
        AppError::unknown("could not serialize the library export")
            .with_user_message("The export could not be prepared.")
            .with_technical_detail(err.to_string())
    })
}

/// Serialize the records as CSV.
///
/// Every field goes through [`csv_field`]; nothing is concatenated by hand,
/// because a caption containing a comma, a quote, or a newline is ordinary
/// user data and must not be able to break the table.
fn to_csv(entries: &[LibraryEntry]) -> String {
    let mut out = String::new();
    out.push_str(&CSV_COLUMNS.join(","));
    // CRLF, which is what RFC 4180 specifies and what Excel expects.
    out.push_str("\r\n");

    for entry in entries {
        let record = ExportRecord::from(entry);
        // Written out in the order of `CSV_COLUMNS`, one cell per column. Built
        // as a list rather than indexed by position, so a column added to one is
        // a compile error in the other rather than a table that quietly has more
        // cells in a row than it has headers.
        let cells: Vec<String> = vec![
            csv_field(record.creator_name.unwrap_or("")),
            csv_field(record.creator_id.unwrap_or("")),
            csv_field(record.creator_handle.unwrap_or("")),
            csv_field(record.post_id),
            csv_field(record.post_url),
            csv_field(record.caption.unwrap_or("")),
            csv_field(record.media_type),
            csv_field(record.published_at.as_deref().unwrap_or("")),
            csv_field(record.status),
            csv_field(record.download_state.unwrap_or("")),
            csv_field(record.local_path.unwrap_or("")),
            csv_field(record.downloaded_at.as_deref().unwrap_or("")),
            csv_field(if record.file_present { "true" } else { "false" }),
            csv_number(record.media_item_count),
            csv_number(record.downloaded_item_count),
            csv_optional_number(record.duration_seconds),
            csv_optional_number(record.width),
            csv_optional_number(record.height),
            csv_field(record.error.unwrap_or("")),
        ];
        debug_assert_eq!(cells.len(), CSV_COLUMNS.len());
        out.push_str(&cells.join(","));
        out.push_str("\r\n");
    }

    out
}

/// One text cell, quoted only when it has to be.
///
/// Two things happen here, and the second changes the value on purpose:
///
/// 1. **RFC 4180 quoting.** A value containing a comma, a quote, or a line
///    break is wrapped in quotes, and its own quotes are doubled.
/// 2. **Formula-injection defence.** A value beginning with `=`, `+`, `-`, `@`,
///    a tab, or a carriage return is treated by Excel and Sheets as a formula.
///    A caption of `=HYPERLINK(...)` is a caption, not an instruction, so such a
///    value is prefixed with a single apostrophe — which is how spreadsheets
///    mark text. The apostrophe is *not* part of the data, but it is part of the
///    file, and that is documented rather than hidden.
fn csv_field(value: &str) -> String {
    let dangerous = value
        .chars()
        .next()
        .is_some_and(|first| matches!(first, '=' | '+' | '-' | '@' | '\t' | '\r'));
    let value = if dangerous {
        format!("'{value}")
    } else {
        value.to_string()
    };

    let needs_quotes = value.contains([',', '"', '\n', '\r']);
    if needs_quotes {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

/// One numeric cell.
///
/// Numbers are never prefixed and never quoted: a count or a duration written as
/// `"12"` would stop being a number in the tools the export is for, and no
/// non-negative value can be a formula.
fn csv_number(value: impl std::fmt::Display) -> String {
    value.to_string()
}

/// A numeric cell that may be unknown.
///
/// An unknown value is an empty cell rather than a zero: a post whose duration
/// nobody measured did not last no seconds.
fn csv_optional_number<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map(|number| number.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{MediaType, Platform, Post};

    fn entry(caption: Option<&str>, status: ContentStatus, media_type: MediaType) -> LibraryEntry {
        let mut post = Post::new(Platform::Douyin, "7123456789012345678", media_type, "https://www.douyin.com/video/7123456789012345678");
        post.caption = caption.map(str::to_string);
        LibraryEntry {
            post,
            creator_name: Some("Alice".into()),
            creator_handle: Some("@alice".into()),
            status,
            download_state: None,
            file_state: None,
            file_state_reason: None,
            local_path: None,
            downloaded_at: None,
            error: None,
            media_item_count: 1,
            downloaded_item_count: 0,
            duration_seconds: None,
            width: None,
            height: None,
        }
    }

    // --- the status rule -----------------------------------------------------

    #[test]
    fn a_post_never_downloaded_stays_as_discovered() {
        let (status, note) = reconcile_status(ContentStatus::New, None, Presence::Missing);
        assert_eq!(status, ContentStatus::New);
        assert!(note.is_none());
    }

    #[test]
    fn a_download_only_counts_while_its_file_is_there() {
        let (status, _) = reconcile_status(
            ContentStatus::New,
            Some(DownloadState::Completed),
            Presence::Present,
        );
        assert_eq!(status, ContentStatus::Downloaded);

        // The row exists, the file does not: that is not a download.
        let (status, note) = reconcile_status(
            ContentStatus::New,
            Some(DownloadState::Completed),
            Presence::Missing,
        );
        assert_eq!(status, ContentStatus::Ready);
        assert!(note.unwrap().contains("no longer on disk"));
    }

    #[test]
    fn a_file_that_could_not_be_checked_is_not_reported_as_deleted() {
        // The distinction this exists for. An unreadable directory, a drive that
        // is not attached, a path from a folder the user has since changed —
        // none of them is evidence that a file is gone, and saying so would
        // empty a user's Downloaded tab because they unplugged a disk.
        let (status, note) = reconcile_status(
            ContentStatus::New,
            Some(DownloadState::Completed),
            Presence::Unavailable,
        );

        assert_eq!(
            status,
            ContentStatus::Downloaded,
            "a completed record with no evidence against it stays downloaded"
        );
        let note = note.expect("the user is told the check did not happen");
        assert!(
            note.contains("could not be checked"),
            "the note must say the check did not happen: {note}"
        );
        assert!(
            !note.contains("no longer on disk"),
            "the note must not claim the file is gone: {note}"
        );
        assert!(
            !note.contains("downloaded again"),
            "and must not invite a re-download of something that may be there: {note}"
        );
    }

    #[test]
    fn the_three_answers_are_three_different_outcomes() {
        // Stated as one test because the point is the *difference*: a screen
        // that collapsed any two of these would be wrong in a way none of the
        // single-case tests above would catch.
        let present = reconcile_status(ContentStatus::New, Some(DownloadState::Completed), Presence::Present);
        let missing = reconcile_status(ContentStatus::New, Some(DownloadState::Completed), Presence::Missing);
        let unavailable =
            reconcile_status(ContentStatus::New, Some(DownloadState::Completed), Presence::Unavailable);

        assert_eq!(present.0, ContentStatus::Downloaded);
        assert_eq!(missing.0, ContentStatus::Ready);
        assert_eq!(unavailable.0, ContentStatus::Downloaded);

        assert!(present.1.is_none(), "nothing to say about a file that is there");
        assert_ne!(missing.1, unavailable.1, "the two notes must differ");
    }

    #[test]
    fn source_unavailability_outranks_a_local_download() {
        for source in [
            ContentStatus::Unavailable,
            ContentStatus::Private,
            ContentStatus::Unsupported,
        ] {
            let (status, _) =
                reconcile_status(source, Some(DownloadState::Completed), Presence::Present);
            assert_eq!(status, source, "a local download must not resurrect {source}");
        }
    }

    #[test]
    fn an_active_download_reads_as_downloading() {
        for state in [
            DownloadState::Queued,
            DownloadState::Downloading,
            DownloadState::Paused,
        ] {
            let (status, _) =
                reconcile_status(ContentStatus::New, Some(state), Presence::Missing);
            assert_eq!(status, ContentStatus::Downloading, "for {state}");
        }
    }

    #[test]
    fn a_cancelled_attempt_leaves_the_content_status_alone() {
        let (status, note) = reconcile_status(
            ContentStatus::New,
            Some(DownloadState::Cancelled),
            Presence::Missing,
        );
        assert_eq!(status, ContentStatus::New);
        assert!(note.unwrap().contains("cancelled"));
    }

    #[test]
    fn the_shared_rule_sees_what_the_inspection_saw() {
        // The Library does not decide what "unavailable" means — `presence`
        // does, and this is the seam between them. A file under a root that is
        // not there is unavailable, and the rule must treat it as such rather
        // than as an absent file.
        let temp = tempfile::tempdir().expect("temp dir");
        let absent_root = temp.path().join("not-mounted");
        let recorded = absent_root.join("clip.mp4");

        let answer = file_answer(&absent_root, Some(recorded.to_string_lossy().as_ref()), &mut PresenceCache::default());
        assert_eq!(answer.presence, Presence::Unavailable);
        assert_eq!(answer.state.as_deref(), Some("unavailable"));
        assert!(answer.reason.is_some(), "the screen can say why");

        let (status, note) = reconcile_status(
            ContentStatus::New,
            Some(DownloadState::Completed),
            answer.presence,
        );
        assert_eq!(status, ContentStatus::Downloaded);
        assert!(note.unwrap().contains("could not be checked"));
    }

    #[test]
    fn a_row_with_no_recorded_destination_has_no_answer_rather_than_a_wrong_one() {
        // `None` is not "missing". A post that was never downloaded produced no
        // file, and the question was never asked — so nothing is reported, which
        // is different from reporting any of the three answers.
        let temp = tempfile::tempdir().expect("temp dir");
        let answer = file_answer(temp.path(), None, &mut PresenceCache::default());

        assert_eq!(answer.state, None);
        assert_eq!(answer.reason, None);
    }

    // --- derived filters -----------------------------------------------------

    #[test]
    fn an_unknown_duration_is_not_a_zero_second_one() {
        let filter = LibraryFilter {
            min_duration_seconds: Some(30.0),
            ..Default::default()
        };
        // Unknown duration: kept, because nothing said it is short.
        assert!(matches_derived(&entry(None, ContentStatus::New, MediaType::Video), &filter, LibraryView::All));

        let mut short = entry(None, ContentStatus::New, MediaType::Video);
        short.duration_seconds = Some(5.0);
        assert!(!matches_derived(&short, &filter, LibraryView::All));

        let mut long = entry(None, ContentStatus::New, MediaType::Video);
        long.duration_seconds = Some(90.0);
        assert!(matches_derived(&long, &filter, LibraryView::All));
    }

    #[test]
    fn an_unknown_resolution_is_not_dropped_by_a_minimum_width() {
        let filter = LibraryFilter {
            min_width: Some(1920),
            ..Default::default()
        };
        assert!(matches_derived(&entry(None, ContentStatus::New, MediaType::Video), &filter, LibraryView::All));

        let mut small = entry(None, ContentStatus::New, MediaType::Video);
        small.width = Some(640);
        assert!(!matches_derived(&small, &filter, LibraryView::All));
    }

    #[test]
    fn a_view_narrows_by_status_and_type() {
        let downloaded = entry(None, ContentStatus::Downloaded, MediaType::Video);
        assert!(matches_derived(&downloaded, &LibraryFilter::default(), LibraryView::Downloaded));
        assert!(!matches_derived(&downloaded, &LibraryFilter::default(), LibraryView::New));
        assert!(!matches_derived(&downloaded, &LibraryFilter::default(), LibraryView::Images));

        let image = entry(None, ContentStatus::New, MediaType::Image);
        assert!(matches_derived(&image, &LibraryFilter::default(), LibraryView::Images));
        assert!(!matches_derived(&image, &LibraryFilter::default(), LibraryView::Videos));
    }

    #[test]
    fn a_carousel_belongs_to_the_images_view() {
        let carousel = entry(None, ContentStatus::New, MediaType::Carousel);
        assert!(matches_derived(&carousel, &LibraryFilter::default(), LibraryView::Images));
    }

    // --- CSV -----------------------------------------------------------------

    #[test]
    fn csv_quotes_only_what_it_has_to() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn csv_defuses_formula_injection() {
        // Each of these is a caption a user could legitimately have. In a
        // spreadsheet they are instructions, so they are marked as text.
        for dangerous in [
            "=1+1",
            "+SUM(A1)",
            "-2+3",
            "@SUM(A1)",
            "=HYPERLINK(\"http://evil\",\"click\")",
        ] {
            let rendered = csv_field(dangerous);
            assert!(
                rendered.starts_with('\'') || rendered.starts_with("\"'"),
                "{dangerous} must not begin a formula, got {rendered}"
            );
        }
        // An ordinary caption is untouched.
        assert_eq!(csv_field("Sunset over the sea"), "Sunset over the sea");
    }

    #[test]
    fn csv_keeps_unicode_and_empty_fields() {
        let text = csv_field("夕阳下的海面，很美");
        assert_eq!(text, "夕阳下的海面，很美");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn csv_has_a_header_and_one_row_per_record() {
        let entries = vec![
            entry(Some("first"), ContentStatus::Downloaded, MediaType::Video),
            entry(None, ContentStatus::New, MediaType::Image),
        ];
        let csv = to_csv(&entries);
        let lines: Vec<&str> = csv.trim_end_matches("\r\n").split("\r\n").collect();
        assert_eq!(lines.len(), 3, "a header and two records");
        assert_eq!(lines[0], CSV_COLUMNS.join(","));
        // The second record has no caption, and its cell is empty rather than
        // the word "null".
        assert!(!lines[2].contains("null"));
    }

    #[test]
    fn csv_numbers_are_not_quoted_and_unknown_ones_are_empty() {
        assert_eq!(csv_number("12"), "12");
        assert_eq!(csv_number(""), "");
    }

    #[test]
    fn an_empty_export_is_still_a_valid_document() {
        assert_eq!(to_csv(&[]), format!("{}\r\n", CSV_COLUMNS.join(",")));
        let json = to_json(&[]).unwrap();
        assert_eq!(json, "[]");
    }

    // --- JSON ----------------------------------------------------------------

    #[test]
    fn json_is_valid_utf8_and_round_trips_unicode() {
        let entries = vec![entry(
            Some("夕阳下的海面 — a caption with \"quotes\" and a \\ backslash"),
            ContentStatus::Downloaded,
            MediaType::Video,
        )];
        let json = to_json(&entries).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let first = &parsed.as_array().unwrap()[0];
        assert_eq!(first["creatorName"], "Alice");
        assert_eq!(first["postId"], "7123456789012345678");
        assert_eq!(first["mediaType"], "video");
        assert_eq!(first["status"], "downloaded");
        assert!(first["caption"]
            .as_str()
            .unwrap()
            .contains("夕阳下的海面"));
    }

    #[test]
    fn json_uses_null_for_what_it_does_not_know() {
        let entries = vec![entry(None, ContentStatus::New, MediaType::Video)];
        let json = to_json(&entries).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let first = &parsed.as_array().unwrap()[0];
        assert!(first["caption"].is_null());
        assert!(first["localPath"].is_null());
        assert!(first["durationSeconds"].is_null());
        assert_eq!(first["filePresent"], false);
    }

    #[test]
    fn an_export_never_carries_a_media_url() {
        // The post URL is a page; a signed media URL is a token. Only the former
        // belongs in a file a user may share.
        let mut item = entry(None, ContentStatus::Downloaded, MediaType::Video);
        item.post.url = "https://www.douyin.com/video/7123456789012345678".into();
        let json = to_json(&[item]).unwrap();
        assert!(!json.contains("playwm"));
        assert!(!json.contains("video_id="));
    }

    #[test]
    fn a_query_defaults_to_the_whole_library() {
        let query = LibraryQuery::default();
        assert_eq!(query.view, LibraryView::All);
        assert_eq!(query.offset, 0);
        assert!(query.limit.is_none());
        assert!(query.filter.is_empty());
    }

    #[test]
    fn a_bad_date_filter_is_a_validation_error() {
        use crate::models::library::parse_instant;
        let err = parse_instant("not a date").unwrap_err();
        assert_eq!(err.kind(), crate::errors::AppErrorKind::Validation);
        assert!(parse_instant("2026-10-11T00:00:00Z").is_ok());
        assert!(parse_instant("2026-10-11").is_ok());
    }
}
