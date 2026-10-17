//! CRUD for the `posts` table.
//!
//! Posts are keyed by their platform identity `(platform, platform_post_id)`
//! so re-resolving the same content updates the existing row instead of
//! creating a duplicate. Media items are stored separately (see
//! [`super::media`]) and attached on read.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::database::repositories::media;
use crate::database::row;
use crate::errors::AppResult;
use crate::models::common::now;
use crate::models::{ContentStatus, Platform, Post};

const COLUMNS: &str = "id, platform, platform_post_id, creator_id, creator_handle, url, \
     canonical_url, caption, media_type, thumbnail_url, published_at, status, metadata, \
     created_at, updated_at";

fn map(row: &Row<'_>) -> AppResult<Post> {
    Ok(Post {
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
        media_items: Vec::new(),
        metadata: row::metadata(row, 12)?,
        created_at: row::timestamp(row, 13, "posts.created_at")?,
        updated_at: row::timestamp(row, 14, "posts.updated_at")?,
    })
}

/// Insert or update a post keyed by `(platform, platform_post_id)`.
///
/// Returns the persisted local id, which is the existing id when the post was
/// already known. Media items are *not* written here; use
/// [`save_media_items`] once media has been resolved.
pub fn upsert(conn: &Connection, post: &Post) -> AppResult<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM posts WHERE platform = ?1 AND platform_post_id = ?2",
            params![post.platform.as_str(), post.platform_post_id],
            |r| r.get(0),
        )
        .optional()?;

    let id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE posts SET creator_id = ?2, creator_handle = ?3, url = ?4, \
                 canonical_url = ?5, caption = ?6, media_type = ?7, thumbnail_url = ?8, \
                 published_at = ?9, status = ?10, metadata = ?11, updated_at = ?12 WHERE id = ?1",
                params![
                    id,
                    post.creator_id,
                    post.creator_handle,
                    post.url,
                    post.canonical_url,
                    post.caption,
                    post.media_type.as_str(),
                    post.thumbnail_url,
                    post.published_at.map(row::timestamp_to_sql),
                    post.status.as_str(),
                    row::metadata_to_sql(&post.metadata)?,
                    row::timestamp_to_sql(now()),
                ],
            )?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO posts (id, platform, platform_post_id, creator_id, creator_handle, \
                 url, canonical_url, caption, media_type, thumbnail_url, published_at, status, \
                 metadata, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    post.id,
                    post.platform.as_str(),
                    post.platform_post_id,
                    post.creator_id,
                    post.creator_handle,
                    post.url,
                    post.canonical_url,
                    post.caption,
                    post.media_type.as_str(),
                    post.thumbnail_url,
                    post.published_at.map(row::timestamp_to_sql),
                    post.status.as_str(),
                    row::metadata_to_sql(&post.metadata)?,
                    row::storing_timestamp(post.created_at),
                    row::storing_timestamp(post.updated_at),
                ],
            )?;
            post.id.clone()
        }
    };
    Ok(id)
}

/// Fetch a post by local id, including its media items.
pub fn get(conn: &Connection, id: &str) -> AppResult<Option<Post>> {
    let sql = format!("SELECT {COLUMNS} FROM posts WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => {
            let mut post = result??;
            post.media_items = media::list_items_for_post(conn, &post.id)?;
            Ok(Some(post))
        }
        None => Ok(None),
    }
}

/// Store a post discovered by a profile listing, without erasing what is
/// already known about it.
///
/// A listing is a *refresh* of metadata the application has usually seen
/// before, and refreshing must not undo knowledge it already holds:
///
/// * a stored content status that says something (`downloaded`, `failed`,
///   `unavailable`, …) survives a listing that reports the post as merely
///   `new`, because the listing says nothing about local state;
/// * a caption or thumbnail the listing omitted does not replace one that is
///   already stored;
/// * the existing local row — and therefore its id, its media, and its
///   download history — is preserved.
///
/// Returns the persisted local id, which is what selections and download
/// requests must be keyed by.
pub fn upsert_from_listing(conn: &Connection, post: &Post) -> AppResult<String> {
    let existing = get_by_platform_id(conn, post.platform, &post.platform_post_id)?;

    let Some(mut prior) = existing else {
        return upsert(conn, post);
    };

    prior.creator_id = post.creator_id.clone().or(prior.creator_id);
    prior.creator_handle = post.creator_handle.clone().or(prior.creator_handle);
    prior.url = post.url.clone();
    prior.canonical_url = post.canonical_url.clone().or(prior.canonical_url);
    // A listing frequently carries no caption at all (an image post, or a card
    // whose text did not render), and blanking a stored caption would lose
    // information the user already has.
    prior.caption = post.caption.clone().or(prior.caption);
    prior.thumbnail_url = post.thumbnail_url.clone().or(prior.thumbnail_url);
    prior.published_at = post.published_at.or(prior.published_at);
    // A media type derived from real media outranks the listing's provisional
    // guess from the route.
    if prior.media_items.is_empty() {
        prior.media_type = post.media_type;
    }
    // `new` means "just discovered", which is not new information about a post
    // whose status already says more; anything else the listing states wins.
    if post.status != ContentStatus::New {
        prior.status = post.status;
    }
    for (key, value) in &post.metadata {
        prior.metadata.insert(key.clone(), value.clone());
    }
    prior.updated_at = Some(now());

    upsert(conn, &prior)?;
    Ok(prior.id)
}

/// Fetch a post by platform identity.
pub fn get_by_platform_id(
    conn: &Connection,
    platform: Platform,
    platform_post_id: &str,
) -> AppResult<Option<Post>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM posts WHERE platform = ?1 AND platform_post_id = ?2",
            params![platform.as_str(), platform_post_id],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => get(conn, &id),
        None => Ok(None),
    }
}

/// Persist the resolved media items of a post, replacing any previous ones.
pub fn save_media_items(conn: &Connection, post_id: &str, items: &[crate::models::MediaItem]) -> AppResult<()> {
    media::replace_items_for_post(conn, post_id, items)
}

pub fn list_by_creator(
    conn: &Connection,
    creator_id: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<Post>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM posts WHERE creator_id = ?1 \
         ORDER BY published_at DESC, created_at DESC LIMIT ?2 OFFSET ?3"
    );
    query_posts(conn, &sql, params![creator_id, limit, offset])
}

pub fn list_recent(conn: &Connection, limit: i64, offset: i64) -> AppResult<Vec<Post>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM posts ORDER BY published_at DESC, created_at DESC LIMIT ?1 OFFSET ?2"
    );
    query_posts(conn, &sql, params![limit, offset])
}

fn query_posts(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> AppResult<Vec<Post>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |row| Ok(map(row)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Update only the content status of a post.
pub fn set_status(conn: &Connection, id: &str, status: ContentStatus) -> AppResult<bool> {
    let affected = conn.execute(
        "UPDATE posts SET status = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, status.as_str(), row::timestamp_to_sql(now())],
    )?;
    Ok(affected > 0)
}

pub fn count(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM posts", [], |row| row.get(0))?)
}

/// How many posts are stored locally for one creator.
pub fn count_by_creator(conn: &Connection, creator_id: &str) -> AppResult<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM posts WHERE creator_id = ?1",
        [creator_id],
        |row| row.get(0),
    )?)
}

pub fn delete(conn: &Connection, id: &str) -> AppResult<bool> {
    let affected = conn.execute("DELETE FROM posts WHERE id = ?1", [id])?;
    Ok(affected > 0)
}
