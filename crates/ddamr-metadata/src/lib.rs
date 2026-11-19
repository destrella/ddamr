//! ddamr-metadata: lectura nativa rápida, sin ExifTool.
//!
//! Hot path: `imagesize` para dimensiones (solo lee la cabecera) y
//! `kamadak-exif` solo para JPEG/TIFF. Cualquier error de parseo
//! devuelve `None`/flags en `false`: esta función nunca hace panic
//! ni devuelve `Err` que aborte un listado. La escritura con
//! ExifTool llegará en fase 2, siempre sobre copia.

use ddamr_core::SafePath;
use std::io::BufReader;

/// Metadatos básicos de un archivo multimedia. Todos los campos
/// son opcionales: un archivo sin EXIF da `None`/`false`, no un error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BasicMetadata {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Epoch en milisegundos desde EXIF `DateTimeOriginal`.
    pub created_at: Option<i64>,
    pub has_keywords: bool,
    pub has_location: bool,
    pub has_description: bool,
}

/// Lee los metadatos básicos de un archivo. Infallible por diseño.
pub fn read_basic(path: &SafePath) -> BasicMetadata {
    let mut meta = BasicMetadata::default();

    // Dimensiones: imagesize solo lee la cabecera del archivo,
    // sin decodificar la imagen. Vale para JPEG, PNG, GIF, WebP…
    if let Ok(size) = imagesize::size(path.path()) {
        meta.width = Some(size.width as u32);
        meta.height = Some(size.height as u32);
    }

    // EXIF: solo JPEG/TIFF, que son los formatos donde kamadak-exif
    // puede leer. El resto de formatos no tiene EXIF que buscar aquí.
    let ext = path
        .path()
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let is_exif = matches!(
        ext.as_deref(),
        Some("jpg") | Some("jpeg") | Some("tif") | Some("tiff")
    );
    if !is_exif {
        return meta;
    }
    let file = match std::fs::File::open(path.path()) {
        Ok(f) => f,
        Err(_) => return meta,
    };
    let mut reader = BufReader::new(file);
    let exif = match exif::Reader::new().read_from_container(&mut reader) {
        Ok(e) => e,
        Err(_) => return meta,
    };
    for field in exif.fields() {
        match field.tag {
            exif::Tag::DateTimeOriginal => {
                if meta.created_at.is_none()
                    && let Some(s) = ascii_value(field)
                {
                    meta.created_at = parse_exif_datetime(&s);
                }
            }
            exif::Tag::GPSLatitude | exif::Tag::GPSLongitude => {
                meta.has_location = true;
            }
            exif::Tag::ImageDescription | exif::Tag::UserComment => {
                if ascii_value(field).is_some_and(|s| !s.trim().is_empty()) {
                    meta.has_description = true;
                }
            }
            _ => {
                // Palabras clave: kamadak-exif no expone un Tag
                // estable para XPKeywords en todas las versiones, así
                // que se detecta por el nombre del tag.
                let name = format!("{:?}", field.tag);
                if name.contains("Keyword") || name.contains("Subject") {
                    meta.has_keywords = true;
                }
            }
        }
    }
    meta
}

/// Primer valor ASCII de un campo EXIF, si lo tiene.
fn ascii_value(field: &exif::Field) -> Option<String> {
    match &field.value {
        exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// "2024:05:01 10:30:00" (fecha EXIF, sin zona horaria) → epoch ms.
/// Sin zona horaria no se puede saber el instante exacto: se
/// interpreta como UTC y se documenta así. Si no parsea, `None`.
fn parse_exif_datetime(s: &str) -> Option<i64> {
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split(':');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, m as u32, day as u32);
    Some((days * 86_400 + hh * 3600 + mm * 60 + ss) * 1000)
}

/// Días desde 1970-01-01 (algoritmo de Howard Hinnant).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddamr_core::SafePath;
    use tempfile::TempDir;

    /// Genera un JPEG real de 100x50 con la crate `image` y verifica
    /// que `read_basic` lee sus dimensiones. No depende de ningún
    /// fixture externo: el archivo se crea en el propio test.
    #[test]
    fn read_basic_jpeg_generated_with_image_crate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        let img = image::RgbImage::new(100, 50);
        img.save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let meta = read_basic(&safe);
        assert_eq!(meta.width, Some(100));
        assert_eq!(meta.height, Some(50));
        // La imagen generada no tiene EXIF: fecha y flags en None/false.
        assert_eq!(meta.created_at, None);
        assert!(!meta.has_location);
        assert!(!meta.has_keywords);
    }

    #[test]
    fn read_basic_never_fails_on_garbage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("roto.jpg");
        std::fs::write(&path, b"esto no es un jpeg").unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let meta = read_basic(&safe);
        assert_eq!(meta.width, None);
        assert_eq!(meta.created_at, None);
    }

    #[test]
    fn read_basic_png_dimensions_without_exif() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("img.png");
        let img = image::RgbImage::new(37, 21);
        img.save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let meta = read_basic(&safe);
        assert_eq!(meta.width, Some(37));
        assert_eq!(meta.height, Some(21));
    }

    #[test]
    fn parse_exif_datetime_works() {
        // 2024-01-02 03:04:05 UTC = 1704067200 + 86400 + 11045 s
        assert_eq!(
            parse_exif_datetime("2024:01:02 03:04:05"),
            Some(1_704_164_645_000)
        );
        assert_eq!(parse_exif_datetime("no-es-fecha"), None);
        assert_eq!(parse_exif_datetime("2024:13:01 00:00:00"), None);
    }
}
