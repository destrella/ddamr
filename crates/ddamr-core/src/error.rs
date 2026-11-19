use std::path::PathBuf;
use thiserror::Error;

/// Error único del core. Cada variante lleva el contexto necesario para
/// registrarlo con `tracing` y mostrarlo en la UI sin exponer detalles
/// internos. La app nunca debe hacer `panic!` por un archivo corrupto,
/// un permiso denegado o un metadato ilegible: esos casos son `Err`.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("la ruta {0:?} está fuera de la raíz permitida")]
    OutsideRoot(PathBuf),

    #[error("ruta inválida: {0}")]
    InvalidPath(String),

    #[error("error de E/S en {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("error de E/S: {0}")]
    IoPlain(#[from] std::io::Error),

    #[error("configuración inválida: {0}")]
    Config(String),

    #[error("base de datos: {0}")]
    Database(String),

    #[error("metadatos: {0}")]
    Metadata(String),

    #[error("operación cancelada por el usuario")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
