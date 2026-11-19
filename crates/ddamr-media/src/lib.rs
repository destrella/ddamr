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
}
