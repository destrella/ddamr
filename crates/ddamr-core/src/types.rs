//! Tipos compartidos por todo el workspace: qué es un archivo,
//! cómo se ordena, cómo se filtra y cómo se pagina.
//!
//! Nada de aquí toca el disco ni la base de datos: son valores
//! puros, por eso viven en `ddamr-core`.

use serde::{Deserialize, Serialize};

/// Clase de un archivo, decidida solo por su extensión (en
/// minúsculas). Un directorio es `Dir` con independencia de su nombre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Other,
    Dir,
}

impl MediaKind {
    /// Clasifica por extensión, sin el punto. Desconocida → `Other`.
    pub fn from_ext(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "heic" | "heif" | "tif" | "tiff"
            | "bmp" | "svg" => Self::Image,
            "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" => Self::Video,
            "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "opus" => Self::Audio,
            _ => Self::Other,
        }
    }

    /// `true` para imagen, vídeo o audio.
    pub fn is_media(self) -> bool {
        matches!(self, Self::Image | Self::Video | Self::Audio)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Other => "other",
            Self::Dir => "dir",
        }
    }

    /// Desde el valor guardado en la DB ("image", "video"…).
    /// Se llama `from_db_str` y no `from_str` para no confundirse
    /// con `std::str::FromStr::from_str`.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "image" => Self::Image,
            "video" => Self::Video,
            "audio" => Self::Audio,
            "dir" => Self::Dir,
            _ => Self::Other,
        }
    }
}

/// Columna por la que se ordena el panel de contenido.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortKey {
    Name,
    Extension,
    Size,
    Date,
}

/// Dirección de la ordenación.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    pub fn flip(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

/// Filtros del panel de contenido. Los valores por defecto son los
/// del spec: ocultar carpetas y solo multimedia activados, archivos
/// ocultos y recursivo desactivados.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFilter {
    pub hide_folders: bool,
    pub only_media: bool,
    pub show_hidden: bool,
    pub recursive: bool,
    /// Búsqueda por nombre de archivo (subcadena, sin distinción
    /// de mayúsculas). Vacía = sin búsqueda.
    pub search: String,
    /// `None` = todos los tipos; `Some(k)` = solo ese tipo.
    pub kind: Option<MediaKind>,
}

impl Default for FileFilter {
    fn default() -> Self {
        Self {
            hide_folders: true,
            only_media: true,
            show_hidden: false,
            recursive: false,
            search: String::new(),
            kind: None,
        }
    }
}

impl FileFilter {
    /// ¿Pasa este archivo los filtros? `is_hidden` es "el nombre
    /// empieza por `.`". Los directorios no pasan por `only_media`
    /// ni por `kind`: solo por `hide_folders` y la búsqueda.
    pub fn matches(&self, name: &str, is_dir: bool, is_hidden: bool, kind: MediaKind) -> bool {
        if is_hidden && !self.show_hidden {
            return false;
        }
        if is_dir {
            if self.hide_folders {
                return false;
            }
        } else {
            if self.only_media && !kind.is_media() {
                return false;
            }
            if let Some(want) = self.kind
                && kind != want
            {
                return false;
            }
        }
        self.search.is_empty()
            || name
                .to_ascii_lowercase()
                .contains(&self.search.to_ascii_lowercase())
    }
}

/// Una página de resultados. `limit` está acotado para que ningún
/// llamador pueda pedir "todo" por accidente.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub offset: usize,
    pub limit: usize,
}

impl Page {
    pub const DEFAULT_LIMIT: usize = 200;
    pub const MAX_LIMIT: usize = 1000;

    pub fn new(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit: limit.clamp(1, Self::MAX_LIMIT),
        }
    }

    pub fn first() -> Self {
        Self {
            offset: 0,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    /// Página `index` (0 = primera) con el límite por defecto.
    pub fn at(index: usize) -> Self {
        Self {
            offset: index.saturating_mul(Self::DEFAULT_LIMIT),
            limit: Self::DEFAULT_LIMIT,
        }
    }

    pub fn index(self) -> usize {
        self.offset / self.limit.max(1)
    }
}

/// Resultado paginado: solo la página pedida, más el total para
/// dibujar "Página X de Y". El llamador nunca recibe más de
/// `page.limit` elementos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageResult<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub page: Page,
}

impl<T> PageResult<T> {
    pub fn new(items: Vec<T>, total: usize, page: Page) -> Self {
        Self { items, total, page }
    }

    pub fn empty(page: Page) -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            page,
        }
    }

    pub fn total_pages(&self) -> usize {
        if self.total == 0 {
            1
        } else {
            self.total.div_ceil(self.page.limit.max(1))
        }
    }

    pub fn has_next(&self) -> bool {
        self.page.index() + 1 < self.total_pages()
    }

    pub fn has_prev(&self) -> bool {
        self.page.index() > 0
    }
}

/// Tamaño en bytes, en forma legible (bytes, KB, MB, GB…).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Epoch en milisegundos → "YYYY-MM-DD HH:MM" (UTC).
/// Sin dependencias externas: conversión civil desde días
/// (algoritmo de Howard Hinnant). `None` → "-".
pub fn format_timestamp_ms(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_kind_from_ext() {
        assert_eq!(MediaKind::from_ext("JPG"), MediaKind::Image);
        assert_eq!(MediaKind::from_ext("mp4"), MediaKind::Video);
        assert_eq!(MediaKind::from_ext("flac"), MediaKind::Audio);
        assert_eq!(MediaKind::from_ext("pdf"), MediaKind::Other);
        assert!(MediaKind::Image.is_media());
        assert!(!MediaKind::Other.is_media());
    }

    #[test]
    fn filter_defaults_hide_folders_and_non_media() {
        let f = FileFilter::default();
        assert!(f.hide_folders && f.only_media && !f.show_hidden);
        assert!(!f.matches("carpeta", true, false, MediaKind::Dir));
        assert!(!f.matches("doc.pdf", false, false, MediaKind::Other));
        assert!(f.matches("foto.jpg", false, false, MediaKind::Image));
        assert!(!f.matches(".oculto.jpg", false, true, MediaKind::Image));
    }

    #[test]
    fn filter_search_is_case_insensitive() {
        let f = FileFilter {
            search: "VACACIONES".into(),
            ..Default::default()
        };
        assert!(f.matches("vacaciones-01.jpg", false, false, MediaKind::Image));
        assert!(!f.matches("trabajo.jpg", false, false, MediaKind::Image));
    }

    #[test]
    fn page_result_total_pages() {
        let r: PageResult<u8> = PageResult::new(vec![], 250, Page::first());
        assert_eq!(r.total_pages(), 2);
        assert!(r.has_next());
        let r2: PageResult<u8> = PageResult::new(vec![], 250, Page::at(1));
        assert!(!r2.has_next());
        assert!(r2.has_prev());
    }

    #[test]
    fn format_size_and_timestamp() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(format_timestamp_ms(Some(0)), "1970-01-01 00:00");
        assert_eq!(format_timestamp_ms(None), "-");
    }
}
