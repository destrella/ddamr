//! ddamr-fs: recorrido en streaming y operaciones seguras.
//!
//! Nada aquí materializa una carpeta entera en un `Vec`: `walk` es
//! un iterador perezoso. Las mutaciones (mover/archivar) irán por el
//! patrón respaldo → copia → verificar → rename atómico (fase 2).
use ddamr_core::{Result, SafePath};
use std::fs::DirEntry;

/// Recorrido no recursivo en streaming de un directorio.
/// Devuelve un iterador; el llamador decide cuántos lotes consume.
/// Los errores de una entrada concreta no abortan el resto.
pub fn walk(dir: &SafePath) -> Result<impl Iterator<Item = Result<DirEntry>>> {
    let rd = std::fs::read_dir(dir.path()).map_err(|e| ddamr_core::AppError::Io {
        path: dir.path().to_path_buf(),
        source: e,
    })?;
    Ok(rd.map(|entry| entry.map_err(ddamr_core::AppError::IoPlain)))
}
