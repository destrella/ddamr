//! ddamr-metadata: lectura nativa rápida, sin ExifTool.
//!
//! Hot path: `imagesize` para dimensiones (solo lee la cabecera) y
//! `kamadak-exif` solo para JPEG/TIFF. Cualquier error de parseo
//! devuelve `None`/flags en `false`: esta función nunca hace panic
//! ni devuelve `Err` que aborte un listado. La escritura con
//! ExifTool llegará en fase 2, siempre sobre copia.

use ddamr_core::{AppError, Result, SafePath};
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

/// Campos que se pueden escribir en los metadatos de un archivo.
/// `None` = dejar el campo como está. La escritura es solo con
/// ExifTool y solo sobre una copia (ver `write_metadata`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetadataPatch {
    pub description: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub copyright: Option<String>,
    /// Formato EXIF: "YYYY:MM:DD HH:MM:SS".
    pub datetime_original: Option<String>,
    /// (latitud, longitud) en grados decimales.
    pub gps: Option<(f64, f64)>,
}

impl MetadataPatch {
    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.keywords.is_none()
            && self.copyright.is_none()
            && self.datetime_original.is_none()
            && self.gps.is_none()
    }
}

/// Normaliza palabras clave: separa por `,` y `;`, recorta
/// espacios, descarta vacías y elimina duplicados sin distinguir
/// mayúsculas, preservando la capitalización de la primera
/// aparición.
pub fn normalize_keywords(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in raw {
        for part in item.split([',', ';']) {
            let kw = part.trim();
            if kw.is_empty() {
                continue;
            }
            let key = kw.to_ascii_lowercase();
            if !seen.contains(&key) {
                seen.push(key);
                out.push(kw.to_string());
            }
        }
    }
    out
}

/// Escribe metadatos con ExifTool, por argv (nunca por shell) y
/// solo sobre la copia que recibe: ExifTool reescribe el archivo
/// in situ, así que jamás se le entrega el original. Timeout de
/// 30 s; si ExifTool no está instalado, error controlado, no pánico.
/// Tras escribir, relee con `read_basic` y verifica que los campos
/// comprobables (descripción, palabras clave) quedaron guardados.
pub fn write_metadata(copy: &SafePath, fields: &MetadataPatch) -> Result<()> {
    if fields.is_empty() {
        return Ok(());
    }
    // Comprobar que el binario existe antes de intentar escribir.
    match Command::new("exiftool").arg("-ver").output() {
        Ok(out) if out.status.success() => {}
        Ok(_) => {
            return Err(AppError::Metadata(
                "exiftool no disponible (salió con error)".into(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::Metadata("exiftool no disponible".into()));
        }
        Err(e) => return Err(AppError::IoPlain(e)),
    }

    let mut args: Vec<String> = vec![
        "-overwrite_original".into(),
        "-m".into(), // ignorar errores menores de formato
        "-q".into(),
    ];
    if let Some(desc) = &fields.description {
        args.push(format!("-Description={desc}"));
        args.push(format!("-EXIF:ImageDescription={desc}"));
    }
    if let Some(keywords) = &fields.keywords {
        let kws = normalize_keywords(keywords);
        args.push("-Keywords=".into()); // vaciar la lista antes
        for kw in &kws {
            args.push(format!("-Keywords+={kw}"));
            args.push(format!("-Subject+={kw}"));
        }
    }
    if let Some(copyright) = &fields.copyright {
        args.push(format!("-Copyright={copyright}"));
        args.push(format!("-Rights={copyright}"));
    }
    if let Some(dt) = &fields.datetime_original {
        args.push(format!("-DateTimeOriginal={dt}"));
    }
    if let Some((lat, lon)) = fields.gps {
        let (lat_ref, lat_abs) = if lat >= 0.0 { ("N", lat) } else { ("S", -lat) };
        let (lon_ref, lon_abs) = if lon >= 0.0 { ("E", lon) } else { ("W", -lon) };
        args.push(format!("-GPSLatitude={lat_abs:.6}"));
        args.push(format!("-GPSLatitudeRef={lat_ref}"));
        args.push(format!("-GPSLongitude={lon_abs:.6}"));
        args.push(format!("-GPSLongitudeRef={lon_ref}"));
    }
    args.push(copy.path().display().to_string());

    let mut child = Command::new("exiftool")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::Metadata("exiftool no disponible".into())
            } else {
                AppError::IoPlain(e)
            }
        })?;
    match child.wait_timeout(Duration::from_secs(30)) {
        Ok(Some(status)) => {
            if !status.success() {
                return Err(AppError::Metadata(format!(
                    "exiftool salió con estado {status}"
                )));
            }
        }
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::Metadata("exiftool no respondió en 30 s".into()));
        }
        Err(e) => return Err(AppError::IoPlain(e)),
    }

    // Verificar lo escrito con la lectura nativa.
    let meta = read_basic(copy);
    if fields.description.is_some() && !meta.has_description {
        return Err(AppError::Metadata(
            "verificación falló: la descripción no quedó guardada".into(),
        ));
    }
    if fields
        .keywords
        .as_ref()
        .is_some_and(|k| !normalize_keywords(k).is_empty())
        && !meta.has_keywords
    {
        return Err(AppError::Metadata(
            "verificación falló: las palabras clave no quedaron guardadas".into(),
        ));
    }
    Ok(())
}

/// Volcado crudo de ExifTool sobre un archivo (`-G -a -s`),
/// con timeout de 30 s y límite de salida de 10 MB. La salida va
/// a un fichero temporal (wait-timeout 0.2 no captura la salida
/// con timeout), que se borra siempre al terminar.
pub fn exiftool_raw(path: &SafePath) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!(
        "ddamr-exiftool-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let out_file = std::fs::File::create(&tmp).map_err(AppError::IoPlain)?;
    let mut child = Command::new("exiftool")
        .args(["-G", "-a", "-s", &path.path().display().to_string()])
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::Metadata("exiftool no disponible".into())
            } else {
                AppError::IoPlain(e)
            }
        })?;
    let status = match child.wait_timeout(Duration::from_secs(30)) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&tmp);
            return Err(AppError::Metadata("exiftool no respondió en 30 s".into()));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(AppError::IoPlain(e));
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(AppError::Metadata(format!(
            "exiftool salió con estado {status}"
        )));
    }
    const MAX_BYTES: usize = 10 * 1024 * 1024;
    let bytes = std::fs::read(&tmp).map_err(AppError::IoPlain)?;
    let _ = std::fs::remove_file(&tmp);
    if bytes.len() > MAX_BYTES {
        return Err(AppError::Metadata(
            "la salida de exiftool supera los 10 MB".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

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
    fn normalize_keywords_dedups_trims_and_splits() {
        let raw = vec![
            "  Playa ".to_string(),
            "playa;Sol, mar".to_string(),
            "SOL".to_string(),
            "".to_string(),
            "  ".to_string(),
        ];
        assert_eq!(normalize_keywords(&raw), vec!["Playa", "Sol", "mar"]);
    }

    #[test]
    fn write_metadata_without_exiftool_is_controlled_error() {
        // En esta VM no hay exiftool: debe devolver un error
        // controlado, no un pánico. En un Mac con exiftool el
        // test seguiría siendo válido si la escritura funciona,
        // así que solo se exige el error cuando el binario falta.
        if Command::new("exiftool").arg("-ver").output().is_ok() {
            return; // con exiftool instalado no se puede exigir el error
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(10, 10).save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let patch = MetadataPatch {
            description: Some("una descripción".into()),
            ..Default::default()
        };
        let err = write_metadata(&safe, &patch).unwrap_err();
        assert!(
            matches!(err, ddamr_core::AppError::Metadata(_)),
            "se esperaba error Metadata, fue: {err}"
        );
        assert!(err.to_string().contains("exiftool no disponible"));
    }

    #[test]
    fn exiftool_raw_without_exiftool_is_controlled_error() {
        if Command::new("exiftool").arg("-ver").output().is_ok() {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(10, 10).save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        assert!(matches!(
            exiftool_raw(&safe).unwrap_err(),
            ddamr_core::AppError::Metadata(_)
        ));
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
