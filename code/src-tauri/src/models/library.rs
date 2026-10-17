//! Library view models.
//!
//! A [`LibraryEntry`] is one *post*, reconciled with everything the application
//! knows locally about it: who wrote it, what it holds, what happened when it
//! was downloaded, and whether the file that download produced is still there.
//!
//! # Why the entry is a post and not a media item
//!
//! A post discovered by a profile listing has no media items at all — the Phase
//! 03 design resolves media only when a download is actually asked for — so a
//! media-item-level library would be empty of everything a user has discovered
//! but not yet downloaded, which is exactly what the "New" view is for.
//!
//! The cost of the post-level choice is that a carousel is one row. That is
//! covered rather than hidden: [`LibraryEntry::media_item_count`] and
//! [`LibraryEntry::downloaded_item_count`] say how many resources the post holds
//! and how many of them are on disk, so a three-image carousel with one image
//! downloaded is visibly partial rather than reported as a completed post.

use serde::{Deserialize, Deserializer, Serialize};

use super::common::{ContentStatus, DownloadState, MediaType, Timestamp};
use super::Post;
use crate::errors::{AppError, AppResult};

/// Which slice of the library a caller wants.
///
/// The views are defined against the reconciled [`ContentStatus`], not against
/// raw database columns, so "Downloaded" means a file that is actually there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryView {
    /// Everything the library holds.
    All,
    Videos,
    Images,
    Audio,
    /// Every resource of the post is present locally.
    ///
    /// Not merely "has a finished download": a carousel with one of its three
    /// images on disk is a partial download and belongs here only once the rest
    /// of it is.
    Downloaded,
    /// The last download attempt failed.
    Failed,
    /// Discovered, with no successful local download.
    New,
}

impl LibraryView {
    /// The media types this view is restricted to, if any.
    pub fn media_types(self) -> &'static [MediaType] {
        match self {
            Self::Videos => &[MediaType::Video],
            // A carousel is Douyin's image post, so it belongs with images.
            Self::Images => &[MediaType::Image, MediaType::Carousel],
            Self::Audio => &[MediaType::Audio],
            _ => &[],
        }
    }

    /// The statuses this view is restricted to, if any.
    pub fn statuses(self) -> &'static [ContentStatus] {
        match self {
            Self::Downloaded => &[ContentStatus::Downloaded],
            Self::Failed => &[ContentStatus::Failed],
            Self::New => &[ContentStatus::New, ContentStatus::Ready],
            _ => &[],
        }
    }
}

/// How a library listing is ordered.
///
/// Every variant has a deterministic tie-breaker, so two rows that compare equal
/// on the primary field never swap places between pages — which is what makes
/// pagination stable rather than merely usually stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibrarySort {
    #[default]
    Newest,
    Oldest,
    TitleAsc,
    CreatorAsc,
    DownloadedFirst,
}

/// Everything a caller can narrow a library listing by.
///
/// Every field is optional, and `Default` means "no narrowing at all" — which is
/// the All view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryFilter {
    /// Free text, matched against creator, caption, post id, URL, and the path
    /// of any file the post produced.
    #[serde(default)]
    pub search: Option<String>,
    /// Restrict to these media types. Empty means all.
    #[serde(default)]
    pub media_types: Vec<MediaType>,
    /// Restrict to one creator by its local id.
    #[serde(default)]
    pub creator_id: Option<String>,
    /// Restrict to these reconciled statuses. Empty means all.
    #[serde(default)]
    pub statuses: Vec<ContentStatus>,
    /// Published on or after this instant.
    #[serde(default, deserialize_with = "deserialize_optional_instant")]
    pub published_after: Option<Timestamp>,
    /// Published on or before this instant.
    #[serde(default, deserialize_with = "deserialize_optional_instant")]
    pub published_before: Option<Timestamp>,
    /// Shortest runtime to include. Posts whose duration is unknown are kept —
    /// an unmeasured duration is not a zero-second one.
    #[serde(default)]
    pub min_duration_seconds: Option<f64>,
    /// Smallest width to include; unknown dimensions are kept, for the same
    /// reason.
    #[serde(default)]
    pub min_width: Option<u32>,
}

impl LibraryFilter {
    /// Build the filter a view implies.
    pub fn for_view(view: LibraryView) -> Self {
        Self {
            media_types: view.media_types().to_vec(),
            statuses: view.statuses().to_vec(),
            ..Default::default()
        }
    }

    /// Whether this filter asks for nothing in particular.
    pub fn is_empty(&self) -> bool {
        let no_search = self
            .search
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true);
        no_search
            && self.media_types.is_empty()
            && self.creator_id.is_none()
            && self.statuses.is_empty()
            && self.published_after.is_none()
            && self.published_before.is_none()
            && self.min_duration_seconds.is_none()
            && self.min_width.is_none()
    }
}

/// Parse a date filter.
///
/// Two forms are accepted, because two things produce them: an RFC 3339 instant,
/// which is what the rest of the application passes around, and a bare
/// `YYYY-MM-DD` date, which is what a browser date input produces. A bare date
/// means midnight UTC on that day, so "published after 2026-10-11" is a
/// comparison against a defined instant rather than a vague one.
pub fn parse_instant(raw: &str) -> AppResult<Timestamp> {
    let trimmed = raw.trim();
    if let Ok(instant) = trimmed.parse::<Timestamp>() {
        return Ok(instant);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        // Midnight on that date. A date input gives no timezone, so UTC is the
        // stated assumption rather than an accident of the machine's locale.
        return Ok(date.and_hms_opt(0, 0, 0).expect("midnight exists").and_utc());
    }
    Err(AppError::validation("invalid date filter")
        .with_user_message("That date filter is not a valid date.")
        .with_technical_detail(trimmed.chars().take(40).collect::<String>()))
}

/// Deserialize one of [`LibraryFilter`]'s instants, or `null`.
///
/// Written by hand so a bad date is a typed validation error rather than a
/// serde message the frontend would have to parse.
fn deserialize_optional_instant<'de, D>(deserializer: D) -> Result<Option<Timestamp>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error as _;
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Ok(None),
        Some(text) => parse_instant(&text).map(Some).map_err(D::Error::custom),
    }
}

/// One post, as the Library presents it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    /// The stored post. Its `media_items` are not loaded by this query — the
    /// summary fields below are what the Library shows.
    pub post: Post,

    pub creator_name: Option<String>,
    pub creator_handle: Option<String>,

    /// The reconciled status: source availability folded with local download
    /// state and the existence of the file that download produced.
    pub status: ContentStatus,
    /// The newest download's state, whatever it was.
    pub download_state: Option<DownloadState>,
    /// What was established about that file: `present`, `missing`, or
    /// `unavailable`.
    ///
    /// Three answers rather than a boolean, because the third is the point. A
    /// file this application could not look at — an unreadable directory, a
    /// drive that is not attached, a stored path from a download folder the user
    /// has since changed — is not a file that is gone, and a screen that says so
    /// is asserting something nobody checked.
    ///
    /// `None` when no completed download recorded a destination: the question
    /// was never asked, which is different from every one of the three answers.
    /// Only meaningful next to `download_state`.
    pub file_state: Option<String>,
    /// Why the answer could not be established, when it could not.
    ///
    /// `Some` only alongside `file_state == Some("unavailable")`. Carried
    /// separately so a screen can say *why* — a refusal and a missing drive call
    /// for different words — and so the wording can match what the Downloads
    /// screen already shows for the same condition.
    pub file_state_reason: Option<String>,
    /// The file the newest completed download produced, when there was one.
    pub local_path: Option<String>,
    /// When that download finished.
    pub downloaded_at: Option<Timestamp>,
    /// Why the last attempt failed, when it did.
    pub error: Option<String>,

    /// How many media resources the post holds. `0` for a post that has only
    /// been discovered, not resolved.
    pub media_item_count: usize,
    /// How many of them have a completed download whose file exists.
    pub downloaded_item_count: usize,

    /// From the post's media, when the source reported them.
    pub duration_seconds: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl LibraryEntry {
    /// A display title that is never empty.
    pub fn title(&self) -> String {
        self.post.display_title()
    }

    /// `1920x1080` when both dimensions are known.
    pub fn resolution(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        }
    }

    /// Whether some, but not all, of the post's resources are downloaded.
    pub fn is_partially_downloaded(&self) -> bool {
        self.media_item_count > 0
            && self.downloaded_item_count > 0
            && self.downloaded_item_count < self.media_item_count
    }
}

/// How many entries each view holds, for the tabs.
///
/// Counted over the *whole* matching set rather than the page on screen, so a
/// tab never claims a number the listing cannot produce.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCounts {
    pub all: usize,
    pub videos: usize,
    pub images: usize,
    pub audio: usize,
    pub downloaded: usize,
    pub failed: usize,
    pub new: usize,
}

impl LibraryCounts {
    pub fn set(&mut self, view: LibraryView, value: usize) {
        match view {
            LibraryView::All => self.all = value,
            LibraryView::Videos => self.videos = value,
            LibraryView::Images => self.images = value,
            LibraryView::Audio => self.audio = value,
            LibraryView::Downloaded => self.downloaded = value,
            LibraryView::Failed => self.failed = value,
            LibraryView::New => self.new = value,
        }
    }
}

/// One page of a library listing, with everything the screen needs to be
/// consistent: the rows, how many there are in total, and the per-view counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryPage {
    pub entries: Vec<LibraryEntry>,
    /// How many entries match the filter in total, not how many are on screen.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub counts: LibraryCounts,
    /// How many posts the filter had to look at before statuses were applied.
    ///
    /// Surfaced rather than hidden: it is the honest bound on how much work a
    /// query did, and it is what tells a maintainer whether the local scan is
    /// still proportionate.
    pub scanned: usize,
    /// True when the read stopped at the scan limit, so the total and the tab
    /// counts are floors rather than the whole answer.
    ///
    /// The limit exists to bound one query's work, and there is nothing wrong
    /// with a bound — but a bound the user cannot see is a silent lie: past it,
    /// "of N" reads as complete when it is a floor. This says so.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Platform;

    fn entry(post: Post) -> LibraryEntry {
        LibraryEntry {
            post,
            creator_name: None,
            creator_handle: None,
            status: ContentStatus::New,
            download_state: None,
            file_state: None,
            file_state_reason: None,
            local_path: None,
            downloaded_at: None,
            error: None,
            media_item_count: 0,
            downloaded_item_count: 0,
            duration_seconds: None,
            width: None,
            height: None,
        }
    }

    #[test]
    fn the_images_view_covers_carousels_as_well_as_images() {
        // A carousel is Douyin's image post; leaving it out would hide every
        // multi-image post from the Images tab.
        let types = LibraryView::Images.media_types();
        assert!(types.contains(&MediaType::Image));
        assert!(types.contains(&MediaType::Carousel));
        assert!(!types.contains(&MediaType::Video));
    }

    #[test]
    fn the_new_view_means_no_successful_local_download() {
        let statuses = LibraryView::New.statuses();
        assert!(statuses.contains(&ContentStatus::New));
        // `Ready` is a post whose download is known to be gone; it is still
        // something that can be downloaded, so it belongs with the new items.
        assert!(statuses.contains(&ContentStatus::Ready));
        assert!(!statuses.contains(&ContentStatus::Downloaded));
        assert!(!statuses.contains(&ContentStatus::Failed));
    }

    #[test]
    fn the_downloaded_view_is_a_status_not_a_guess() {
        assert_eq!(LibraryView::Downloaded.statuses(), &[ContentStatus::Downloaded]);
        assert!(LibraryView::Downloaded.media_types().is_empty());
    }

    #[test]
    fn an_empty_filter_asks_for_nothing() {
        assert!(LibraryFilter::default().is_empty());
        assert!(LibraryFilter {
            search: Some("   ".into()),
            ..Default::default()
        }
        .is_empty());
        assert!(!LibraryFilter {
            search: Some("cat".into()),
            ..Default::default()
        }
        .is_empty());
        assert!(!LibraryFilter::for_view(LibraryView::Videos).is_empty());
    }

    #[test]
    fn a_partly_downloaded_carousel_is_visibly_partial() {
        let mut post = Post::new(Platform::Douyin, "1", MediaType::Carousel, "https://d/1");
        post.media_items.clear();
        let mut e = entry(post);
        e.media_item_count = 3;
        e.downloaded_item_count = 1;
        assert!(e.is_partially_downloaded());

        e.downloaded_item_count = 3;
        assert!(!e.is_partially_downloaded());
        e.downloaded_item_count = 0;
        assert!(!e.is_partially_downloaded());
        // A listing post has no resources at all, so nothing is "partial".
        e.media_item_count = 0;
        e.downloaded_item_count = 0;
        assert!(!e.is_partially_downloaded());
    }

    #[test]
    fn resolution_is_absent_unless_both_dimensions_are_known() {
        let post = Post::new(Platform::Douyin, "1", MediaType::Video, "https://d/1");
        let mut e = entry(post);
        assert!(e.resolution().is_none());
        e.width = Some(1920);
        assert!(e.resolution().is_none(), "a width alone is not a resolution");
        e.height = Some(1080);
        assert_eq!(e.resolution().as_deref(), Some("1920x1080"));
    }

    #[test]
    fn counts_are_filled_by_view() {
        let mut counts = LibraryCounts::default();
        counts.set(LibraryView::Videos, 3);
        counts.set(LibraryView::New, 7);
        assert_eq!(counts.videos, 3);
        assert_eq!(counts.new, 7);
        assert_eq!(counts.all, 0);
    }

    #[test]
    fn a_date_filter_accepts_an_instant_or_a_bare_date() {
        let instant = parse_instant("2026-10-11T13:45:00Z").unwrap();
        assert_eq!(instant.to_rfc3339(), "2026-10-11T13:45:00+00:00");

        // What an `<input type="date">` sends: midnight UTC on that day.
        let date = parse_instant("2026-10-11").unwrap();
        assert_eq!(date.to_rfc3339(), "2026-10-11T00:00:00+00:00");
        assert!(date < instant);

        assert!(parse_instant("yesterday").is_err());
        assert!(parse_instant("").is_err());
    }

    #[test]
    fn a_filter_deserializes_the_dates_the_frontend_sends() {
        let filter: LibraryFilter = serde_json::from_str(
            r#"{"search":"sunset","mediaTypes":["video"],
                "publishedAfter":"2026-01-01","publishedBefore":"2026-10-11T23:59:59Z",
                "minDurationSeconds":30,"minWidth":1280}"#,
        )
        .expect("deserializes");

        assert_eq!(filter.search.as_deref(), Some("sunset"));
        assert_eq!(filter.media_types, vec![MediaType::Video]);
        assert_eq!(
            filter.published_after.unwrap().to_rfc3339(),
            "2026-01-01T00:00:00+00:00"
        );
        assert_eq!(filter.min_duration_seconds, Some(30.0));
        assert_eq!(filter.min_width, Some(1280));
    }

    #[test]
    fn an_absent_or_blank_date_is_no_filter_at_all() {
        for json in [
            r#"{}"#,
            r#"{"publishedAfter":null}"#,
            r#"{"publishedAfter":""}"#,
            r#"{"publishedAfter":"   "}"#,
        ] {
            let filter: LibraryFilter = serde_json::from_str(json).expect("deserializes");
            assert!(filter.published_after.is_none(), "for {json}");
            assert!(filter.is_empty(), "for {json}");
        }
    }

    #[test]
    fn a_malformed_date_is_refused_rather_than_ignored() {
        // Silently dropping it would show the user everything while the screen
        // still claimed a date filter was applied.
        let err = serde_json::from_str::<LibraryFilter>(r#"{"publishedAfter":"11/10/2026"}"#)
            .expect_err("must not parse");
        let message = err.to_string();
        assert!(message.contains("invalid date filter"), "got {message}");
        assert!(message.contains("validation_error"), "got {message}");
    }
}
