//! ddamr-index: indexador simple y cancelable (fase 1).
//!
//! Recorre un directorio en profundidad, en streaming, y guarda
//! cada archivo en la DB en lotes de 500. La cola de jobs con
//! prioridades llegará en fase 2; aquí el contrato es mínimo:
//! contar, no abortar por un archivo roto y poder cancelarse.

use ddamr_core::{AppError, Result, SafePath};
use ddamr_db::insert_files_batch;
use ddamr_fs::{FileEntry, walk};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Contadores del escaneo. `errors` son archivos que no se
/// pudieron leer o insertar: se cuentan y se registran, nunca
/// abortan el escaneo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub scanned: usize,
    pub indexed: usize,
    pub errors: usize,
}

/// Indexador cancelable. `cancel()` puede llamarse desde otro hilo;
/// `scan_dir` lo comprueba entre directorios y entre lotes.
#[derive(Debug, Clone)]
pub struct Indexer {
    cancel: Arc<AtomicBool>,
}

impl Default for Indexer {
    fn default() -> Self {
        Self::new()
    }
}

impl Indexer {
    pub fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Escanea `root` recursivamente y guarda los archivos en la
    /// DB de `db_path` (se crea si no existe). Solo archivos: los
    /// directorios se recorren pero no se indexan como filas.
    /// Si se cancela, devuelve `Err(AppError::Cancelled)`; lo ya
    /// insertado queda guardado (cada lote es una transacción).
    pub fn scan_dir(&self, root: &SafePath, db_path: &Path) -> Result<IndexStats> {
        let mut conn = ddamr_db::open(db_path)?;
        let mut stats = IndexStats::default();
        let mut batch: Vec<FileEntry> = Vec::with_capacity(500);
        // Pila de directorios por visitar (DFS, sin recursión).
        let mut stack: Vec<SafePath> = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            if self.cancelled() {
                return Err(AppError::Cancelled);
            }
            let entries = match walk(&dir) {
                Ok(it) => it,
                Err(e) => {
                    stats.errors += 1;
                    eprintln!("index: no se pudo leer {}: {e}", dir.path().display());
                    continue;
                }
            };
            for entry in entries {
                if self.cancelled() {
                    return Err(AppError::Cancelled);
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        stats.errors += 1;
                        eprintln!("index: entrada ilegible en {}: {e}", dir.path().display());
                        continue;
                    }
                };
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with('.') {
                        continue;
                    }
                    match dir.join(std::path::Path::new(&name)) {
                        Ok(child) => stack.push(child),
                        Err(_) => continue, // symlink que escapa: se ignora
                    }
                    continue;
                }
                stats.scanned += 1;
                match FileEntry::from_dir_entry(root.root(), &entry) {
                    Ok(mut fe) => {
                        // Fecha de creación desde EXIF si la hay:
                        // es la fecha que el usuario ve y por la que
                        // se archivará en fase 2.
                        let meta = ddamr_metadata::read_basic(&fe.path);
                        if let Some(created) = meta.created_at {
                            fe.created_at = Some(created);
                        }
                        fe.has_keywords = meta.has_keywords;
                        fe.has_location = meta.has_location;
                        fe.has_description = meta.has_description;
                        batch.push(fe);
                    }
                    Err(e) => {
                        stats.errors += 1;
                        eprintln!("index: no se pudo leer {}: {e}", entry.path().display());
                    }
                }
                if batch.len() >= 500 {
                    match insert_files_batch(&mut conn, &batch) {
                        Ok(n) => stats.indexed += n,
                        Err(e) => {
                            stats.errors += batch.len();
                            eprintln!("index: lote no insertado: {e}");
                        }
                    }
                    batch.clear();
                }
            }
        }
        if !batch.is_empty() {
            match insert_files_batch(&mut conn, &batch) {
                Ok(n) => stats.indexed += n,
                Err(e) => {
                    stats.errors += batch.len();
                    eprintln!("index: lote no insertado: {e}");
                }
            }
        }
        Ok(stats)
    }
}

/// Ruta por defecto de la DB: `$DDAMR_DB` o `~/.cache/ddamr/ddamr.db`.
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("DDAMR_DB")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".cache")
        .join("ddamr")
        .join("ddamr.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn scan_dir_indexes_files_and_counts_errors() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("a.jpg"), b"x").unwrap();
        std::fs::write(root.path().join("sub/b.mp3"), b"x").unwrap();
        std::fs::write(root.path().join("c.txt"), b"x").unwrap();
        let safe = SafePath::from_root(root.path()).unwrap();
        // La DB va fuera del árbol escaneado: si viviera dentro,
        // el propio idx.db (y su -wal/-shm) se contarían como archivos.
        let db_dir = TempDir::new().unwrap();
        let db = db_dir.path().join("idx.db");
        let stats = Indexer::new().scan_dir(&safe, &db).unwrap();
        assert_eq!(stats.scanned, 3);
        assert_eq!(stats.indexed, 3);
        assert_eq!(stats.errors, 0);

        let conn = ddamr_db::open(&db).unwrap();
        let page = ddamr_db::list_files_paginated(
            &conn,
            "",
            ddamr_core::SortKey::Name,
            ddamr_core::SortOrder::Asc,
            ddamr_core::Page::first(),
        )
        .unwrap();
        assert_eq!(page.total, 3);
    }

    #[test]
    fn scan_dir_cancelled_before_start_returns_cancelled() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("a.jpg"), b"x").unwrap();
        let safe = SafePath::from_root(root.path()).unwrap();
        let indexer = Indexer::new();
        indexer.cancel();
        let err = indexer
            .scan_dir(&safe, &root.path().join("idx.db"))
            .unwrap_err();
        assert!(matches!(err, AppError::Cancelled));
    }
}
