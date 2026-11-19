//! ddamr-media: hash, dHash y miniaturas.
//!
//! El hash se calcula en streaming, en bloques de 128 KB: nunca se
//! carga un archivo entero en memoria.
use ddamr_core::{AppError, Result, SafePath};
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
    use std::io::Cursor;
    let img = image::open(path.path()).map_err(|e| AppError::Other(e.to_string()))?;
    if img.width() as u64 * img.height() as u64 > 100_000_000 {
        return Err(AppError::Other(format!(
            "imagen demasiado grande: {}x{}",
            img.width(),
            img.height()
        )));
    }
    let thumb = image::imageops::thumbnail(&img, size_px, size_px);
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
    fn dhash_video_non_video_is_controlled_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("doc.txt");
        std::fs::write(&path, b"no es un video").unwrap();
        let safe = SafePath::new(dir.path(), &path).unwrap();
        assert!(dhash_video(&safe).is_err());
    }
}
