//! Read-only queries backing the Library.
//!
//! This is the same [`Connection`] and the same `Database` pool the other
//! repositories use — it is a module of its own only because the query joins
//! posts, creators, media and downloads at once, and folding that into
//! `posts.rs` would bury the much simpler CRUD that lives there.
//!
//! # What is done in SQL and what is not
//!
//! The **high-cardinality** narrowing happens here: free-text search, content
//! type, creator, and publication date. Those are the filters that decide how
//! many rows there are to look at, and they are the ones that benefit from
//! indexes.
//!
//! The **derived** filters do not: duration, resolution, and — above all —
//! status, which depends on whether the file a completed download produced is
//! still on disk. File existence is not a fact SQLite has, and a status computed
//! without it would be exactly the "a row is not a file" mistake this
//! application exists to avoid. So the rows this module returns carry the raw
//! facts, and the application service derives the status from them.
//!
//! [`scan`] deliberately does not paginate. The service applies the status and
//! the derived filters first, because paginating before a filter that can drop
//! rows would produce pages that are short, or empty, for no visible reason.

use rusqlite::{Connection, Row};

use crate::database::row;
use crate::errors::AppResult;
use crate::models::{DownloadState, LibraryFilter, LibrarySort, Post, Timestamp};

/// One post with everything the Library needs to describe it.
///
/// Raw facts only: the status is derived from these by the application service,
/// which is where the file check lives.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryRow {
    pub post: Post,
    pub creator_name: Option<String>,
    pub creator_handle: Option<String>,
    /// Media resources the post holds. `0` for one that has only been
    /// discovered.
    pub media_item_count: usize,
    pub duration_seconds: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// State of the post's most recent download, whatever it was.
    pub latest_download_state: Option<DownloadState>,
    pub latest_destination: Option<String>,
    pub latest_completed_at: Option<Timestamp>,
    pub latest_error: Option<String>,
}

/// The post columns, in the order [`map_row`] reads them.
const POST_COLUMNS: &str = "p.id, p.platform, p.platform_post_id, p.creator_id, p.creator_handle, \
     p.url, p.canonical_url, p.caption, p.media_type, p.thumbnail_url, p.published_at, \
     p.status, p.metadata, p.created_at, p.updated_at";

/// The joined select every library query is built from.
///
/// The download join is a correlated subquery rather than a `GROUP BY` so it
/// returns exactly one row per post — the newest attempt — which is what the
/// status rule is written against.
fn base_select() -> String {
    format!(
        "SELECT {POST_COLUMNS}, c.display_name, c.handle, \
         (SELECT COUNT(*) FROM media_items m WHERE m.post_id = p.id), \
         (SELECT MIN(m.duration_seconds) FROM media_items m \
            WHERE m.post_id = p.id AND m.duration_seconds IS NOT NULL), \
         (SELECT MAX(m.width) FROM media_items m \
            WHERE m.post_id = p.id AND m.width IS NOT NULL), \
         (SELECT MAX(m.height) FROM media_items m \
            WHERE m.post_id = p.id AND m.height IS NOT NULL), \
         d.state, d.destination_path, d.completed_at, d.error \
         FROM posts p \
         LEFT JOIN creators c ON c.id = p.creator_id \
         LEFT JOIN downloads d ON d.id = ( \
            SELECT id FROM downloads WHERE post_id = p.id \
            ORDER BY created_at DESC, rowid DESC LIMIT 1)"
    )
}

/// Highest post id scanned in one query.
///
/// A local library is a personal collection, not a warehouse, but "scan
/// everything" should still have a stated bound rather than be an accident of
/// how much the user has downloaded. Exceeding it narrows nothing — the filters
/// still decide which rows are *shown*; this only caps how many are considered,
/// and whether the cap was reached is reported through [`ScanOutcome::truncated`].
pub const MAX_SCAN: usize = 20_000;

/// What one read of the matching rows produced.
pub struct ScanOutcome {
    pub rows: Vec<LibraryRow>,
    /// True when the read stopped because it filled its limit and at least one
    /// matching row was left behind.
    ///
    /// Decided by reading **one row more than the cap**, not by comparing the
    /// count against it: exactly `MAX_SCAN` matching rows is a complete answer,
    /// and a comparison (`count >= cap`) would call it truncated. One extra
    /// candidate is the only way to tell "filled to the brim" from "more behind
    /// it", and it costs one row.
    pub truncated: bool,
}

/// Every post matching the filter's SQL-side narrowing, and whether more exist.
///
/// At most [`MAX_SCAN`] rows come back. The query fetches **one candidate
/// beyond** the cap so `truncated` is a fact about the match rather than an
/// inference from the count: a library of exactly 20,000 is not a truncated
/// one, and no comparison of a count against the cap can tell that apart from
/// 20,001.
pub fn scan(conn: &Connection, filter: &LibraryFilter, sort: LibrarySort) -> AppResult<ScanOutcome> {
    let (mut sql, params) = build_query(filter, sort);
    // One candidate beyond the cap — see the doc above. The loop below discards
    // it, keeping at most MAX_SCAN rows for the caller.
    sql.push_str(&format!(" LIMIT {}", MAX_SCAN + 1));

    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|value| value.as_ref()).collect();
    let mut rows = stmt.query(bound.as_slice())?;
    let mut out = Vec::new();
    let mut beyond = false;
    while let Some(row) = rows.next()? {
        if out.len() == MAX_SCAN {
            // One row past the cap: the match does not end where the read did.
            beyond = true;
            break;
        }
        out.push(map_row(row)?);
    }
    Ok(ScanOutcome {
        rows: out,
        truncated: beyond,
    })
}

/// Media resources of a post that have a completed download, with the file each
/// one produced.
///
/// Asked for the page's posts in one query rather than one query per post, so
/// showing fifty rows costs one read instead of fifty.
pub fn completed_downloads(
    conn: &Connection,
    post_ids: &[String],
) -> AppResult<Vec<(String, String, Option<String>)>> {
    if post_ids.is_empty() {
        return Ok(Vec::new());
    }
    // A parameter per id; `IN (?, ?, ...)`. Built from the length, never from
    // the ids themselves, so the ids stay data.
    let placeholders = vec!["?"; post_ids.len()].join(", ");
    let sql = format!(
        "SELECT post_id, media_item_id, destination_path FROM downloads \
         WHERE state = 'completed' AND post_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let bound: Vec<&dyn rusqlite::ToSql> =
        post_ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let mut rows = stmt.query(bound.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let post_id: String = row.get(0)?;
        let media_item_id: Option<String> = row.get(1)?;
        let destination: Option<String> = row.get(2)?;
        if let Some(media_item_id) = media_item_id {
            out.push((post_id, media_item_id, destination));
        }
    }
    Ok(out)
}

/// The distinct creators present in the library, for the filter menu.
pub fn creators(conn: &Connection) -> AppResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.display_name FROM creators c \
         WHERE EXISTS (SELECT 1 FROM posts p WHERE p.creator_id = c.id) \
         ORDER BY LOWER(c.display_name), c.id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((row.get(0)?, row.get(1)?));
    }
    Ok(out)
}

/// Build the statement and its bound parameters.
///
/// Every value is a bound parameter. The only text spliced into the statement is
/// a placeholder index or a value from a closed enum, so no user input can
/// become SQL.
fn build_query(filter: &LibraryFilter, sort: LibrarySort) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(term) = search_term(filter) {
        // One parameter, referenced eleven times: SQLite numbers them, so the
        // same value does not have to be pushed eleven times.
        //
        // `ESCAPE '\'` is what makes the escaping in `search_term` mean
        // anything — without it SQLite reads the backslash as an ordinary
        // character and a search for `100%` looks for the literal text `\%`.
        let idx = params.len() + 1;
        clauses.push(format!(
            "(c.display_name LIKE ?{idx} ESCAPE '\\' \
             OR c.platform_id LIKE ?{idx} ESCAPE '\\' \
             OR c.handle LIKE ?{idx} ESCAPE '\\' \
             OR p.caption LIKE ?{idx} ESCAPE '\\' \
             OR p.platform_post_id LIKE ?{idx} ESCAPE '\\' \
             OR p.url LIKE ?{idx} ESCAPE '\\' \
             OR p.canonical_url LIKE ?{idx} ESCAPE '\\' \
             OR d.destination_path LIKE ?{idx} ESCAPE '\\' \
             OR d.file_name LIKE ?{idx} ESCAPE '\\' \
             OR EXISTS (SELECT 1 FROM download_history h \
                        WHERE h.post_id = p.id AND h.local_path LIKE ?{idx} ESCAPE '\\'))"
        ));
        params.push(Box::new(term));
    }

    if !filter.media_types.is_empty() {
        let placeholders: Vec<String> = filter
            .media_types
            .iter()
            .map(|media_type| {
                params.push(Box::new(media_type.as_str().to_string()));
                format!("?{}", params.len())
            })
            .collect();
        clauses.push(format!("p.media_type IN ({})", placeholders.join(", ")));
    }

    if let Some(creator_id) = filter.creator_id.as_deref().filter(|id| !id.trim().is_empty()) {
        params.push(Box::new(creator_id.to_string()));
        clauses.push(format!("p.creator_id = ?{}", params.len()));
    }

    if let Some(after) = filter.published_after {
        params.push(Box::new(row::timestamp_to_sql(after)));
        clauses.push(format!("p.published_at IS NOT NULL AND p.published_at >= ?{}", params.len()));
    }
    if let Some(before) = filter.published_before {
        params.push(Box::new(row::timestamp_to_sql(before)));
        clauses.push(format!("p.published_at IS NOT NULL AND p.published_at <= ?{}", params.len()));
    }

    let mut sql = base_select();
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push(' ');
    sql.push_str(order_by(sort));
    (sql, params)
}

/// The search term as a `LIKE` pattern, or `None` when there is nothing to match.
///
/// `LIKE` is case-insensitive for ASCII by default in SQLite, which is the
/// behaviour the Library documents. Wildcards in the user's own input are
/// escaped, so searching for `100%` looks for the literal text rather than
/// matching everything.
fn search_term(filter: &LibraryFilter) -> Option<String> {
    let raw = filter.search.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    Some(format!("%{escaped}%"))
}

/// The ordering clause, with a tie-breaker on every branch.
fn order_by(sort: LibrarySort) -> &'static str {
    match sort {
        // A post whose publication date is unknown sorts last rather than
        // first: it is not the newest thing in the library, it is the one with
        // the least information.
        LibrarySort::Newest => {
            "ORDER BY (p.published_at IS NULL), p.published_at DESC, p.id ASC"
        }
        LibrarySort::Oldest => {
            "ORDER BY (p.published_at IS NULL), p.published_at ASC, p.id ASC"
        }
        LibrarySort::TitleAsc => {
            // The key is the *display* title, fallback included, so sorting by
            // title produces the order a user reading the column would expect.
            // Sorting a captionless post under its raw id would put "douyin post
            // 9100" at the top of a list it reads as a `d`.
            "ORDER BY LOWER(COALESCE(NULLIF(TRIM(p.caption), ''), \
             p.platform || ' post ' || p.platform_post_id)) ASC, p.id ASC"
        }
        LibrarySort::CreatorAsc => {
            "ORDER BY LOWER(COALESCE(c.display_name, '')) ASC, p.id ASC"
        }
        LibrarySort::DownloadedFirst => {
            "ORDER BY CASE WHEN d.state = 'completed' THEN 0 ELSE 1 END, \
             (p.published_at IS NULL), p.published_at DESC, p.id ASC"
        }
    }
}

/// Read one joined row.
fn map_row(row: &Row<'_>) -> AppResult<LibraryRow> {
    let post = Post {
        id: row::text(row, 0)?,
        platform: row::parse_enum(&row::text(row, 1)?, "platform")?,
        platform_post_id: row::text(row, 2)?,
        creator_id: row::opt_text(row, 3)?,
        creator_handle: row::opt_text(row, 4)?,
        url: row::text(row, 5)?,
        canonical_url: row::opt_text(row, 6)?,
        caption: row::opt_text(row, 7)?,
        media_type: row::parse_enum(&row::text(row, 8)?, "media_type")?,
        thumbnail_url: row::opt_text(row, 9)?,
        published_at: row::opt_timestamp(row, 10, "posts.published_at")?,
        status: row::parse_enum(&row::text(row, 11)?, "content_status")?,
        // The Library shows a summary, not the media themselves; loading every
        // item of every row would be a second query per row for data the screen
        // does not render.
        media_items: Vec::new(),
        metadata: row::metadata(row, 12)?,
        created_at: row::timestamp(row, 13, "posts.created_at")?,
        updated_at: row::timestamp(row, 14, "posts.updated_at")?,
    };

    let media_item_count = row::int(row, 17)?.max(0) as usize;

    Ok(LibraryRow {
        post,
        creator_name: row::opt_text(row, 15)?,
        creator_handle: row::opt_text(row, 16)?,
        media_item_count,
        duration_seconds: row::opt_f64(row, 18)?,
        width: row::opt_uint(row, 19)?,
        height: row::opt_uint(row, 20)?,
        latest_download_state: row::opt_enum(row::opt_text(row, 21)?, "download_state")?,
        latest_destination: row::opt_text(row, 22)?,
        latest_completed_at: row::opt_timestamp(row, 23, "downloads.completed_at")?,
        latest_error: row::opt_text(row, 24)?,
    })
}
