//! Key/value access to the `settings` table.
//!
//! This module only stores opaque string values; interpreting them (JSON,
//! validation, defaults) is the job of the settings service. Keeping the raw
//! storage dumb makes the schema stable as settings evolve.

use rusqlite::{params, Connection};

use crate::database::row;
use crate::errors::AppResult;

/// Read a value, or `None` when the key is absent.
pub fn get(conn: &Connection, key: &str) -> AppResult<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM settings WHERE key = ?1")?;
    let mut rows = stmt.query_map([key], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(value) => Ok(Some(value?)),
        None => Ok(None),
    }
}

/// Insert or overwrite a value.
pub fn set(conn: &Connection, key: &str, value: &str) -> AppResult<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, row::timestamp_to_sql(crate::models::common::now())],
    )?;
    Ok(())
}

/// All key/value pairs, ordered by key.
pub fn all(conn: &Connection) -> AppResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Delete a key, returning whether it existed.
pub fn delete(conn: &Connection, key: &str) -> AppResult<bool> {
    let affected = conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(affected > 0)
}
