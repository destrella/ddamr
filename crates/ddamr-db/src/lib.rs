//! ddamr-db: SQLite con rusqlite (bundled, SQLite 3.54.1).
//!
//! Un solo escritor, WAL, `foreign_keys = ON` en cada conexión.
//! Migraciones numeradas en `PRAGMA user_version`.
use ddamr_core::{AppError, Result};
use rusqlite::Connection;
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 1;

/// Abre (o crea) la DB aplicando los pragmas de la sección 4 del diseño.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).map_err(|e| AppError::Database(e.to_string()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| AppError::Database(e.to_string()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| AppError::Database(e.to_string()))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| AppError::Database(e.to_string()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(|e| AppError::Database(e.to_string()))?;
    migrate(&conn)?;
    Ok(conn)
}

/// Migración 001: esquema mínimo (files/tags). Las siguientes van
/// en `match version { 1 => migrate_002, ... }` con user_version.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| AppError::Database(e.to_string()))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL, ext TEXT,
            size_bytes INTEGER NOT NULL,
            created_at INTEGER, modified_at INTEGER,
            kind TEXT NOT NULL,
            hash_blake3 TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_files_hash ON files(hash_blake3)
            WHERE hash_blake3 IS NOT NULL;
        CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            usage_count INTEGER NOT NULL DEFAULT 0
        );
        PRAGMA user_version = 1;",
    )
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}
