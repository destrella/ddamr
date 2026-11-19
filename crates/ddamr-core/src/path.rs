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
        let root_canon = root.canonicalize().map_err(|e| AppError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;

        let abs = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            root_canon.join(candidate)
        };

        let canon = if abs.exists() {
            abs.canonicalize().map_err(|e| AppError::Io {
                path: abs.clone(),
                source: e,
            })?
        } else {
            let parent = abs
                .parent()
                .ok_or_else(|| AppError::InvalidPath(format!("sin padre: {}", abs.display())))?;
            let file_name = abs
                .file_name()
                .ok_or_else(|| AppError::InvalidPath(format!("sin nombre: {}", abs.display())))?;
            // El padre debe existir; si no, no se puede validar la ruta.
            let parent_canon = parent.canonicalize().map_err(|e| AppError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
            parent_canon.join(file_name)
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
}
