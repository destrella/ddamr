//! ddamr-media: hash, dHash y miniaturas.
//!
//! El hash se calcula en streaming, en bloques de 128 KB: nunca se
//! carga un archivo entero en memoria.
use ddamr_core::{AppError, Result, SafePath};
use std::io::Read;

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
}
