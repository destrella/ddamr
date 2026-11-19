use crate::Result;
use crate::error::AppError;
use std::path::{Path, PathBuf};

/// Una ruta canónica garantizada dentro de la raíz permitida.
///
/// Toda operación sobre archivos (listar, mover, archivar, borrar,
/// leer metadatos) recibe un `SafePath`, nunca un `PathBuf` crudo.
/// La garantía se comprueba en la construcción —canonicalizando, lo
/// que resuelve `..` y symlinks— y se vuelve a comprobar en `join`.
///
/// La raíz es, por defecto, el home del usuario; la configuración solo
/// permite elegir una raíz dentro de él (ver `Config`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SafePath {
    /// Raíz canónica permitida.
    root: PathBuf,
    /// Ruta canónica, siempre bajo `root`.
    inner: PathBuf,
}

impl SafePath {
    /// Construye un `SafePath` validando `candidate` contra `root`.
    ///
    /// - `candidate` absoluta: se valida tal cual.
    /// - `candidate` relativa: se interpreta relativa a `root`.
    /// - Si `candidate` no existe todavía (p. ej. destino de un mover),
    ///   se canonicaliza su padre y se une el nombre de archivo.
    /// - Un symlink dentro de `root` que apunte fuera se rechaza,
    ///   porque `canonicalize` lo resuelve antes de comparar.
    pub fn new(root: &Path, candidate: &Path) -> Result<Self> {
        Self::new_inner(root, candidate, 0)
    }

    fn new_inner(root: &Path, candidate: &Path, depth: u8) -> Result<Self> {
        if depth > 16 {
            return Err(AppError::InvalidPath(
                "demasiados symlinks encadenados".into(),
            ));
        }
        let root_canon = root.canonicalize().map_err(|e| AppError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;

        let abs = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root_canon.join(candidate)
        };
        // Normalizar `..` y `.` léxicamente ANTES de tocar el disco.
        // Sin esto, el resultado depende de la profundidad del
        // directorio temporal: en Linux `/tmp/.tmpXXX/../../etc/passwd`
        // existe y se resolvía a `/etc/passwd`, pero en macOS el
        // temporal es `/var/folders/.../T/.tmpXXX` y esa misma ruta
        // cae en un `etc/` inexistente, con lo que el código anterior
        // devolvía un error de E/S en vez de `OutsideRoot`.
        let abs = normalize_lexically(&abs);

        // Un symlink roto no "existe" para `Path::exists`, así que
        // hay que resolverlo explícitamente; si no, un enlace roto
        // que apunte fuera de la raíz se aceptaría como ruta interior.
        if let Ok(md) = std::fs::symlink_metadata(&abs)
            && md.file_type().is_symlink()
        {
            let target = std::fs::read_link(&abs).map_err(|e| AppError::Io {
                path: abs.clone(),
                source: e,
            })?;
            let resolved = if target.is_absolute() {
                target
            } else {
                abs.parent()
                    .map(|p| p.join(target))
                    .ok_or_else(|| AppError::InvalidPath(format!("sin padre: {}", abs.display())))?
            };
            return Self::new_inner(root, &resolved, depth + 1);
        }

        let canon = if abs.exists() {
            abs.canonicalize().map_err(|e| AppError::Io {
                path: abs.clone(),
                source: e,
            })?
        } else {
            // La ruta no existe: buscar el ancestro existente más
            // largo, canonicalizarlo (resuelve symlinks reales) y
            // unir el resto, que ya no contiene `..` ni `.`.
            // Así funcionan tanto `nuevo.txt` como `a/b/c.txt`
            // con directorios intermedios aún no creados, y un
            // escape a una ruta inexistente sigue siendo
            // `OutsideRoot` en vez de un error de E/S.
            let mut ancestor: Option<PathBuf> = None;
            for anc in abs.ancestors() {
                if anc.exists() {
                    ancestor = Some(anc.to_path_buf());
                    break;
                }
            }
            let anc = ancestor.ok_or_else(|| {
                AppError::InvalidPath(format!("sin ancestro existente: {}", abs.display()))
            })?;
            let anc_canon = anc.canonicalize().map_err(|e| AppError::Io {
                path: anc.clone(),
                source: e,
            })?;
            let remainder = abs.strip_prefix(&anc).unwrap_or(Path::new(""));
            anc_canon.join(remainder)
        };

        if !canon.starts_with(&root_canon) {
            return Err(AppError::OutsideRoot(canon));
        }
        Ok(Self {
            root: root_canon,
            inner: canon,
        })
    }

    /// La raíz misma, como `SafePath`.
    pub fn from_root(root: &Path) -> Result<Self> {
        Self::new(root, root)
    }

    /// Une una ruta relativa y revalida el resultado. `..` y symlinks
    /// que escapen se rechazan aquí, no en el llamador.
    pub fn join(&self, rel: &Path) -> Result<Self> {
        if rel.is_absolute() {
            return Err(AppError::InvalidPath(format!(
                "join espera una ruta relativa, no {:?}",
                rel
            )));
        }
        Self::new(&self.root, &self.inner.join(rel))
    }

    pub fn path(&self) -> &Path {
        &self.inner
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ruta relativa a la raíz. Es lo que se guarda en la DB
    /// (nunca la ruta absoluta): si el home cambia de sitio, la DB
    /// sigue sirviendo.
    pub fn relative(&self) -> &Path {
        self.inner.strip_prefix(&self.root).unwrap_or(Path::new(""))
    }
}

/// Resuelve `.` y `..` sin tocar el disco. En una ruta absoluta,
/// un `..` en la raíz se descarta. No resuelve symlinks: eso lo hace
/// `canonicalize` sobre el ancestro existente en `SafePath::new`.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

impl AsRef<Path> for SafePath {
    fn as_ref(&self) -> &Path {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn setup() -> (TempDir, TempDir) {
        // (root, outside): dos temporales distintos, fuera uno del otro.
        (TempDir::new().unwrap(), TempDir::new().unwrap())
    }

    #[test]
    fn accepts_root_itself_and_inside() {
        let (root, _) = setup();
        let root_path = SafePath::from_root(root.path()).unwrap();
        assert_eq!(root_path.path(), root.path().canonicalize().unwrap());

        std::fs::create_dir(root.path().join("fotos")).unwrap();
        let inside = SafePath::new(root.path(), &root.path().join("fotos")).unwrap();
        assert_eq!(inside.relative(), Path::new("fotos"));
    }

    #[test]
    fn rejects_parent_escape_with_dotdot() {
        let (root, outside) = setup();
        let err = SafePath::new(root.path(), &outside.path().join("secreto.txt")).unwrap_err();
        assert!(matches!(err, AppError::OutsideRoot(_)));

        // `..` explícito desde dentro.
        let err = SafePath::new(root.path(), Path::new("../fuera.txt")).unwrap_err();
        assert!(matches!(err, AppError::OutsideRoot(_)));
    }

    #[test]
    fn rejects_symlink_pointing_outside() {
        let (root, outside) = setup();
        let target = outside.path().join("real.txt");
        std::fs::write(&target, b"no deberias leer esto").unwrap();
        symlink(&target, root.path().join("enlace.txt")).unwrap();

        let err = SafePath::new(root.path(), &root.path().join("enlace.txt")).unwrap_err();
        assert!(matches!(err, AppError::OutsideRoot(_)));
    }

    #[test]
    fn join_rejects_escape() {
        let (root, _) = setup();
        let base = SafePath::from_root(root.path()).unwrap();
        let err = base.join(Path::new("../../etc/passwd")).unwrap_err();
        assert!(matches!(err, AppError::OutsideRoot(_)));
        let err = base.join(Path::new("/absoluta")).unwrap_err();
        assert!(matches!(err, AppError::InvalidPath(_)));
    }

    #[test]
    fn nonexistent_inside_is_allowed_but_outside_is_not() {
        let (root, outside) = setup();
        let ok = SafePath::new(root.path(), &root.path().join("nuevo.txt")).unwrap();
        assert_eq!(ok.relative(), Path::new("nuevo.txt"));

        let err = SafePath::new(root.path(), &outside.path().join("nuevo.txt")).unwrap_err();
        assert!(matches!(err, AppError::OutsideRoot(_)));
    }

    /// Regresión del fallo visto en macOS: el temporal es profundo
    /// (`/var/folders/.../T/.tmpXXX`), así que `../../etc/passwd`
    /// no existe y el código anterior devolvía E/S en vez de
    /// `OutsideRoot`. Con suficientes `..` para salir de la raíz,
    /// el escape debe rechazarse aunque el destino no exista.
    #[test]
    fn deep_dotdot_escape_to_nonexistent_is_rejected() {
        let (root, _) = setup();
        let deep = root.path().join("a/b/c/d");
        std::fs::create_dir_all(&deep).unwrap();
        let base = SafePath::new(root.path(), &deep).unwrap();
        let err = base
            .join(Path::new("../../../../../../etc/passwd"))
            .unwrap_err();
        assert!(
            matches!(err, AppError::OutsideRoot(_)),
            "escape profundo a ruta inexistente debe ser OutsideRoot, fue: {err:?}"
        );
    }

    /// Una ruta inexistente con intermedios aún no creados dentro
    /// de la raíz debe aceptarse (destino de un mover/archivado).
    #[test]
    fn nonexistent_nested_inside_is_allowed() {
        let (root, _) = setup();
        let ok = SafePath::new(root.path(), &root.path().join("nuevo/sub/archivo.txt")).unwrap();
        assert_eq!(ok.relative(), Path::new("nuevo/sub/archivo.txt"));
    }

    #[test]
    fn broken_symlink_pointing_outside_is_rejected() {
        let (root, outside) = setup();
        let target = outside.path().join("no-existe.txt");
        symlink(&target, root.path().join("roto.txt")).unwrap();
        let err = SafePath::new(root.path(), &root.path().join("roto.txt")).unwrap_err();
        assert!(matches!(err, AppError::OutsideRoot(_)));
    }
}
