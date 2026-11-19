//! Buscador de duplicados (fase 2).
//!
//! Estrategia, en orden de coste:
//! 1. Agrupar por `size_bytes` en SQL: solo los tamaños con más
//!    de un archivo pueden ser duplicados.
//! 2. Dentro de cada grupo, exactos por `hash_blake3` (BLAKE3 en
//!    streaming; nunca se carga un archivo entero).
//! 3. Los que no son exactos: imágenes por dHash (Hamming ≤ 10),
//!    vídeos por dHash de frames (media de Hamming ≤ 10).
//!
//! `find_duplicates` usa solo lo ya guardado en la DB;
//! `find_duplicates_with_root` calcula y guarda los hashes que
//! falten antes de comparar (es la que usa la app y la CLI).

use ddamr_core::{AppError, MediaKind, Result, SafePath};
use ddamr_db::{FileHashes, update_file_hashes};
use ddamr_media::{dhash_image, dhash_video, hamming};
use rusqlite::Connection;

/// Umbral de similitud visual: Hamming ≤ 10 es "visualmente similar".
pub const DHASH_THRESHOLD: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateKind {
    Exact,
    ImageDHash,
    VideoDHash,
}

/// Un archivo tal como lo ve el buscador de duplicados.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    /// Ruta relativa a la raíz (la que guarda la DB).
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub kind: MediaKind,
    pub hash_blake3: Option<String>,
    pub dhash_image: Option<u64>,
    pub dhash_video: Option<Vec<u64>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<i64>,
    /// Fecha de creación (ms desde epoch; EXIF si la hay).
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    pub kind: DuplicateKind,
    pub files: Vec<FileInfo>,
    /// Suma de los tamaños de todos los archivos del grupo.
    pub total_bytes: u64,
}

impl DuplicateGroup {
    fn new(kind: DuplicateKind, files: Vec<FileInfo>) -> Self {
        let total_bytes = files.iter().map(|f| f.size_bytes).sum();
        Self {
            kind,
            files,
            total_bytes,
        }
    }
}

/// Candidatos: archivos cuyo tamaño aparece más de una vez.
fn load_candidates(conn: &Connection) -> Result<Vec<FileInfo>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, name, size_bytes, kind, hash_blake3, dhash_image,
                    dhash_video, width, height, duration_ms, created_at, modified_at
             FROM files
             WHERE size_bytes IN (
                 SELECT size_bytes FROM files GROUP BY size_bytes HAVING COUNT(*) > 1
             )
             ORDER BY size_bytes, path",
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows = stmt
        .query_map([], |r| {
            let dhash_video: Option<String> = r.get(6)?;
            Ok(FileInfo {
                path: r.get(0)?,
                name: r.get(1)?,
                size_bytes: r.get::<_, i64>(2)? as u64,
                kind: MediaKind::from_db_str(&r.get::<_, String>(3)?),
                hash_blake3: r.get(4)?,
                dhash_image: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                dhash_video: dhash_video.map(|s| {
                    s.split(',')
                        .filter_map(|p| p.trim().parse::<u64>().ok())
                        .collect()
                }),
                width: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
                height: r.get::<_, Option<i64>>(8)?.map(|v| v as u32),
                duration_ms: r.get(9)?,
                created_at: r.get(10)?,
                modified_at: r.get(11)?,
            })
        })
        .map_err(|e| AppError::Database(e.to_string()))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| AppError::Database(e.to_string()))
}

/// Calcula (y guarda) el BLAKE3 de los archivos del grupo que no
/// lo tengan. Sin raíz no se puede calcular: se dejan sin hash.
fn fill_missing_hashes(conn: &Connection, root: Option<&SafePath>, files: &mut [FileInfo]) {
    let Some(root) = root else { return };
    for f in files.iter_mut() {
        if f.hash_blake3.is_some() {
            continue;
        }
        let Ok(safe) = root.join(std::path::Path::new(&f.path)) else {
            continue;
        };
        if let Ok(h) = ddamr_media::blake3_file(&safe) {
            let _ = update_file_hashes(
                conn,
                &f.path,
                &FileHashes {
                    hash_blake3: Some(h.clone()),
                    ..Default::default()
                },
            );
            f.hash_blake3 = Some(h);
        }
    }
}

/// Calcula (y guarda) el dHash de imagen de los archivos que no
/// lo tengan. Un archivo que no es imagen decodificable se ignora.
fn fill_missing_image_dhash(conn: &Connection, root: Option<&SafePath>, files: &mut [FileInfo]) {
    let Some(root) = root else { return };
    for f in files.iter_mut() {
        if f.kind != MediaKind::Image || f.dhash_image.is_some() {
            continue;
        }
        let Ok(safe) = root.join(std::path::Path::new(&f.path)) else {
            continue;
        };
        if let Ok(h) = dhash_image(&safe) {
            let _ = update_file_hashes(
                conn,
                &f.path,
                &FileHashes {
                    dhash_image: Some(h as i64),
                    ..Default::default()
                },
            );
            f.dhash_image = Some(h);
        }
    }
}

/// Calcula (y guarda) el dHash de vídeo. Si ffmpeg no está, el
/// archivo queda sin dHash: "no disponible", no un fallo fatal.
fn fill_missing_video_dhash(conn: &Connection, root: Option<&SafePath>, files: &mut [FileInfo]) {
    let Some(root) = root else { return };
    for f in files.iter_mut() {
        if f.kind != MediaKind::Video || f.dhash_video.is_some() {
            continue;
        }
        let Ok(safe) = root.join(std::path::Path::new(&f.path)) else {
            continue;
        };
        match dhash_video(&safe) {
            Ok(hashes) => {
                let joined = hashes
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let _ = update_file_hashes(
                    conn,
                    &f.path,
                    &FileHashes {
                        dhash_video: Some(joined),
                        ..Default::default()
                    },
                );
                f.dhash_video = Some(hashes);
            }
            Err(e) => {
                eprintln!(
                    "duplicados: dHash de vídeo no disponible para {}: {e}",
                    f.path
                );
            }
        }
    }
}

/// Agrupa por similitud: cada archivo se une al primer grupo
/// existente con el que sea similar, o abre uno nuevo.
fn cluster_by<F>(files: Vec<FileInfo>, similar: F) -> Vec<Vec<FileInfo>>
where
    F: Fn(&FileInfo, &FileInfo) -> bool,
{
    let mut groups: Vec<Vec<FileInfo>> = Vec::new();
    for f in files {
        let mut placed = false;
        for g in groups.iter_mut() {
            if g.iter().any(|m| similar(m, &f)) {
                g.push(f.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push(vec![f]);
        }
    }
    groups.retain(|g| g.len() > 1);
    groups
}

fn video_frames_similar(a: &[u64], b: &[u64]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let sum: u32 = a.iter().zip(b.iter()).map(|(x, y)| hamming(*x, *y)).sum();
    sum as f64 / a.len() as f64 <= DHASH_THRESHOLD as f64
}

fn find_duplicates_inner(
    conn: &Connection,
    root: Option<&SafePath>,
) -> Result<Vec<DuplicateGroup>> {
    let candidates = load_candidates(conn)?;
    let mut groups: Vec<DuplicateGroup> = Vec::new();
    // Agrupar por tamaño: los de distinto tamaño no se comparan.
    let mut by_size: Vec<(u64, Vec<FileInfo>)> = Vec::new();
    for f in candidates {
        match by_size.iter_mut().find(|(s, _)| *s == f.size_bytes) {
            Some((_, v)) => v.push(f),
            None => by_size.push((f.size_bytes, vec![f])),
        }
    }
    for (_size, mut files) in by_size {
        fill_missing_hashes(conn, root, &mut files);
        // 1) Exactos por BLAKE3.
        let mut by_hash: Vec<(String, Vec<FileInfo>)> = Vec::new();
        let mut rest: Vec<FileInfo> = Vec::new();
        for f in files {
            match &f.hash_blake3 {
                Some(h) => match by_hash.iter_mut().find(|(k, _)| k == h) {
                    Some((_, v)) => v.push(f),
                    None => by_hash.push((h.clone(), vec![f])),
                },
                None => rest.push(f),
            }
        }
        for (_, g) in by_hash {
            if g.len() > 1 {
                groups.push(DuplicateGroup::new(DuplicateKind::Exact, g));
            } else {
                // Hash único: pasa a la comparación visual.
                rest.push(g.into_iter().next().unwrap());
            }
        }
        // 2) Imágenes visualmente similares por dHash.
        let (images, videos): (Vec<FileInfo>, Vec<FileInfo>) =
            rest.into_iter().partition(|f| f.kind == MediaKind::Image);
        let mut images = images;
        fill_missing_image_dhash(conn, root, &mut images);
        let with_dhash: Vec<FileInfo> = images
            .into_iter()
            .filter(|f| f.dhash_image.is_some())
            .collect();
        for g in cluster_by(with_dhash, |a, b| {
            hamming(a.dhash_image.unwrap(), b.dhash_image.unwrap()) <= DHASH_THRESHOLD
        }) {
            groups.push(DuplicateGroup::new(DuplicateKind::ImageDHash, g));
        }
        // 3) Vídeos visualmente similares por dHash de frames.
        let mut videos = videos;
        fill_missing_video_dhash(conn, root, &mut videos);
        let with_vdhash: Vec<FileInfo> = videos
            .into_iter()
            .filter(|f| f.dhash_video.is_some())
            .collect();
        for g in cluster_by(with_vdhash, |a, b| {
            video_frames_similar(
                a.dhash_video.as_ref().unwrap(),
                b.dhash_video.as_ref().unwrap(),
            )
        }) {
            groups.push(DuplicateGroup::new(DuplicateKind::VideoDHash, g));
        }
    }
    groups.sort_by_key(|a| std::cmp::Reverse(a.total_bytes));
    Ok(groups)
}

/// Duplicados según lo ya guardado en la DB (sin calcular hashes
/// que falten). Para calcularlos, usar `find_duplicates_with_root`.
pub fn find_duplicates(conn: &Connection) -> Result<Vec<DuplicateGroup>> {
    find_duplicates_inner(conn, None)
}

/// Duplicados calculando (y guardando en la DB) los hashes que
/// falten para los archivos candidatos.
pub fn find_duplicates_with_root(
    conn: &Connection,
    root: &SafePath,
) -> Result<Vec<DuplicateGroup>> {
    find_duplicates_inner(conn, Some(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Indexer;
    use ddamr_db::{insert_files_batch, open};
    use ddamr_fs::FileEntry;
    use tempfile::TempDir;

    fn setup(files: &[(&str, &[u8])]) -> (TempDir, TempDir, Connection) {
        let root = TempDir::new().unwrap();
        for (name, content) in files {
            let p = root.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        let db_dir = TempDir::new().unwrap();
        let db = db_dir.path().join("t.db");
        let conn = open(&db).unwrap();
        // Indexar recursivamente con el propio Indexer.
        let safe = SafePath::from_root(root.path()).unwrap();
        Indexer::new().scan_dir(&safe, &db).unwrap();
        (root, db_dir, conn)
    }

    #[test]
    fn exact_duplicates_with_root() {
        let (root, _dbdir, conn) = setup(&[
            ("a.jpg", b"contenido identico 123"),
            ("sub/b.jpg", b"contenido identico 123"),
            ("c.jpg", b"contenido DISTINTO 12"),
            ("d.jpg", b"corto"),
        ]);
        let safe = SafePath::from_root(root.path()).unwrap();
        let groups = find_duplicates_with_root(&conn, &safe).unwrap();
        assert_eq!(groups.len(), 1, "solo el par idéntico: {groups:?}");
        assert_eq!(groups[0].kind, DuplicateKind::Exact);
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(
            groups[0].total_bytes,
            2 * "contenido identico 123".len() as u64
        );
        // El hash quedó guardado: una segunda pasada sin raíz ya
        // encuentra el grupo con lo almacenado.
        assert_eq!(find_duplicates(&conn).unwrap().len(), 1);
    }

    #[test]
    fn image_dhash_groups_similar_images() {
        use image::{ImageBuffer, Rgb};
        let root = TempDir::new().unwrap();
        // Dos mitades iguales con un píxel distinto: dHash casi igual.
        let mut a: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(64, 64);
        let mut b: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(64, 64);
        for (x, _y, px) in a.enumerate_pixels_mut() {
            *px = if x < 32 {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            };
        }
        for (x, _y, px) in b.enumerate_pixels_mut() {
            *px = if x < 32 {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            };
        }
        b.put_pixel(63, 63, Rgb([250, 250, 250]));
        // BMP sin comprimir: mismo tamaño de archivo garantizado
        // (un PNG cambiaría de tamaño por un solo píxel y el
        // algoritmo agrupa primero por tamaño).
        a.save(root.path().join("a.bmp")).unwrap();
        b.save(root.path().join("b.bmp")).unwrap();
        // Tercera imagen distinta (invertida) con el mismo tamaño
        // de archivo es improbable; basta con que no se agrupe.
        let db_dir = TempDir::new().unwrap();
        let mut conn = open(&db_dir.path().join("t.db")).unwrap();
        let entries: Vec<FileEntry> = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| FileEntry::from_dir_entry(root.path(), &e).ok())
            .collect();
        insert_files_batch(&mut conn, &entries).unwrap();
        let safe = SafePath::from_root(root.path()).unwrap();
        let groups = find_duplicates_with_root(&conn, &safe).unwrap();
        assert_eq!(groups.len(), 1, "las dos mitades son similares: {groups:?}");
        assert_eq!(groups[0].kind, DuplicateKind::ImageDHash);
    }

    #[test]
    fn no_duplicates_when_all_different_sizes() {
        let (_root, _dbdir, conn) = setup(&[("a.jpg", b"1"), ("b.jpg", b"12"), ("c.jpg", b"123")]);
        assert!(find_duplicates(&conn).unwrap().is_empty());
    }
}
