//! ddamr-db: SQLite con rusqlite (bundled, SQLite 3.54.1).
//!
//! Un solo escritor, WAL, `foreign_keys = ON` en cada conexión.
//! Migraciones numeradas en `PRAGMA user_version`.
pub mod repos;

use ddamr_core::{AppError, MediaKind, Page, PageResult, Result, SortKey, SortOrder};
use ddamr_fs::FileEntry;
use rusqlite::{Connection, params};
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 2;

/// Una fila de la tabla `files`, tal como se lee de la DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFile {
    pub path: String,
    pub name: String,
    pub ext: String,
    pub size_bytes: u64,
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub kind: MediaKind,
}

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

/// Migraciones numeradas en `PRAGMA user_version`. Cada una corre
/// solo si la versión actual es menor, en su propia transacción.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| AppError::Database(e.to_string()))?;
    if version < 1 {
        migrate_001(conn)?;
    }
    if version < 2 {
        migrate_002(conn)?;
    }
    Ok(())
}

/// Migración 001: esquema mínimo (files/tags).
fn migrate_001(conn: &Connection) -> Result<()> {
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
        CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);
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

/// Añade una columna solo si no existe (SQLite no tiene
/// `ADD COLUMN IF NOT EXISTS`): se consulta `PRAGMA table_info`.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    coltype: &str,
) -> Result<()> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| AppError::Database(e.to_string()))?;
    let columns = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| AppError::Database(e.to_string()))?
        .collect::<std::result::Result<Vec<String>, _>>()
        .map_err(|e| AppError::Database(e.to_string()))?;
    let exists = columns.iter().any(|n| n == column);
    if !exists {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {coltype};"
        ))
        .map_err(|e| AppError::Database(e.to_string()))?;
    }
    Ok(())
}

/// Migración 002: ubicaciones, regiones, asociaciones, la tabla
/// `file_tags` (la 001 creó `tags` pero no su tabla de unión) y
/// las columnas de duplicados/dimensiones en `files`.
fn migrate_002(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_tags (
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY (file_id, tag_id)
        );
        CREATE TABLE IF NOT EXISTS locations (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            lat REAL NOT NULL, lon REAL NOT NULL,
            lat_r REAL NOT NULL, lon_r REAL NOT NULL,
            city TEXT, state TEXT, country TEXT,
            usage_count INTEGER NOT NULL DEFAULT 0,
            UNIQUE (lat_r, lon_r)
        );
        CREATE TABLE IF NOT EXISTS regions (
            id INTEGER PRIMARY KEY,
            file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            x REAL NOT NULL, y REAL NOT NULL, w REAL NOT NULL, h REAL NOT NULL,
            UNIQUE (file_id, name, x, y, w, h)
        );
        CREATE TABLE IF NOT EXISTS file_locations (
            file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
            location_id INTEGER NOT NULL REFERENCES locations(id)
        );
        CREATE TABLE IF NOT EXISTS associations (
            id INTEGER PRIMARY KEY,
            pattern TEXT NOT NULL,
            suggestions TEXT NOT NULL
        );",
    )
    .map_err(|e| AppError::Database(e.to_string()))?;
    for (column, coltype) in [
        ("dhash_image", "INTEGER"),
        ("dhash_video", "TEXT"),
        ("width", "INTEGER"),
        ("height", "INTEGER"),
        ("duration_ms", "INTEGER"),
    ] {
        add_column_if_missing(conn, "files", column, coltype)?;
    }
    conn.execute_batch("PRAGMA user_version = 2;")
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Hashes y dimensiones calculados para una fila. Solo se tocan
/// las columnas que se pasan como `Some`.
#[derive(Debug, Clone, Default)]
pub struct FileHashes {
    pub hash_blake3: Option<String>,
    pub dhash_image: Option<i64>,
    pub dhash_video: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<i64>,
}

/// Actualiza los hashes/dimensiones de una fila (ruta relativa).
pub fn update_file_hashes(conn: &Connection, rel_path: &str, h: &FileHashes) -> Result<()> {
    conn.execute(
        "UPDATE files SET
            hash_blake3 = COALESCE(?2, hash_blake3),
            dhash_image = COALESCE(?3, dhash_image),
            dhash_video = COALESCE(?4, dhash_video),
            width = COALESCE(?5, width),
            height = COALESCE(?6, height),
            duration_ms = COALESCE(?7, duration_ms)
         WHERE path = ?1",
        params![
            rel_path,
            h.hash_blake3,
            h.dhash_image,
            h.dhash_video,
            h.width.map(|w| w as i64),
            h.height.map(|h| h as i64),
            h.duration_ms,
        ],
    )
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Inserta (o reemplaza) entradas en una sola transacción, en
/// lotes de 500 sentencias preparadas. `path` se guarda relativo
/// a la raíz, nunca absoluto. Devuelve cuántas se insertaron.
pub fn insert_files_batch(conn: &mut Connection, entries: &[FileEntry]) -> Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }
    let tx = conn
        .transaction()
        .map_err(|e| AppError::Database(e.to_string()))?;
    let mut inserted = 0usize;
    {
        // ON CONFLICT con COALESCE: reindexar no borra los hashes
        // ni dimensiones ya calculados (serían NULL en la entrada).
        let mut stmt = tx
            .prepare(
                "INSERT INTO files
                 (path, name, ext, size_bytes, created_at, modified_at, kind,
                  hash_blake3, dhash_image, dhash_video, width, height, duration_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(path) DO UPDATE SET
                   name = excluded.name, ext = excluded.ext,
                   size_bytes = excluded.size_bytes,
                   created_at = excluded.created_at,
                   modified_at = excluded.modified_at, kind = excluded.kind,
                   hash_blake3 = COALESCE(excluded.hash_blake3, files.hash_blake3),
                   dhash_image = COALESCE(excluded.dhash_image, files.dhash_image),
                   dhash_video = COALESCE(excluded.dhash_video, files.dhash_video),
                   width = COALESCE(excluded.width, files.width),
                   height = COALESCE(excluded.height, files.height),
                   duration_ms = COALESCE(excluded.duration_ms, files.duration_ms)",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;
        for chunk in entries.chunks(500) {
            for entry in chunk {
                stmt.execute(params![
                    entry.path.relative().to_string_lossy().as_ref(),
                    entry.name,
                    entry.ext,
                    entry.size_bytes as i64,
                    entry.created_at,
                    entry.modified_at,
                    entry.kind.as_str(),
                    entry.hash_blake3,
                    entry.dhash_image,
                    entry.dhash_video,
                    entry.width.map(|w| w as i64),
                    entry.height.map(|h| h as i64),
                    entry.duration_ms,
                ])
                .map_err(|e| AppError::Database(e.to_string()))?;
                inserted += 1;
            }
        }
    }
    tx.commit().map_err(|e| AppError::Database(e.to_string()))?;
    Ok(inserted)
}

/// Columna y dirección de `ORDER BY`, elegidas solo desde el enum:
/// ningún valor del usuario llega jamás a la sentencia SQL.
fn order_by(sort: SortKey, order: SortOrder) -> &'static str {
    match (sort, order) {
        (SortKey::Name, SortOrder::Asc) => "name ASC",
        (SortKey::Name, SortOrder::Desc) => "name DESC",
        (SortKey::Extension, SortOrder::Asc) => "ext ASC, name ASC",
        (SortKey::Extension, SortOrder::Desc) => "ext DESC, name ASC",
        (SortKey::Size, SortOrder::Asc) => "size_bytes ASC",
        (SortKey::Size, SortOrder::Desc) => "size_bytes DESC",
        (SortKey::Date, SortOrder::Asc) => "modified_at ASC",
        (SortKey::Date, SortOrder::Desc) => "modified_at DESC",
    }
}

/// Lista las filas cuya ruta empieza por `dir_prefix` (ruta relativa,
/// "" = todas), ordenadas y paginadas. `LIMIT`/`OFFSET` y el prefijo
/// van siempre como parámetros, nunca interpolados.
pub fn list_files_paginated(
    conn: &Connection,
    dir_prefix: &str,
    sort: SortKey,
    order: SortOrder,
    page: Page,
) -> Result<PageResult<DbFile>> {
    let like = format!("{dir_prefix}%");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path LIKE ?1",
            params![like],
            |r| r.get(0),
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
    // ORDER BY no admite parámetros: el fragmento sale del enum.
    let sql = format!(
        "SELECT path, name, ext, size_bytes, created_at, modified_at, kind
         FROM files WHERE path LIKE ?1 ORDER BY {} LIMIT ?2 OFFSET ?3",
        order_by(sort, order)
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows = stmt
        .query_map(params![like, page.limit as i64, page.offset as i64], |r| {
            let kind: String = r.get(6)?;
            Ok(DbFile {
                path: r.get(0)?,
                name: r.get(1)?,
                ext: r.get(2)?,
                size_bytes: r.get::<_, i64>(3)? as u64,
                created_at: r.get(4)?,
                modified_at: r.get(5)?,
                kind: MediaKind::from_db_str(&kind),
            })
        })
        .map_err(|e| AppError::Database(e.to_string()))?;
    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| AppError::Database(e.to_string()))?);
    }
    Ok(PageResult::new(items, total as usize, page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_entries(root: &std::path::Path, n: usize) -> Vec<FileEntry> {
        let mut out = Vec::new();
        for i in 0..n {
            let name = format!("f-{i:03}.jpg");
            let p = root.join(&name);
            std::fs::write(&p, b"x").unwrap();
            let found = std::fs::read_dir(root)
                .unwrap()
                .find(|e| {
                    e.as_ref()
                        .is_ok_and(|e| e.file_name().to_string_lossy() == name)
                })
                .unwrap()
                .unwrap();
            out.push(FileEntry::from_dir_entry(root, &found).unwrap());
        }
        out
    }

    #[test]
    fn batch_insert_and_paginated_list() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let mut conn = open(&db_path).unwrap();
        let entries = make_entries(dir.path(), 250);
        assert_eq!(insert_files_batch(&mut conn, &entries).unwrap(), 250);
        // Reinsertar no duplica: INSERT OR REPLACE sobre path UNIQUE.
        assert_eq!(insert_files_batch(&mut conn, &entries).unwrap(), 250);

        let page0 =
            list_files_paginated(&conn, "", SortKey::Name, SortOrder::Asc, Page::first()).unwrap();
        assert_eq!(page0.items.len(), 200);
        assert_eq!(page0.total, 250);
        assert_eq!(page0.items[0].name, "f-000.jpg");
        let page1 =
            list_files_paginated(&conn, "", SortKey::Name, SortOrder::Asc, Page::at(1)).unwrap();
        assert_eq!(page1.items.len(), 50);
        assert_eq!(page1.items[0].name, "f-200.jpg");
    }
}
