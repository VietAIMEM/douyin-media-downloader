//! CRUD for `media_items` and `media_variants`.

use rusqlite::{params, Connection, Row};

use crate::database::row;
use crate::errors::AppResult;
use crate::models::{MediaItem, MediaVariant};

const ITEM_COLUMNS: &str = "id, post_id, media_type, source_url, thumbnail_url, mime_type, \
     extension, width, height, duration_seconds, file_size, metadata";

const VARIANT_COLUMNS: &str = "id, media_item_id, kind, label, source_url, mime_type, extension, \
     width, height, file_size, bitrate_kbps, is_original";

fn map_item(row: &Row<'_>) -> AppResult<MediaItem> {
    Ok(MediaItem {
        id: row::text(row, 0)?,
        post_id: row::text(row, 1)?,
        media_type: row::parse_enum(&row::text(row, 2)?, "media_type")?,
        source_url: row::text(row, 3)?,
        thumbnail_url: row::opt_text(row, 4)?,
        mime_type: row::opt_text(row, 5)?,
        extension: row::opt_text(row, 6)?,
        width: row::opt_uint(row, 7)?,
        height: row::opt_uint(row, 8)?,
        duration_seconds: row::opt_f64(row, 9)?,
        file_size: row::opt_u64(row, 10)?,
        variants: Vec::new(),
        metadata: row::metadata(row, 11)?,
    })
}

fn map_variant(row: &Row<'_>) -> AppResult<MediaVariant> {
    Ok(MediaVariant {
        id: row::text(row, 0)?,
        media_item_id: row::text(row, 1)?,
        kind: row::parse_enum(&row::text(row, 2)?, "variant_kind")?,
        label: row::text(row, 3)?,
        source_url: row::text(row, 4)?,
        mime_type: row::opt_text(row, 5)?,
        extension: row::opt_text(row, 6)?,
        width: row::opt_uint(row, 7)?,
        height: row::opt_uint(row, 8)?,
        file_size: row::opt_u64(row, 9)?,
        bitrate_kbps: row::opt_uint(row, 10)?,
        is_original: row::bool_val(row, 11)?,
    })
}

/// Insert one media item (without its variants).
pub fn insert_item(conn: &Connection, item: &MediaItem) -> AppResult<()> {
    conn.execute(
        "INSERT INTO media_items (id, post_id, media_type, source_url, thumbnail_url, mime_type, \
         extension, width, height, duration_seconds, file_size, metadata, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            item.id,
            item.post_id,
            item.media_type.as_str(),
            item.source_url,
            item.thumbnail_url,
            item.mime_type,
            item.extension,
            item.width.map(|v| v as i64),
            item.height.map(|v| v as i64),
            item.duration_seconds,
            item.file_size.map(|v| v as i64),
            row::metadata_to_sql(&item.metadata)?,
            row::timestamp_to_sql(crate::models::common::now()),
        ],
    )?;
    Ok(())
}

/// Insert one variant.
pub fn insert_variant(conn: &Connection, variant: &MediaVariant) -> AppResult<()> {
    conn.execute(
        "INSERT INTO media_variants (id, media_item_id, kind, label, source_url, mime_type, \
         extension, width, height, file_size, bitrate_kbps, is_original) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            variant.id,
            variant.media_item_id,
            variant.kind.as_str(),
            variant.label,
            variant.source_url,
            variant.mime_type,
            variant.extension,
            variant.width.map(|v| v as i64),
            variant.height.map(|v| v as i64),
            variant.file_size.map(|v| v as i64),
            variant.bitrate_kbps.map(|v| v as i64),
            variant.is_original as i64,
        ],
    )?;
    Ok(())
}

/// Replace all media items (and their variants) for a post.
///
/// Used when re-resolving a post: the old media rows are removed first so a
/// post never accumulates stale media. Runs in a savepoint so a partial
/// failure leaves the previous media intact.
pub fn replace_items_for_post(conn: &Connection, post_id: &str, items: &[MediaItem]) -> AppResult<()> {
    conn.execute("SAVEPOINT replace_media", [])?;
    let result = (|| -> AppResult<()> {
        conn.execute("DELETE FROM media_items WHERE post_id = ?1", [post_id])?;
        for item in items {
            insert_item(conn, item)?;
            for variant in &item.variants {
                insert_variant(conn, variant)?;
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute("RELEASE replace_media", [])?;
            Ok(())
        }
        Err(err) => {
            conn.execute("ROLLBACK TO replace_media", [])?;
            conn.execute("RELEASE replace_media", [])?;
            Err(err)
        }
    }
}

/// Load all media items for a post, including their variants.
pub fn list_items_for_post(conn: &Connection, post_id: &str) -> AppResult<Vec<MediaItem>> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM media_items WHERE post_id = ?1 ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([post_id], |row| Ok(map_item(row)))?;
    let mut items = Vec::new();
    for row in rows {
        let mut item = row??;
        item.variants = list_variants(conn, &item.id)?;
        items.push(item);
    }
    Ok(items)
}

/// Load all variants for a media item.
pub fn list_variants(conn: &Connection, media_item_id: &str) -> AppResult<Vec<MediaVariant>> {
    let sql = format!("SELECT {VARIANT_COLUMNS} FROM media_variants WHERE media_item_id = ?1 ORDER BY is_original DESC, width DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([media_item_id], |row| Ok(map_variant(row)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Number of media items stored.
pub fn count_items(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM media_items", [], |row| row.get(0))?)
}

/// Number of variants stored.
pub fn count_variants(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM media_variants", [], |row| row.get(0))?)
}
