//! CRUD for the `creators` table.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::database::row;
use crate::errors::AppResult;
use crate::models::common::now;
use crate::models::{Creator, Platform};

const COLUMNS: &str = "id, platform, platform_id, handle, display_name, avatar_url, description, \
     verified, post_count, profile_url, metadata, created_at, updated_at";

fn map(row: &Row<'_>) -> AppResult<Creator> {
    Ok(Creator {
        id: row::text(row, 0)?,
        platform: row::parse_enum(&row::text(row, 1)?, "platform")?,
        platform_id: row::text(row, 2)?,
        handle: row::opt_text(row, 3)?,
        display_name: row::text(row, 4)?,
        avatar_url: row::opt_text(row, 5)?,
        description: row::opt_text(row, 6)?,
        verified: row::bool_val(row, 7)?,
        post_count: row::opt_uint(row, 8)?,
        profile_url: row::text(row, 9)?,
        metadata: row::metadata(row, 10)?,
        created_at: row::timestamp(row, 11, "creators.created_at")?,
        updated_at: row::timestamp(row, 12, "creators.updated_at")?,
    })
}

/// Insert or update a creator keyed by `(platform, platform_id)`.
///
/// Returns the id actually persisted. When a creator with the same platform
/// identity already exists, its existing local id is kept and the supplied id
/// is ignored, so callers cannot create duplicates for one profile.
pub fn upsert(conn: &Connection, creator: &Creator) -> AppResult<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM creators WHERE platform = ?1 AND platform_id = ?2",
            params![creator.platform.as_str(), creator.platform_id],
            |r| r.get(0),
        )
        .optional()?;

    let id = match existing {
        Some(id) => {
            // `COALESCE` on the post count, because a resolution that did not
            // read the profile header has nothing to say about it — and the
            // identity screen and the listing resolve the same creator
            // separately, so the one that read less would otherwise erase what
            // the other found. The count is the denominator the listing's
            // completion status is measured against; losing it silently turns
            // "the source stopped short" into "everything was read".
            conn.execute(
                "UPDATE creators SET handle = ?2, display_name = ?3, avatar_url = ?4, \
                 description = ?5, verified = ?6, post_count = COALESCE(?7, post_count), \
                 profile_url = ?8, metadata = ?9, updated_at = ?10 WHERE id = ?1",
                params![
                    id,
                    creator.handle,
                    creator.display_name,
                    creator.avatar_url,
                    creator.description,
                    creator.verified as i64,
                    creator.post_count.map(|v| v as i64),
                    creator.profile_url,
                    row::metadata_to_sql(&creator.metadata)?,
                    row::timestamp_to_sql(now()),
                ],
            )?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO creators (id, platform, platform_id, handle, display_name, \
                 avatar_url, description, verified, post_count, profile_url, metadata, \
                 created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    creator.id,
                    creator.platform.as_str(),
                    creator.platform_id,
                    creator.handle,
                    creator.display_name,
                    creator.avatar_url,
                    creator.description,
                    creator.verified as i64,
                    creator.post_count.map(|v| v as i64),
                    creator.profile_url,
                    row::metadata_to_sql(&creator.metadata)?,
                    row::storing_timestamp(creator.created_at),
                    row::storing_timestamp(creator.updated_at),
                ],
            )?;
            creator.id.clone()
        }
    };
    Ok(id)
}

pub fn get(conn: &Connection, id: &str) -> AppResult<Option<Creator>> {
    let sql = format!("SELECT {COLUMNS} FROM creators WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([id], |row| Ok(map(row)))?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

/// Look up a creator by its platform identity.
pub fn get_by_platform_id(
    conn: &Connection,
    platform: Platform,
    platform_id: &str,
) -> AppResult<Option<Creator>> {
    let sql = format!("SELECT {COLUMNS} FROM creators WHERE platform = ?1 AND platform_id = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![platform.as_str(), platform_id], |row| {
        Ok(map(row))
    })?;
    match rows.next() {
        Some(result) => Ok(Some(result??)),
        None => Ok(None),
    }
}

pub fn list(conn: &Connection, limit: i64, offset: i64) -> AppResult<Vec<Creator>> {
    let sql = format!("SELECT {COLUMNS} FROM creators ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![limit, offset], |row| Ok(map(row)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

pub fn count(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM creators", [], |row| row.get(0))?)
}

pub fn delete(conn: &Connection, id: &str) -> AppResult<bool> {
    let affected = conn.execute("DELETE FROM creators WHERE id = ?1", [id])?;
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    fn creator(post_count: Option<u32>) -> Creator {
        let mut creator = Creator::new(
            Platform::Douyin,
            "MS4wLjABAAAAexample",
            "a creator",
            "https://www.douyin.com/user/MS4wLjABAAAAexample",
        );
        creator.post_count = post_count;
        creator
    }

    #[test]
    fn a_resolution_that_read_no_post_count_does_not_erase_the_one_on_file() {
        // The identity screen and the listing resolve the same profile through
        // separate renders, and read different amounts from them: the listing's
        // render frequently does not capture the works tab at all. That count is
        // the denominator the listing's own completion status is measured
        // against, so letting the thinner resolution blank it turns "the source
        // stopped short" into "everything was read".
        let db = Database::open_in_memory().expect("a database");
        db.with_conn(|conn| {
            let id = upsert(conn, &creator(Some(50)))?;

            upsert(conn, &creator(None))?;
            let stored = get(conn, &id)?.expect("the creator is stored");
            assert_eq!(
                stored.post_count,
                Some(50),
                "an absent count must not erase a known one"
            );

            // A count that was actually read still wins — including a real zero,
            // which is a fact about the profile rather than an absence.
            upsert(conn, &creator(Some(0)))?;
            let stored = get(conn, &id)?.expect("the creator is stored");
            assert_eq!(stored.post_count, Some(0));

            upsert(conn, &creator(Some(54)))?;
            let stored = get(conn, &id)?.expect("the creator is stored");
            assert_eq!(stored.post_count, Some(54));
            Ok(())
        })
        .expect("the round trips succeed");
    }
}
