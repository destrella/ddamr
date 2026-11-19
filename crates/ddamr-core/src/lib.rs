//! ddamr-core: tipos, errores, paths seguros y configuración.
//!
//! Este crate no conoce a iced, ni a SQLite, ni al sistema de archivos
//! más allá de lo necesario para validar rutas. Todo lo demás depende
//! de él, nunca al revés.

pub mod config;
pub mod error;
pub mod path;
pub mod types;

pub use config::Config;
pub use error::AppError;
pub use path::SafePath;
pub use types::{
    FileFilter, MediaKind, Page, PageResult, SortKey, SortOrder, format_size, format_timestamp_ms,
};

/// Resultado usado en todo el workspace.
pub type Result<T> = std::result::Result<T, AppError>;
