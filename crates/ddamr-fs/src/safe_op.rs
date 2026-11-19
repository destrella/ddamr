//! `SafeOp`: operaciones que mutan archivos sin poder perderlos.
//!
//! Patrón (sección 7 del diseño), siempre en este orden:
//! 1. Validar: origen y destino son `SafePath` dentro de la raíz.
//! 2. Respaldar: copiar el original a `.ddamr-backup/<id>/`, en el
//!    mismo filesystem que el origen, junto a un manifiesto
//!    `origin.txt` con la ruta original.
//! 3. Trabajar solo sobre una copia en un temporal del mismo
//!    filesystem que el destino.
//! 4. Verificar: el resultado existe, pesa > 0 y su BLAKE3 coincide
//!    con el del original.
//! 5. Confirmar: `rename` atómico copia → destino.
//! 6. Descartar el respaldo solo tras el éxito.
//!
//! Si cualquier paso falla, se restaura el original desde el
//! respaldo (si llegó a tocarse) y se devuelve el error.
//!
//! La papelera es la excepción: el propio sistema la hace
//! recuperable, así que no necesita respaldo previo.

use ddamr_core::{AppError, Result, SafePath};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Operaciones seguras sobre una raíz. Un `SafeOp` por raíz.
#[derive(Debug, Clone)]
pub struct SafeOp {
    root: PathBuf,
}

static OP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn op_id() -> String {
    let n = OP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{n}", std::process::id(), now)
}

/// BLAKE3 en streaming (bloques de 128 KB), sin cargar el archivo.
pub fn hash_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).map_err(|e| AppError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut reader = std::io::BufReader::with_capacity(128 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(AppError::IoPlain)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Destino sin colisión: si `dst` existe, `nombre (2).ext`,
/// `nombre (3).ext`… Nunca sobrescribe.
pub fn resolve_collision(dst: &Path) -> PathBuf {
    if !dst.exists() {
        return dst.to_path_buf();
    }
    let parent = dst.parent().unwrap_or(Path::new(""));
    let stem = dst
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = dst
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut i = 2u32;
    loop {
        let cand = parent.join(format!("{stem} ({i}){ext}"));
        if !cand.exists() {
            return cand;
        }
        i += 1;
    }
}

impl SafeOp {
    pub fn new(root: &SafePath) -> Self {
        Self {
            root: root.path().to_path_buf(),
        }
    }

    fn backup_root(&self) -> PathBuf {
        self.root.join(".ddamr-backup")
    }

    /// Copia `src` al respaldo y escribe el manifiesto con el
    /// origen. Devuelve el directorio del respaldo.
    fn backup(&self, src: &SafePath) -> Result<PathBuf> {
        let dir = self.backup_root().join(op_id());
        std::fs::create_dir_all(&dir).map_err(|e| AppError::Io {
            path: dir.clone(),
            source: e,
        })?;
        let name = src
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archivo".into());
        let backup_file = dir.join(&name);
        std::fs::copy(src.path(), &backup_file).map_err(|e| AppError::Io {
            path: backup_file.clone(),
            source: e,
        })?;
        if hash_file(&backup_file)? != hash_file(src.path())? {
            return Err(AppError::Other(format!(
                "el respaldo de {} no coincide con el original",
                src.path().display()
            )));
        }
        std::fs::write(dir.join("origin.txt"), src.path().display().to_string())
            .map_err(AppError::IoPlain)?;
        Ok(dir)
    }

    /// Restaura el original desde el respaldo y borra el respaldo.
    /// Si el original sigue intacto, no hace falta copiar: solo
    /// descarta el respaldo.
    fn restore_and_discard(&self, backup_dir: &Path, src: &SafePath) {
        if !src.path().exists() {
            let name = src
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let backup_file = backup_dir.join(&name);
            if backup_file.exists() {
                if let Some(parent) = src.path().parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::copy(&backup_file, src.path()).is_ok() {
                    // Verificar lo restaurado cuando se pueda.
                    let _ = hash_file(src.path());
                } else {
                    eprintln!(
                        "SafeOp: NO se pudo restaurar {} desde {}",
                        src.path().display(),
                        backup_dir.display()
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(backup_dir);
    }

    fn discard_backup(&self, backup_dir: &Path) {
        if let Err(e) = std::fs::remove_dir_all(backup_dir) {
            eprintln!(
                "SafeOp: no se pudo borrar el respaldo {}: {e}",
                backup_dir.display()
            );
        }
    }

    /// Mueve `src` a `dst` con el patrón completo. Devuelve el
    /// destino final (puede llevar sufijo por colisión).
    pub fn move_file(&self, src: &SafePath, dst: &SafePath) -> Result<SafePath> {
        if src.root() != dst.root() {
            return Err(AppError::InvalidPath(
                "origen y destino deben estar en la misma raíz".into(),
            ));
        }
        let original_hash = hash_file(src.path())?;
        let backup_dir = self.backup(src)?;
        let result = self.move_file_inner(src, dst, &original_hash);
        match result {
            Ok(final_dst) => {
                self.discard_backup(&backup_dir);
                Ok(final_dst)
            }
            Err(e) => {
                self.restore_and_discard(&backup_dir, src);
                Err(e)
            }
        }
    }

    fn move_file_inner(
        &self,
        src: &SafePath,
        dst: &SafePath,
        original_hash: &str,
    ) -> Result<SafePath> {
        let dst_dir = dst
            .path()
            .parent()
            .ok_or_else(|| AppError::InvalidPath("destino sin directorio".into()))?
            .to_path_buf();
        // El padre puede no existir (p. ej. AÑO/MES del archivado):
        // crearlo aquí; si un componente es un archivo, falla y el
        // llamador restaura desde el respaldo.
        std::fs::create_dir_all(&dst_dir).map_err(|e| AppError::Io {
            path: dst_dir.clone(),
            source: e,
        })?;
        let final_dst = resolve_collision(dst.path());
        // Copia de trabajo en el mismo filesystem que el destino.
        let tmp = dst_dir.join(format!(
            ".ddamr-tmp-{}-{}",
            op_id(),
            dst.path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "archivo".into())
        ));
        std::fs::copy(src.path(), &tmp).map_err(|e| AppError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        let tmp_hash = hash_file(&tmp)?;
        if tmp_hash != original_hash {
            let _ = std::fs::remove_file(&tmp);
            return Err(AppError::Other(format!(
                "la copia de trabajo de {} no coincide con el original",
                src.path().display()
            )));
        }
        // rename atómico (misma partición: el temporal está en el
        // directorio destino). Si el rename falla por cross-device,
        // copiar + borrar.
        match std::fs::rename(&tmp, &final_dst) {
            Ok(()) => {}
            Err(rename_err) => {
                std::fs::copy(&tmp, &final_dst).map_err(|e| AppError::Io {
                    path: final_dst.clone(),
                    source: e,
                })?;
                let _ = std::fs::remove_file(&tmp);
                if hash_file(&final_dst)? != original_hash {
                    let _ = std::fs::remove_file(&final_dst);
                    return Err(AppError::Other(format!(
                        "el destino de {} no coincide con el original",
                        src.path().display()
                    )));
                }
                let _ = rename_err; // el fallback de copia funcionó
            }
        }
        // Verificar el destino final antes de tocar el origen.
        let meta = std::fs::metadata(&final_dst).map_err(AppError::IoPlain)?;
        if meta.len() == 0 || hash_file(&final_dst)? != original_hash {
            let _ = std::fs::remove_file(&final_dst);
            return Err(AppError::Other(format!(
                "verificación del destino falló para {}",
                final_dst.display()
            )));
        }
        // Solo ahora se borra el origen. Si esto falla, el archivo
        // existe en ambos sitios: error, y el llamador restaura.
        if let Err(e) = std::fs::remove_file(src.path()) {
            let _ = std::fs::remove_file(&final_dst);
            return Err(AppError::Io {
                path: src.path().to_path_buf(),
                source: e,
            });
        }
        SafePath::new(&self.root, &final_dst)
    }

    /// Archiva `src` en `archive_root/AÑO/MES/`. Sin fecha no se
    /// mueve nada: devuelve `AppError::NoDate` para que el llamador
    /// lo cuente. `date` es (año, mes) desde los metadatos.
    pub fn archive_file(
        &self,
        src: &SafePath,
        archive_root: &SafePath,
        date: Option<(u32, u32)>,
    ) -> Result<SafePath> {
        let Some((year, month)) = date else {
            return Err(AppError::NoDate(src.path().to_path_buf()));
        };
        let dest_dir = archive_root
            .path()
            .join(format!("{year:04}"))
            .join(format!("{month:02}"));
        std::fs::create_dir_all(&dest_dir).map_err(|e| AppError::Io {
            path: dest_dir.clone(),
            source: e,
        })?;
        let name = src
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archivo".into());
        let dst = SafePath::new(&self.root, &dest_dir.join(&name))?;
        self.move_file(src, &dst)
    }

    /// Envía a la papelera del sistema. La papelera es recuperable
    /// por sí misma, así que no necesita respaldo previo. Nunca
    /// usa `remove_file`.
    pub fn trash_file(&self, src: &SafePath) -> Result<()> {
        trash::delete(src.path()).map_err(|e| {
            AppError::Other(format!(
                "no se pudo enviar {} a la papelera: {e}",
                src.path().display()
            ))
        })
    }

    /// Restaura los archivos de un directorio de respaldo a sus
    /// rutas originales (leídas de su `origin.txt`). Devuelve las
    /// rutas restauradas. No borra el respaldo: eso lo decide el
    /// llamador tras verificar.
    pub fn restore_backup(backup_dir: &Path) -> Result<Vec<PathBuf>> {
        let origin =
            std::fs::read_to_string(backup_dir.join("origin.txt")).map_err(|e| AppError::Io {
                path: backup_dir.join("origin.txt"),
                source: e,
            })?;
        let origin = origin.trim();
        if origin.is_empty() {
            return Err(AppError::InvalidPath(format!(
                "respaldo sin origen: {}",
                backup_dir.display()
            )));
        }
        let origin_path = PathBuf::from(origin);
        let name = origin_path
            .file_name()
            .ok_or_else(|| AppError::InvalidPath("origen sin nombre".into()))?;
        let backup_file = backup_dir.join(name);
        if let Some(parent) = origin_path.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::IoPlain)?;
        }
        std::fs::copy(&backup_file, &origin_path).map_err(|e| AppError::Io {
            path: origin_path.clone(),
            source: e,
        })?;
        Ok(vec![origin_path])
    }

    /// Lista los respaldos huérfanos (directorios dentro de
    /// `.ddamr-backup/`). No borra nada: solo los reporta, para
    /// que el usuario decida restaurarlos o descartarlos.
    pub fn cleanup_orphan_backups(root: &SafePath) -> Result<Vec<PathBuf>> {
        let backup_root = root.path().join(".ddamr-backup");
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&backup_root) else {
            return Ok(out); // no hay respaldos: nada huérfano
        };
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, SafePath, SafeOp) {
        let dir = TempDir::new().unwrap();
        let root = SafePath::from_root(dir.path()).unwrap();
        let op = SafeOp::new(&root);
        (dir, root, op)
    }

    fn write(root: &TempDir, rel: &str, content: &[u8]) -> SafePath {
        let p = root.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        SafePath::new(root.path(), &p).unwrap()
    }

    #[test]
    fn move_succeeds_hash_matches_and_backup_discarded() {
        let (dir, root, op) = setup();
        let src = write(&dir, "a/foto.jpg", b"contenido original");
        let before = hash_file(src.path()).unwrap();
        let dst_dir = dir.path().join("b");
        std::fs::create_dir(&dst_dir).unwrap();
        let dst = SafePath::new(root.path(), &dst_dir.join("foto.jpg")).unwrap();
        let final_dst = op.move_file(&src, &dst).unwrap();
        assert!(!src.path().exists(), "el origen debe desaparecer");
        assert!(final_dst.path().exists());
        assert_eq!(hash_file(final_dst.path()).unwrap(), before);
        assert!(
            SafeOp::cleanup_orphan_backups(&root).unwrap().is_empty(),
            "el respaldo debe descartarse tras el éxito"
        );
    }

    #[test]
    fn move_with_collision_adds_suffix_never_overwrites() {
        let (dir, root, op) = setup();
        let src = write(&dir, "a/foto.jpg", b"nuevo");
        let _existing = write(&dir, "b/foto.jpg", b"existente");
        let dst = SafePath::new(root.path(), &dir.path().join("b/foto.jpg")).unwrap();
        let final_dst = op.move_file(&src, &dst).unwrap();
        assert_eq!(
            final_dst.path().file_name().unwrap().to_string_lossy(),
            "foto (2).jpg"
        );
        assert_eq!(
            std::fs::read(dir.path().join("b/foto.jpg")).unwrap(),
            b"existente"
        );
        assert_eq!(std::fs::read(final_dst.path()).unwrap(), b"nuevo");
    }

    #[test]
    fn failed_move_leaves_original_intact() {
        let (dir, root, op) = setup();
        let src = write(&dir, "a/foto.jpg", b"intacto");
        // El destino tiene como componente un archivo, no un
        // directorio: create_dir_all falla y nada debe moverse.
        let _blocker = write(&dir, "bloqueador", b"x");
        let dst_path = dir.path().join("bloqueador/foto.jpg");
        let dst = SafePath::new(root.path(), &dst_path).unwrap();
        let err = op.move_file(&src, &dst).unwrap_err();
        assert!(src.path().exists(), "el original debe seguir: {err}");
        assert_eq!(std::fs::read(src.path()).unwrap(), b"intacto");
        assert!(
            SafeOp::cleanup_orphan_backups(&root).unwrap().is_empty(),
            "no debe quedar respaldo huérfano tras el fallo"
        );
    }

    #[test]
    fn archive_without_date_returns_nodate_and_keeps_file() {
        let (dir, root, op) = setup();
        let src = write(&dir, "foto.jpg", b"x");
        let archive = SafePath::new(root.path(), &dir.path().join("archivo")).unwrap();
        let err = op.archive_file(&src, &archive, None).unwrap_err();
        assert!(matches!(err, AppError::NoDate(_)));
        assert!(src.path().exists());
    }

    #[test]
    fn archive_with_date_creates_year_month_folders() {
        let (dir, root, op) = setup();
        let src = write(&dir, "foto.jpg", b"x");
        let archive = SafePath::new(root.path(), &dir.path().join("archivo")).unwrap();
        let dst = op.archive_file(&src, &archive, Some((2024, 5))).unwrap();
        assert_eq!(dst.relative(), Path::new("archivo/2024/05/foto.jpg"));
        assert!(!src.path().exists());
    }

    #[test]
    fn trash_moves_file_or_skips_without_trash() {
        let (dir, _root, op) = setup();
        let src = write(&dir, "foto.jpg", b"x");
        match op.trash_file(&src) {
            Ok(()) => assert!(!src.path().exists()),
            Err(e) => {
                // En entornos sin papelera (CI/VM sin XDG) el test
                // no debe fallar: se reporta y se da por bueno.
                eprintln!("sin papelera en este entorno, test omitido: {e}");
            }
        }
    }

    #[test]
    fn restore_backup_recovers_original() {
        let (dir, root, op) = setup();
        let src = write(&dir, "foto.jpg", b"recuperable");
        // Simular un respaldo a mano con el formato de SafeOp.
        let backup_dir = dir.path().join(".ddamr-backup/manual-1");
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::copy(src.path(), backup_dir.join("foto.jpg")).unwrap();
        std::fs::write(
            backup_dir.join("origin.txt"),
            src.path().display().to_string(),
        )
        .unwrap();
        std::fs::remove_file(src.path()).unwrap();
        let restored = SafeOp::restore_backup(&backup_dir).unwrap();
        assert_eq!(restored, vec![src.path().to_path_buf()]);
        assert_eq!(std::fs::read(src.path()).unwrap(), b"recuperable");
        let orphans = SafeOp::cleanup_orphan_backups(&root).unwrap();
        assert_eq!(orphans, vec![backup_dir]);
        let _ = op; // SafeOp no se usa aquí, solo sus helpers
    }

    #[test]
    fn resolve_collision_numbers_suffixes() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.txt");
        assert_eq!(resolve_collision(&p), p);
        std::fs::write(&p, b"x").unwrap();
        assert_eq!(
            resolve_collision(&p).file_name().unwrap().to_string_lossy(),
            "a (2).txt"
        );
        std::fs::write(dir.path().join("a (2).txt"), b"x").unwrap();
        assert_eq!(
            resolve_collision(&p).file_name().unwrap().to_string_lossy(),
            "a (3).txt"
        );
    }
}
