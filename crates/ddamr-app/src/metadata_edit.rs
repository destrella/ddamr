//! Edición segura de metadatos en la UI (fase 2b).
//!
//! El formulario vive en `App`, pero la lógica que toca el disco
//! está aquí, fuera de `update`: validación pura (fecha EXIF,
//! GPS, regiones en %) y la orquestación segura
//! `apply_metadata_edit` (respaldo → copia → escribir → verificar
//! → rename atómico → descartar respaldo; si algo falla, el
//! original se restaura intacto). La escritura en sí es inyectable
//! (`WriteFn`) para poder probarla sin ExifTool: en la app real
//! es `ddamr_metadata::write_metadata`.

use ddamr_core::{AppError, Result, SafePath};
use ddamr_db::repos::TagRepo;
use ddamr_metadata::{BasicMetadata, MetadataPatch, normalize_keywords, read_basic};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Función que escribe metadatos sobre una copia. En la app es
/// `ddamr_metadata::write_metadata`; en los tests, un stub.
pub type WriteFn = fn(&SafePath, &MetadataPatch) -> Result<()>;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_id() -> String {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{n}", std::process::id(), now)
}

/// Abre la DB de la app, creando el directorio padre si falta
/// (en una instalación nueva `~/.cache/ddamr/` no existe y
/// `Connection::open` fallaría sin él).
pub fn open_app_db(db_path: &Path) -> Result<rusqlite::Connection> {
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(AppError::IoPlain)?;
    }
    ddamr_db::open(db_path)
}

/// Sincroniza las etiquetas de un archivo en la DB con las
/// palabras clave guardadas en el archivo: quita las actuales y
/// pone las normalizadas. Un archivo no indexado da error (el
/// llamador lo muestra, no rompe el guardado del archivo).
pub fn sync_tags(db_path: &Path, rel_path: &str, keywords: &[String]) -> Result<()> {
    let conn = open_app_db(db_path)?;
    let current = TagRepo::list_for_file(&conn, rel_path)?;
    for tag in &current {
        TagRepo::remove_from_file(&conn, rel_path, tag.id)?;
    }
    for kw in normalize_keywords(keywords) {
        let id = TagRepo::create(&conn, &kw)?;
        TagRepo::add_to_file(&conn, rel_path, id)?;
    }
    Ok(())
}

// ------------------------------------------------------------- formulario

/// Estado del formulario de edición de la columna derecha.
/// Los campos vacíos significan "sin cambios" (no se borra un
/// campo dejándolo vacío: ExifTool distingue mal "vaciar" de "no
/// tocar" según el campo, así que en 2b vacío = no tocar).
#[derive(Debug, Clone, Default)]
pub struct EditForm {
    pub description: String,
    pub keywords: String,
    pub copyright: String,
    /// "YYYY:MM:DD HH:MM:SS" o vacío.
    pub datetime_original: String,
    pub lat: String,
    pub lon: String,
    /// `true` desde que el usuario edita cualquier campo.
    pub dirty: bool,
}

impl EditForm {
    /// Pre-rellena el formulario desde los metadatos leídos y las
    /// etiquetas de la DB. La fecha y el GPS salen de `read_basic`;
    /// las palabras clave, de las etiquetas del archivo.
    pub fn from_metadata(meta: &BasicMetadata, tags: &[String]) -> Self {
        Self {
            description: meta.description.clone().unwrap_or_default(),
            keywords: tags.join(", "),
            copyright: String::new(),
            datetime_original: meta
                .created_at
                .map(format_exif_datetime)
                .unwrap_or_default(),
            lat: meta
                .gps
                .map(|(lat, _)| format!("{lat:.6}"))
                .unwrap_or_default(),
            lon: meta
                .gps
                .map(|(_, lon)| format!("{lon:.6}"))
                .unwrap_or_default(),
            dirty: false,
        }
    }
}

// ------------------------------------------------------------ validación

/// "YYYY:MM:DD HH:MM:SS" con rangos reales (mes 1-12, día válido
/// para ese mes y año, hora 0-23, min/seg 0-59).
pub fn validate_exif_datetime(s: &str) -> std::result::Result<(), String> {
    let fail = || format!("fecha inválida {s:?}: usa YYYY:MM:DD HH:MM:SS");
    let (date, time) = s.split_once(' ').ok_or_else(fail)?;
    let mut d = date.split(':');
    let y: i64 = d.next().and_then(|v| v.parse().ok()).ok_or_else(fail)?;
    let m: u32 = d.next().and_then(|v| v.parse().ok()).ok_or_else(fail)?;
    let day: u32 = d.next().and_then(|v| v.parse().ok()).ok_or_else(fail)?;
    if d.next().is_some() {
        return Err(fail());
    }
    let mut t = time.split(':');
    let hh: u32 = t.next().and_then(|v| v.parse().ok()).ok_or_else(fail)?;
    let mm: u32 = t.next().and_then(|v| v.parse().ok()).ok_or_else(fail)?;
    let ss: u32 = t.next().and_then(|v| v.parse().ok()).ok_or_else(fail)?;
    if t.next().is_some() {
        return Err(fail());
    }
    if !(1..=12).contains(&m) || hh > 23 || mm > 59 || ss > 59 {
        return Err(fail());
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let max_day = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => unreachable!(),
    };
    if day == 0 || day > max_day {
        return Err(fail());
    }
    Ok(())
}

/// Latitud/longitud de texto: ambas vacías = sin cambios (`None`);
/// una sola rellena, un número no válido o fuera de rango = error.
pub fn parse_lat_lon(lat_s: &str, lon_s: &str) -> std::result::Result<Option<(f64, f64)>, String> {
    let (lat_s, lon_s) = (lat_s.trim(), lon_s.trim());
    if lat_s.is_empty() && lon_s.is_empty() {
        return Ok(None);
    }
    if lat_s.is_empty() || lon_s.is_empty() {
        return Err("hay que rellenar latitud y longitud juntas".into());
    }
    let lat: f64 = lat_s
        .parse()
        .map_err(|_| format!("latitud inválida: {lat_s:?}"))?;
    let lon: f64 = lon_s
        .parse()
        .map_err(|_| format!("longitud inválida: {lon_s:?}"))?;
    if !(-90.0..=90.0).contains(&lat) {
        return Err(format!("latitud {lat} fuera de -90..90"));
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err(format!("longitud {lon} fuera de -180..180"));
    }
    Ok(Some((lat, lon)))
}

/// Región en porcentaje (0..=100) → normalizada (0..=1), con la
/// comprobación de desbordamiento (x+w ≤ 100, y+h ≤ 100).
pub fn parse_region_pct(
    x_s: &str,
    y_s: &str,
    w_s: &str,
    h_s: &str,
) -> std::result::Result<(f64, f64, f64, f64), String> {
    let parse = |label: &str, s: &str| -> std::result::Result<f64, String> {
        let v: f64 = s
            .trim()
            .parse()
            .map_err(|_| format!("{label} inválido: {s:?}"))?;
        if !(0.0..=100.0).contains(&v) {
            return Err(format!("{label}={v} fuera de 0..100"));
        }
        Ok(v / 100.0)
    };
    let (x, y, w, h) = (
        parse("x", x_s)?,
        parse("y", y_s)?,
        parse("ancho", w_s)?,
        parse("alto", h_s)?,
    );
    if x + w > 1.0 || y + h > 1.0 {
        return Err("la región desborda la imagen (x+ancho o y+alto > 100)".into());
    }
    Ok((x, y, w, h))
}

/// Epoch en ms (UTC) → "YYYY:MM:DD HH:MM:SS" para el formulario.
pub fn format_exif_datetime(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}:{m:02}:{d:02} {:02}:{:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
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
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let d = doy - (153 * mp + 2) / 5 + 1;
    // Enero y febrero pertenecen al año siguiente del cómputo.
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Construye el `MetadataPatch` desde el formulario, validando
/// fecha y GPS. Los campos vacíos no se tocan (`None`).
pub fn build_patch(form: &EditForm) -> std::result::Result<MetadataPatch, String> {
    if !form.datetime_original.trim().is_empty() {
        validate_exif_datetime(form.datetime_original.trim())?;
    }
    let gps = parse_lat_lon(&form.lat, &form.lon)?;
    let keywords = if form.keywords.trim().is_empty() {
        None
    } else {
        Some(normalize_keywords(std::slice::from_ref(&form.keywords)))
    };
    Ok(MetadataPatch {
        description: non_empty(&form.description),
        keywords,
        copyright: non_empty(&form.copyright),
        datetime_original: non_empty(&form.datetime_original),
        gps,
    })
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

// ------------------------------------------------- orquestación segura

/// Resultado de una edición guardada: tamaño y fecha del archivo
/// tras el reemplazo, para actualizar la entrada de la lista sin
/// re-escanear. `warning` lleva avisos no fatales (p. ej. las
/// etiquetas no se pudieron sincronizar en la DB).
#[derive(Debug, Clone, Default)]
pub struct EditOutcome {
    pub size_bytes: u64,
    pub modified_at: Option<i64>,
    pub warning: String,
}

/// Edita los metadatos de `src` sin poder perder el archivo:
///
/// 1. Respaldo del original en `.ddamr-backup/`.
/// 2. Copia de trabajo en el mismo directorio (mismo filesystem).
/// 3. `write` (ExifTool) solo sobre la copia.
/// 4. Verificación de la copia: existe, pesa > 0 y `read_basic`
///    relee los campos pedidos (descripción, palabras clave).
/// 5. Rename atómico copia → original.
/// 6. Verificación final sobre el original y descarte del
///    respaldo. Si el paso 6 falla, se restaura desde el respaldo.
///
/// Si cualquier paso anterior al 5 falla, el original no se ha
/// tocado: solo se borran la copia y el respaldo. `patch` vacío
/// no toca nada y devuelve el estado actual del archivo.
pub fn apply_metadata_edit(
    root: &SafePath,
    src: &SafePath,
    patch: &MetadataPatch,
    write: WriteFn,
) -> Result<EditOutcome> {
    let outcome_now = || -> Result<EditOutcome> {
        let md = std::fs::metadata(src.path()).map_err(AppError::IoPlain)?;
        Ok(EditOutcome {
            size_bytes: md.len(),
            modified_at: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64),
            warning: String::new(),
        })
    };
    if patch.is_empty() {
        return outcome_now();
    }
    let ops = ddamr_fs::SafeOp::new(root);
    let backup_dir = ops.backup_file(src)?;
    let name = src
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archivo".into());
    let tmp_path = src
        .path()
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!(".ddamr-tmp-{}-{name}", unique_id()));
    // Si algo sale mal antes del rename, borrar la copia y el
    // respaldo y devolver el error con el original intacto.
    let abort = |ops: &ddamr_fs::SafeOp, backup_dir: &Path, tmp_path: &Path, e: AppError| {
        let _ = std::fs::remove_file(tmp_path);
        ops.discard_backup(backup_dir);
        Err(e)
    };
    std::fs::copy(src.path(), &tmp_path).map_err(|e| {
        ops.discard_backup(&backup_dir);
        AppError::Io {
            path: tmp_path.clone(),
            source: e,
        }
    })?;
    let tmp_safe = match SafePath::new(root.root(), &tmp_path) {
        Ok(p) => p,
        Err(e) => return abort(&ops, &backup_dir, &tmp_path, e),
    };
    if let Err(e) = write(&tmp_safe, patch) {
        return abort(&ops, &backup_dir, &tmp_path, e);
    }
    // Verificar la copia antes de reemplazar el original.
    if let Err(e) = verify_copy(&tmp_safe, patch) {
        return abort(&ops, &backup_dir, &tmp_path, e);
    }
    if let Err(e) = std::fs::rename(&tmp_path, src.path()) {
        return abort(
            &ops,
            &backup_dir,
            &tmp_path,
            AppError::Io {
                path: src.path().to_path_buf(),
                source: e,
            },
        );
    }
    // Verificación final sobre el original ya reemplazado: si
    // falla, restaurar desde el respaldo.
    if let Err(e) = verify_copy(src, patch) {
        let restore_err = ops.restore_backup_file(&backup_dir, src);
        return Err(match restore_err {
            Ok(()) => e,
            Err(re) => AppError::Other(format!(
                "verificación falló ({e}) y la restauración también ({re})"
            )),
        });
    }
    ops.discard_backup(&backup_dir);
    outcome_now()
}

/// La copia (o el original tras el rename) existe, pesa > 0 y
/// relee los campos comprobables que se pidieron.
fn verify_copy(path: &SafePath, patch: &MetadataPatch) -> Result<()> {
    let md = std::fs::metadata(path.path()).map_err(AppError::IoPlain)?;
    if md.len() == 0 {
        return Err(AppError::Metadata(format!(
            "verificación falló: {} quedó vacío",
            path.path().display()
        )));
    }
    let meta = read_basic(path);
    if patch.description.is_some() && !meta.has_description {
        return Err(AppError::Metadata(
            "verificación falló: la descripción no quedó guardada".into(),
        ));
    }
    if patch
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

#[cfg(test)]
mod tests {
    use super::*;
    use ddamr_core::SafePath;
    use ddamr_metadata::write_metadata;
    use tempfile::TempDir;

    fn setup_file(content: &[u8]) -> (TempDir, SafePath, SafePath) {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("foto.jpg");
        std::fs::write(&p, content).unwrap();
        let root = SafePath::from_root(dir.path()).unwrap();
        let src = SafePath::new(dir.path(), &p).unwrap();
        (dir, root, src)
    }

    /// Stub de escritura: añade un marcador a la copia, como haría
    /// ExifTool al reescribir el archivo.
    fn stub_append(copy: &SafePath, _patch: &MetadataPatch) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(copy.path())
            .map_err(AppError::IoPlain)?;
        f.write_all(b"!").map_err(AppError::IoPlain)?;
        Ok(())
    }

    fn stub_fail(_copy: &SafePath, _patch: &MetadataPatch) -> Result<()> {
        Err(AppError::Metadata("fallo simulado de escritura".into()))
    }

    fn stub_noop(_copy: &SafePath, _patch: &MetadataPatch) -> Result<()> {
        Ok(())
    }

    #[test]
    fn apply_edit_replaces_original_and_discards_backup() {
        let (dir, root, src) = setup_file(b"original");
        let patch = MetadataPatch {
            copyright: Some("yo".into()),
            ..Default::default()
        };
        let out = apply_metadata_edit(&root, &src, &patch, stub_append).unwrap();
        assert_eq!(std::fs::read(src.path()).unwrap(), b"original!");
        assert!(out.size_bytes > 0);
        assert!(
            ddamr_fs::SafeOp::cleanup_orphan_backups(&root)
                .unwrap()
                .is_empty(),
            "el respaldo debe descartarse tras el éxito"
        );
        let _ = dir;
    }

    #[test]
    fn apply_edit_failure_leaves_original_intact() {
        let (dir, root, src) = setup_file(b"original");
        let patch = MetadataPatch {
            copyright: Some("yo".into()),
            ..Default::default()
        };
        let err = apply_metadata_edit(&root, &src, &patch, stub_fail).unwrap_err();
        assert!(err.to_string().contains("fallo simulado"));
        assert_eq!(std::fs::read(src.path()).unwrap(), b"original");
        assert!(
            ddamr_fs::SafeOp::cleanup_orphan_backups(&root)
                .unwrap()
                .is_empty(),
            "no debe quedar respaldo huérfano tras el fallo"
        );
        let _ = dir;
    }

    #[test]
    fn apply_edit_failed_verification_restores_original() {
        // La escritura "funciona" pero no deja la descripción:
        // la verificación de la copia debe abortar antes del rename.
        let (dir, root, src) = setup_file(b"original");
        let patch = MetadataPatch {
            description: Some("una descripción".into()),
            ..Default::default()
        };
        let err = apply_metadata_edit(&root, &src, &patch, stub_noop).unwrap_err();
        assert!(err.to_string().contains("verificación falló"));
        assert_eq!(std::fs::read(src.path()).unwrap(), b"original");
        assert!(
            ddamr_fs::SafeOp::cleanup_orphan_backups(&root)
                .unwrap()
                .is_empty()
        );
        let _ = dir;
    }

    #[test]
    fn apply_edit_without_exiftool_is_controlled_error() {
        if std::process::Command::new("exiftool")
            .arg("-ver")
            .output()
            .is_ok()
        {
            return; // con exiftool instalado no se puede exigir el error
        }
        let (dir, root, src) = setup_file(b"original");
        let patch = MetadataPatch {
            description: Some("una descripción".into()),
            ..Default::default()
        };
        let err = apply_metadata_edit(&root, &src, &patch, write_metadata).unwrap_err();
        assert!(err.to_string().contains("exiftool no disponible"));
        assert_eq!(std::fs::read(src.path()).unwrap(), b"original");
        let _ = dir;
    }

    #[test]
    fn validate_exif_datetime_accepts_and_rejects() {
        assert!(validate_exif_datetime("2024:05:01 10:30:00").is_ok());
        assert!(
            validate_exif_datetime("2024:02:29 00:00:00").is_ok(),
            "2024 es bisiesto"
        );
        assert!(validate_exif_datetime("2023:02:29 00:00:00").is_err());
        assert!(validate_exif_datetime("2024:13:01 00:00:00").is_err());
        assert!(validate_exif_datetime("2024:04:31 00:00:00").is_err());
        assert!(validate_exif_datetime("2024:05:01 24:00:00").is_err());
        assert!(validate_exif_datetime("2024-05-01 10:30:00").is_err());
        assert!(validate_exif_datetime("2024:05:01").is_err());
        assert!(validate_exif_datetime("").is_err());
    }

    #[test]
    fn parse_lat_lon_ranges() {
        assert_eq!(parse_lat_lon("", "").unwrap(), None);
        assert_eq!(parse_lat_lon("41.38", "2.16").unwrap(), Some((41.38, 2.16)));
        assert_eq!(parse_lat_lon("-90", "-180").unwrap(), Some((-90.0, -180.0)));
        assert!(parse_lat_lon("91", "0").is_err());
        assert!(parse_lat_lon("0", "181").is_err());
        assert!(parse_lat_lon("41", "").is_err(), "una sola no vale");
        assert!(parse_lat_lon("no-numero", "0").is_err());
    }

    #[test]
    fn parse_region_pct_validates_overflow() {
        assert_eq!(
            parse_region_pct("10", "20", "30", "40").unwrap(),
            (0.1, 0.2, 0.3, 0.4)
        );
        assert!(parse_region_pct("90", "0", "20", "10").is_err(), "x+w>100");
        assert!(parse_region_pct("0", "90", "10", "20").is_err(), "y+h>100");
        assert!(parse_region_pct("0", "0", "100", "100").is_ok());
        assert!(parse_region_pct("101", "0", "1", "1").is_err());
        assert!(parse_region_pct("a", "0", "1", "1").is_err());
    }

    #[test]
    fn format_exif_datetime_roundtrip() {
        // 2024-01-02 03:04:05 UTC = 1704164645 s.
        assert_eq!(
            format_exif_datetime(1_704_164_645_000),
            "2024:01:02 03:04:05"
        );
        assert_eq!(format_exif_datetime(0), "1970:01:01 00:00:00");
    }

    #[test]
    fn build_patch_normalizes_keywords_and_validates() {
        let form = EditForm {
            description: "  Una foto  ".into(),
            keywords: "Playa, playa ; Sol".into(),
            datetime_original: "2024:05:01 10:30:00".into(),
            lat: "41.38".into(),
            lon: "2.16".into(),
            ..Default::default()
        };
        let patch = build_patch(&form).unwrap();
        assert_eq!(patch.description.as_deref(), Some("Una foto"));
        assert_eq!(
            patch.keywords.as_deref(),
            Some(&["Playa".to_string(), "Sol".to_string()][..])
        );
        assert_eq!(patch.gps, Some((41.38, 2.16)));
        let bad = EditForm {
            datetime_original: "2024:13:01 00:00:00".into(),
            ..Default::default()
        };
        assert!(build_patch(&bad).is_err());
        let empty = EditForm::default();
        assert!(build_patch(&empty).unwrap().is_empty());
    }
}
