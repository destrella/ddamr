//! ddamr-core: tipos, errores, paths seguros y configuración.
//!
//! Este crate no conoce a iced, ni a SQLite, ni al sistema de archivos
//! más allá de lo necesario para validar rutas. Todo lo demás depende
//! de él, nunca al revés.

pub mod config;
pub mod error;
pub mod path;

pub use config::Config;
pub use error::AppError;
pub use path::SafePath;

/// Resultado usado en todo el workspace.
pub type Result<T> = std::result::Result<T, AppError>;
