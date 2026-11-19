use crate::Result;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuración persistente de la app. Se guarda como JSON en la DB
/// (`settings`) y se versiona con `schema_version`: al añadir un campo
/// nuevo, los usuarios antiguos conservan sus preferencias.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub schema_version: u32,
    /// Raíz permitida. Solo puede estar dentro del home del usuario.
    pub root_dir: PathBuf,
    /// Destino del archivado cronológico, dentro de `root_dir`.
    /// Por defecto `~/archivo_cronológico`.
    pub archive_dir: PathBuf,
    /// Filtros por defecto del panel de contenido.
    pub hide_folders: bool,
    pub only_media: bool,
    pub show_hidden: bool,
    pub recursive: bool,
    /// Tamaño de miniatura en la rejilla (128..=512).
    pub thumbnail_size: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("~"));
        Self {
            schema_version: 1,
            root_dir: home.clone(),
            archive_dir: home.join("archivo_cronológico"),
            hide_folders: true,
            only_media: true,
            show_hidden: false,
            recursive: false,
            thumbnail_size: 256,
        }
    }
}

impl Config {
    /// Valida que la raíz y el archivado estén dentro del home y que
    /// el tamaño de miniatura esté en rango. Rechazar aquí es más
    /// barato que descubrirlo a mitad de un archivado.
    pub fn validate(&self) -> Result<()> {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        if !home.as_os_str().is_empty() {
            let home_canon = home.canonicalize().unwrap_or(home.clone());
            let root_canon = self.root_dir.canonicalize().unwrap_or_else(|_| {
                home_canon.join(self.root_dir.strip_prefix(&home).unwrap_or(&self.root_dir))
            });
            if !root_canon.starts_with(&home_canon) {
                return Err(AppError::Config(format!(
                    "la raíz {:?} debe estar dentro del home",
                    self.root_dir
                )));
            }
            // archive_dir puede no existir aún: validar por prefijo textual.
            if !self.archive_dir.starts_with(&self.root_dir)
                && !self.archive_dir.starts_with(&root_canon)
            {
                return Err(AppError::Config(format!(
                    "el archivado {:?} debe estar dentro de la raíz",
                    self.archive_dir
                )));
            }
        }
        if !(128..=512).contains(&self.thumbnail_size) {
            return Err(AppError::Config(
                "thumbnail_size debe estar entre 128 y 512".into(),
            ));
        }
        Ok(())
    }
}
