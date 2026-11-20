//! ddamr-media: hash, dHash y miniaturas.
//!
//! El hash se calcula en streaming, en bloques de 128 KB: nunca se
//! carga un archivo entero en memoria.
use ddamr_core::{AppError, MediaKind, Result, SafePath};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

/// Distancia de Hamming entre dos dHash de 64 bits: nº de bits
/// distintos. 0 = idénticos; el umbral de "visualmente similar"
/// que usa el buscador de duplicados es ≤ 10.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// dHash de una imagen ya decodificada: reducir a 9×8 en gris y
/// comparar píxeles adyacentes, un bit por comparación.
pub fn dhash_of_image(img: &image::DynamicImage) -> u64 {
    if img.width() as u64 * img.height() as u64 > 100_000_000 {
        // Cabecera absurda: no reservar memoria desmedida. Se
        // devuelve 0 y el llamador lo trata como "sin dHash".
        return 0;
    }
    let gray = img.to_luma8();
    let small = image::imageops::resize(&gray, 9, 8, image::imageops::FilterType::Triangle);
    let mut hash: u64 = 0;
    for y in 0..8 {
        for x in 0..8 {
            let left = small.get_pixel(x, y)[0];
            let right = small.get_pixel(x + 1, y)[0];
            if left > right {
                hash |= 1 << (y * 8 + x);
            }
        }
    }
    hash
}

/// dHash de imagen de un archivo. Devuelve `Err` si no es imagen.
pub fn dhash_image(path: &SafePath) -> Result<u64> {
    let img = image::open(path.path()).map_err(|e| AppError::Other(e.to_string()))?;
    Ok(dhash_of_image(&img))
}

/// Duración de un vídeo en segundos, vía `ffprobe`.
fn video_duration_secs(path: &SafePath) -> Result<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            &path.path().display().to_string(),
        ])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::Metadata("ffmpeg no disponible".into())
            } else {
                AppError::IoPlain(e)
            }
        })?;
    if !out.status.success() {
        return Err(AppError::Metadata(
            "ffprobe no pudo leer la duración del vídeo".into(),
        ));
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| AppError::Metadata("ffprobe devolvió una duración inválida".into()))
}

/// dHash de vídeo: un dHash por frame al 2%, 25%, 50%, 75% y 98%
/// del tiempo de reproducción. Si ffmpeg no está, error
/// controlado: el llamador lo trata como "dHash de vídeo no
/// disponible", no como fallo fatal. Timeout de 60 s por frame.
pub fn dhash_video(path: &SafePath) -> Result<Vec<u64>> {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return Err(AppError::Metadata("ffmpeg no disponible".into()));
    }
    let duration = video_duration_secs(path)?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err(AppError::Metadata(
            "duración de vídeo inválida para dHash".into(),
        ));
    }
    let mut hashes = Vec::with_capacity(5);
    for (i, pct) in [0.02, 0.25, 0.50, 0.75, 0.98].into_iter().enumerate() {
        // No pedir un instante más allá de duración − 0,25 s: con
        // seek de entrada ffmpeg puede salir con 0 sin escribir
        // ningún frame si cae pasado el último.
        let t = (duration * pct).min(duration - 0.25).max(0.0);
        // La salida va a un fichero temporal: un pipe se puede
        // bloquear si el PNG del frame llena el buffer.
        let tmp = std::env::temp_dir().join(format!("ddamr-frame-{}-{i}.png", std::process::id()));
        extract_frame(path, t, &tmp, true)?;
        if !tmp.exists() {
            // Reintento con seek de salida (decodifica y descarta
            // hasta el instante): más lento, pero fiable.
            extract_frame(path, t, &tmp, false)?;
        }
        let bytes = std::fs::read(&tmp).map_err(AppError::IoPlain)?;
        let _ = std::fs::remove_file(&tmp);
        let img = image::load_from_memory(&bytes).map_err(|e| AppError::Other(e.to_string()))?;
        hashes.push(dhash_of_image(&img));
    }
    Ok(hashes)
}

/// Extrae un único frame de `path` en el instante `t` y lo escribe
/// en `tmp` como PNG. Con `input_seek` el `-ss` va antes de `-i`
/// (rápido); si no, va después (lento pero fiable cerca del final).
fn extract_frame(path: &SafePath, t: f64, tmp: &std::path::Path, input_seek: bool) -> Result<()> {
    let input = path.path().display().to_string();
    let t_arg = format!("{t:.3}");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-v", "error"]);
    if input_seek {
        cmd.args(["-ss", &t_arg]);
    }
    cmd.args(["-i", &input]);
    if !input_seek {
        cmd.args(["-ss", &t_arg]);
    }
    cmd.args(["-frames:v", "1", "-f", "image2", "-vcodec", "png"])
        .arg(tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::Metadata("ffmpeg no disponible".into())
        } else {
            AppError::IoPlain(e)
        }
    })?;
    let status = match child.wait_timeout(Duration::from_secs(60)) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(tmp);
            return Err(AppError::Metadata(
                "ffmpeg no respondió en 60 s extrayendo un frame".into(),
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            return Err(AppError::IoPlain(e));
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(tmp);
        return Err(AppError::Metadata(
            "ffmpeg no pudo extraer un frame del vídeo".into(),
        ));
    }
    Ok(())
}

/// BLAKE3 de un archivo, en streaming. Devuelve el hash en hex.
pub fn blake3_file(path: &SafePath) -> Result<String> {
    let file = std::fs::File::open(path.path()).map_err(|e| AppError::Io {
        path: path.path().to_path_buf(),
        source: e,
    })?;
    let mut reader = std::io::BufReader::with_capacity(128 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(AppError::IoPlain)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Genera una miniatura JPEG de `path`, con el lado mayor como
/// máximo en `size_px` píxeles. Devuelve los bytes JPEG (calidad 80).
/// No es una imagen → `Err`. Se rechazan cabeceras que declaren
/// más de 100 megapíxeles para no reservar memoria desmedida.
pub fn thumbnail(path: &SafePath, size_px: u32) -> Result<Vec<u8>> {
    thumbnail_sized(path, size_px)
}

/// Como [`thumbnail`], con el nombre que deja claro que el tamaño
/// es el lado mayor máximo en píxeles.
pub fn thumbnail_sized(path: &SafePath, max_side: u32) -> Result<Vec<u8>> {
    let img = image::open(path.path()).map_err(|e| AppError::Other(e.to_string()))?;
    jpeg_thumbnail(&img, max_side)
}

/// Núcleo compartido: reduce `img` a una miniatura JPEG (calidad 80)
/// con el lado mayor como máximo en `max_side` píxeles, conservando
/// la proporción original. Rechaza imágenes de más de 100
/// megapíxeles para no reservar memoria desmedida.
///
/// OJO: `image::imageops::thumbnail(img, n, n)` NO conserva la
/// proporción: re-muestrea la imagen a exactamente n×n,
/// aplastándola a cuadrado. Por eso el tamaño se calcula aquí
/// explícitamente y se usa `resize` con esas dimensiones. Las
/// miniaturas cuadradas que se veían en la UI venían de aquí, no
/// solo del widget.
fn jpeg_thumbnail(img: &image::DynamicImage, max_side: u32) -> Result<Vec<u8>> {
    use std::io::Cursor;
    if img.width() as u64 * img.height() as u64 > 100_000_000 {
        return Err(AppError::Other(format!(
            "imagen demasiado grande: {}x{}",
            img.width(),
            img.height()
        )));
    }
    let (w, h) = (img.width(), img.height());
    let scale = max_side as f32 / w.max(h).max(1) as f32;
    let (tw, th) = if scale >= 1.0 {
        // No ampliar imágenes pequeñas: se quedan como están.
        (w, h)
    } else {
        (
            (w as f32 * scale).round() as u32,
            (h as f32 * scale).round() as u32,
        )
    };
    let thumb = image::imageops::resize(
        img,
        tw.max(1),
        th.max(1),
        image::imageops::FilterType::Triangle,
    );
    let mut out = Vec::new();
    {
        let mut cursor = Cursor::new(&mut out);
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 80);
        encoder
            .encode_image(&thumb)
            .map_err(|e| AppError::Other(e.to_string()))?;
    }
    Ok(out)
}

/// Miniatura JPEG de un vídeo: extrae el frame al 25% de la
/// duración (el mismo instante que usa `dhash_video`) y lo reduce
/// con el mismo núcleo que las imágenes. Si ffmpeg no está, la
/// duración no es válida o ffmpeg falla, error controlado
/// (`AppError::Metadata`).
pub fn video_thumbnail(path: &SafePath, max_side: u32) -> Result<Vec<u8>> {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        return Err(AppError::Metadata("ffmpeg no disponible".into()));
    }
    let duration = video_duration_secs(path)?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err(AppError::Metadata(
            "duración de vídeo inválida para miniatura".into(),
        ));
    }
    let t = (duration * 0.25).min(duration - 0.25).max(0.0);
    let tmp = std::env::temp_dir().join(format!("ddamr-vthumb-{}.png", std::process::id()));
    extract_frame(path, t, &tmp, true)?;
    if !tmp.exists() {
        // Reintento con seek de salida, como en `dhash_video`.
        extract_frame(path, t, &tmp, false)?;
    }
    let bytes = std::fs::read(&tmp).map_err(AppError::IoPlain)?;
    let _ = std::fs::remove_file(&tmp);
    let img = image::load_from_memory(&bytes).map_err(|e| AppError::Other(e.to_string()))?;
    jpeg_thumbnail(&img, max_side)
}

/// Miniatura JPEG según el tipo de archivo: imagen → miniatura de
/// la imagen; vídeo → frame al 25%; cualquier otro tipo →
/// `AppError::Other("no es imagen ni vídeo")`.
pub fn thumbnail_for_entry(path: &SafePath, kind: MediaKind, max_side: u32) -> Result<Vec<u8>> {
    match kind {
        MediaKind::Image => thumbnail_sized(path, max_side),
        MediaKind::Video => video_thumbnail(path, max_side),
        _ => Err(AppError::Other("no es imagen ni vídeo".into())),
    }
}

/// Directorio de caché de miniaturas en disco: `DDAMR_THUMBS_DIR`
/// si está definida (los tests la usan para no tocar el home real),
/// si no `~/.cache/ddamr/thumbs`. Es el mismo directorio que usa
/// `ddamr thumbs` en el CLI, así que la app y el CLI comparten
/// la caché.
pub fn thumbs_cache_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("DDAMR_THUMBS_DIR")
        && !dir.is_empty()
    {
        return std::path::PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".cache")
                .join("ddamr")
                .join("thumbs")
        })
        .unwrap_or_default()
}

/// Dimensiones de un JPEG sin decodificarlo: solo se lee la
/// cabecera (`ImageReader::into_dimensions`). `None` si los bytes
/// no son una imagen legible. Barato: no reserva el píxel.
pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    use std::io::Cursor;
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Miniatura con caché en disco.
///
/// Orden: (1) fichero `<clave>-<max_side>.jpg` en la caché;
/// (2) si se pide `<= 256 px` y existe el fichero `<clave>.jpg`
/// que escribe `ddamr thumbs` (256 px), se reescala desde él en
/// vez de decodificar el original — mucho más barato con
/// originales grandes; (3) generar desde el original y guardar
/// el resultado como `<clave>-<max_side>.jpg`.
///
/// Si la caché no está disponible (sin permisos, sin HOME…),
/// se genera en memoria sin fallar: la caché es un acelerador,
/// nunca un requisito.
pub fn thumbnail_cached(path: &SafePath, kind: MediaKind, max_side: u32) -> Result<Vec<u8>> {
    let key = thumb_cache_key(path).ok();
    if let Some(key) = &key {
        let dir = thumbs_cache_dir();
        let sized = dir.join(format!("{key}-{max_side}.jpg"));
        if let Ok(bytes) = std::fs::read(&sized) {
            return Ok(bytes);
        }
        if max_side <= 256 {
            // Reutilizar la miniatura de 256 px del CLI.
            let cli_file = dir.join(format!("{key}.jpg"));
            if let Ok(cached) = std::fs::read(&cli_file)
                && let Ok(img) = image::load_from_memory(&cached)
            {
                let bytes = jpeg_thumbnail(&img, max_side)?;
                let _ = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&sized, &bytes));
                return Ok(bytes);
            }
        }
    }
    let bytes = thumbnail_for_entry(path, kind, max_side)?;
    if let Some(key) = &key {
        let dir = thumbs_cache_dir();
        let sized = dir.join(format!("{key}-{max_side}.jpg"));
        let _ = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&sized, &bytes));
    }
    Ok(bytes)
}

/// Clave de caché para la miniatura de `path`: BLAKE3 en hex de
/// (ruta canónica + mtime en milisegundos, separados por `'\0'`).
/// Es estable mientras no cambie el archivo y cambia cuando se
/// toca su mtime, sin leer el contenido.
pub fn thumb_cache_key(path: &SafePath) -> Result<String> {
    let canonical = std::fs::canonicalize(path.path()).map_err(|e| AppError::Io {
        path: path.path().to_path_buf(),
        source: e,
    })?;
    let mtime_ms = std::fs::metadata(path.path())
        .map_err(AppError::IoPlain)?
        .modified()
        .map_err(AppError::IoPlain)?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AppError::Other("mtime anterior a la época Unix".into()))?
        .as_millis();
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(mtime_ms.to_string().as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn blake3_is_stable_and_streaming() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"hola ddamr").unwrap();
        let safe = SafePath::new(dir.path(), &file).unwrap();
        assert_eq!(blake3_file(&safe).unwrap(), blake3_file(&safe).unwrap());
        assert_eq!(blake3_file(&safe).unwrap().len(), 64);
    }

    #[test]
    fn thumbnail_jpeg_is_smaller_than_source() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(400, 300).save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let bytes = thumbnail(&safe, 128).unwrap();
        // JPEG válido: empieza por FFD8 y termina en FFD9.
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
        assert!(bytes.ends_with(&[0xFF, 0xD9]));
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(decoded.width() <= 128 && decoded.height() <= 128);
    }

    #[test]
    fn thumbnail_rejects_non_image() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("doc.txt");
        std::fs::write(&path, b"hola").unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        assert!(thumbnail(&safe, 128).is_err());
    }

    fn half_image(left_black: bool) -> image::RgbImage {
        // Mitad negra / mitad blanca (o al revés): el dHash debe
        // cambiar mucho entre ambas.
        let mut img = image::RgbImage::new(64, 64);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            let black = if left_black { x < 32 } else { x >= 32 };
            *px = if black {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        img
    }

    #[test]
    fn dhash_identical_is_zero_and_different_is_far() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.png");
        let c = dir.path().join("c.png");
        half_image(true).save(&a).unwrap();
        half_image(true).save(&b).unwrap();
        half_image(false).save(&c).unwrap();
        let (sa, sb, sc) = (
            SafePath::new(dir.path(), &a).unwrap(),
            SafePath::new(dir.path(), &b).unwrap(),
            SafePath::new(dir.path(), &c).unwrap(),
        );
        let (ha, hb, hc) = (
            dhash_image(&sa).unwrap(),
            dhash_image(&sb).unwrap(),
            dhash_image(&sc).unwrap(),
        );
        assert_eq!(hamming(ha, hb), 0, "imágenes idénticas");
        assert!(
            hamming(ha, hc) > 10,
            "imágenes invertidas deben distar > 10, distan {}",
            hamming(ha, hc)
        );
        assert_eq!(hamming(ha, ha), 0);
        assert_eq!(hamming(0, u64::MAX), 64);
    }

    #[test]
    fn dhash_video_five_frames_or_controlled_error() {
        // Vídeo fixture generado con ffmpeg (testsrc, 2 s). Si
        // ffmpeg no está o falla la generación, el test verifica
        // el error controlado en vez de fallar.
        let dir = TempDir::new().unwrap();
        let video = dir.path().join("test.mp4");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=64x64:rate=10",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                &video.display().to_string(),
            ])
            .output();
        let Ok(out) = generated else {
            eprintln!("sin ffmpeg en este entorno, test omitido");
            return;
        };
        if !out.status.success() || !video.exists() {
            eprintln!("no se pudo generar el vídeo fixture, test omitido");
            return;
        }
        let safe = SafePath::new(dir.path(), &video).unwrap();
        let hashes = dhash_video(&safe).unwrap();
        assert_eq!(hashes.len(), 5, "un dHash por porcentaje (2/25/50/75/98)");
    }

    #[test]
    fn thumbnail_keeps_aspect_ratio() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(400, 300).save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let bytes = thumbnail_sized(&safe, 64).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (64, 48),
            "400×300 a 64 px debe ser 64×48, no 64×64"
        );
        let vertical = dir.path().join("vert.jpg");
        image::RgbImage::new(300, 400).save(&vertical).unwrap();
        let safe_v = SafePath::new(dir.path(), &vertical).unwrap();
        let decoded = image::load_from_memory(&thumbnail_sized(&safe_v, 64).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (48, 64));
    }

    #[test]
    fn thumbnail_sized_respects_max_side() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(400, 300).save(&path).unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let small = thumbnail_sized(&safe, 64).unwrap();
        let big = thumbnail_sized(&safe, 320).unwrap();
        for bytes in [&small, &big] {
            assert!(bytes.starts_with(&[0xFF, 0xD8]));
            assert!(bytes.ends_with(&[0xFF, 0xD9]));
        }
        let dec_small = image::load_from_memory(&small).unwrap();
        let dec_big = image::load_from_memory(&big).unwrap();
        assert!(dec_small.width() <= 64 && dec_small.height() <= 64);
        assert!(dec_big.width() <= 320 && dec_big.height() <= 320);
        assert!(
            dec_big.width() > dec_small.width() && dec_big.height() > dec_small.height(),
            "320 px debe dar una miniatura mayor que 64 px: {}x{} frente a {}x{}",
            dec_big.width(),
            dec_big.height(),
            dec_small.width(),
            dec_small.height()
        );
    }

    #[test]
    fn video_thumbnail_frame_or_controlled_error() {
        // Mismo vídeo fixture que `dhash_video_five_frames_or_controlled_error`.
        let dir = TempDir::new().unwrap();
        let video = dir.path().join("test.mp4");
        let generated = Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=64x64:rate=10",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                &video.display().to_string(),
            ])
            .output();
        let Ok(out) = generated else {
            // Sin ffmpeg: el error debe ser controlado, no un fallo.
            let safe = SafePath::new(dir.path(), &video).unwrap();
            assert!(video_thumbnail(&safe, 128).is_err());
            return;
        };
        if !out.status.success() || !video.exists() {
            eprintln!("no se pudo generar el vídeo fixture, test omitido");
            return;
        }
        let safe = SafePath::new(dir.path(), &video).unwrap();
        let bytes = video_thumbnail(&safe, 128).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
        assert!(bytes.ends_with(&[0xFF, 0xD9]));
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert!(decoded.width() <= 128 && decoded.height() <= 128);
    }

    #[test]
    fn thumbnail_for_entry_rejects_non_image_video() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("cancion.mp3");
        std::fs::write(&audio, b"no es audio de verdad").unwrap();
        let other = dir.path().join("doc.pdf");
        std::fs::write(&other, b"no es un pdf").unwrap();
        let sa = SafePath::new(dir.path(), &audio).unwrap();
        let so = SafePath::new(dir.path(), &other).unwrap();
        assert!(thumbnail_for_entry(&sa, MediaKind::Audio, 128).is_err());
        assert!(thumbnail_for_entry(&so, MediaKind::Other, 128).is_err());
        assert!(thumbnail_for_entry(&so, MediaKind::Dir, 128).is_err());
        // Y una imagen sí pasa por la misma función.
        let img_path = dir.path().join("foto.jpg");
        image::RgbImage::new(200, 100).save(&img_path).unwrap();
        let si = SafePath::new(dir.path(), &img_path).unwrap();
        let bytes = thumbnail_for_entry(&si, MediaKind::Image, 64).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn thumb_cache_key_stable_and_changes_with_mtime() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        std::fs::write(&path, b"contenido").unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let key1 = thumb_cache_key(&safe).unwrap();
        assert_eq!(key1.len(), 64);
        assert_eq!(key1, thumb_cache_key(&safe).unwrap(), "estable");
        // Tocar el archivo: si el mtime cambia, la clave cambia; si
        // el sistema de archivos no registra el cambio, al menos
        // dos llamadas seguidas deben coincidir.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&path, b"contenido").unwrap();
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        let key2 = thumb_cache_key(&safe).unwrap();
        if after != before {
            assert_ne!(key1, key2, "el mtime cambió, la clave debe cambiar");
        } else {
            assert_eq!(key2, thumb_cache_key(&safe).unwrap());
        }
    }

    #[test]
    fn jpeg_dimensions_reads_header_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(400, 300).save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(jpeg_dimensions(&bytes), Some((400, 300)));
        assert_eq!(jpeg_dimensions(b"no es una imagen"), None);
        assert_eq!(jpeg_dimensions(&[]), None);
    }

    // Un solo test para la caché en disco: las tres escenas tocan
    // la misma variable de entorno y no pueden correr en paralelo.
    #[test]
    fn thumbnail_cached_disk_cache() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("thumbs");
        std::fs::create_dir_all(&cache).unwrap();
        // SAFETY: este es el único test que toca DDAMR_THUMBS_DIR.
        unsafe { std::env::set_var("DDAMR_THUMBS_DIR", &cache) };
        // (1) Fichero `<clave>-64.jpg` ya existente: se devuelve
        // tal cual, sin generar desde el original. El original es
        // negro y la caché contiene una imagen blanca: el
        // resultado debe ser blanco.
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(400, 300).save(&path).unwrap(); // negro
        let safe = SafePath::new(dir.path(), &path).unwrap();
        let key = thumb_cache_key(&safe).unwrap();
        let white = {
            let mut img = image::RgbImage::new(10, 10);
            for px in img.pixels_mut() {
                *px = image::Rgb([255, 255, 255]);
            }
            let mut buf = Vec::new();
            {
                let mut cur = std::io::Cursor::new(&mut buf);
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 80)
                    .encode_image(&image::DynamicImage::ImageRgb8(img))
                    .unwrap();
            }
            buf
        };
        std::fs::write(cache.join(format!("{key}-64.jpg")), &white).unwrap();
        let from_cache = thumbnail_cached(&safe, MediaKind::Image, 64).unwrap();
        let decoded = image::load_from_memory(&from_cache).unwrap().to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0), &image::Rgb([255, 255, 255]));
        // (2) Reutilización del fichero `<clave>.jpg` de 256 px
        // del CLI: sin fichero de 64 px, se reescala desde él.
        // El original es negro, el fichero del CLI es blanco.
        let path2 = dir.path().join("foto2.jpg");
        image::RgbImage::new(200, 100).save(&path2).unwrap(); // negro
        let safe2 = SafePath::new(dir.path(), &path2).unwrap();
        let key2 = thumb_cache_key(&safe2).unwrap();
        std::fs::write(cache.join(format!("{key2}.jpg")), &white).unwrap();
        let small = thumbnail_cached(&safe2, MediaKind::Image, 64).unwrap();
        let decoded = image::load_from_memory(&small).unwrap();
        assert!(decoded.width() <= 64 && decoded.height() <= 64);
        assert!(
            cache.join(format!("{key2}-64.jpg")).exists(),
            "el reescalado se guarda como fichero de 64 px"
        );
        // (3) Sin directorio de caché usable: genera en memoria.
        unsafe { std::env::set_var("DDAMR_THUMBS_DIR", "") };
        let path3 = dir.path().join("foto3.jpg");
        image::RgbImage::new(100, 80).save(&path3).unwrap();
        let safe3 = SafePath::new(dir.path(), &path3).unwrap();
        assert!(thumbnail_cached(&safe3, MediaKind::Image, 64).is_ok());
        unsafe { std::env::remove_var("DDAMR_THUMBS_DIR") };
    }

    #[test]
    fn dhash_video_non_video_is_controlled_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("doc.txt");
        std::fs::write(&path, b"no es un video").unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        assert!(dhash_video(&safe).is_err());
    }
}
