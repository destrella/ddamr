//! ddamr CLI: el mismo core que usará la UI, sin ventana.
//!
//! `ddamr scan <dir>` lista en streaming (sin cargar la carpeta
//! entera) y `ddamr doctor` verifica DB, config y herramientas.
//! Fase 2: `duplicates` lista los grupos por espacio total y
//! `archive <archivo>...` archiva con SafeOp.
use ddamr_core::{AppError, Config, SafePath, format_size};
use ddamr_fs::SafeOp;
use ddamr_index::{DuplicateKind, find_duplicates_with_root};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = run(&args);
    std::process::exit(code);
}

fn run(args: &[String]) -> i32 {
    match args.get(1).map(String::as_str) {
        Some("scan") => {
            let dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::var("HOME").map(PathBuf::from).unwrap_or_default());
            scan(&dir)
        }
        Some("index") => {
            let dir = args
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::var("HOME").map(PathBuf::from).unwrap_or_default());
            index(&dir)
        }
        Some("doctor") => doctor(),
        Some("duplicates") => duplicates(),
        Some("archive") => {
            let files: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
            if files.is_empty() {
                eprintln!("uso: ddamr archive <archivo>...");
                2
            } else {
                archive(&files)
            }
        }
        _ => {
            eprintln!(
                "uso: ddamr scan <dir> | ddamr index <dir> | ddamr doctor | ddamr duplicates | ddamr archive <archivo>..."
            );
            2
        }
    }
}

fn duplicates() -> i32 {
    let config = Config::default();
    let root = match SafePath::from_root(&config.root_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let db_path = ddamr_index::default_db_path();
    let conn = match ddamr_db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: no se pudo abrir la DB {}: {e}", db_path.display());
            return 1;
        }
    };
    match find_duplicates_with_root(&conn, &root) {
        Ok(groups) => {
            if groups.is_empty() {
                println!("Sin duplicados.");
                return 0;
            }
            let total: u64 = groups.iter().map(|g| g.total_bytes).sum();
            println!(
                "{} grupos de duplicados · {} en total",
                groups.len(),
                format_size(total)
            );
            for (i, g) in groups.iter().enumerate() {
                let kind = match g.kind {
                    DuplicateKind::Exact => "exactos",
                    DuplicateKind::ImageDHash => "similares (imagen)",
                    DuplicateKind::VideoDHash => "similares (vídeo)",
                };
                println!(
                    "{}. {} · {} archivos · {}",
                    i + 1,
                    kind,
                    g.files.len(),
                    format_size(g.total_bytes)
                );
                for f in &g.files {
                    println!("   {}  {}", format_size(f.size_bytes), f.path);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn archive(files: &[PathBuf]) -> i32 {
    let config = Config::default();
    let root = match SafePath::from_root(&config.root_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let archive_root = match SafePath::new(root.root(), &config.archive_dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: archivado no válido: {e}");
            return 1;
        }
    };
    let ops = SafeOp::new(&root);
    let mut done = 0usize;
    let mut no_date = 0usize;
    for path in files {
        let src = match SafePath::new(root.root(), path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: fuera de la raíz: {e}", path.display());
                continue;
            }
        };
        // Fecha de los metadatos nativos (EXIF si la hay).
        let date = ddamr_metadata::read_basic(&src)
            .created_at
            .and_then(ym_from_ms);
        match ops.archive_file(&src, &archive_root, date) {
            Ok(dst) => {
                println!("{} -> {}", src.path().display(), dst.path().display());
                done += 1;
            }
            Err(AppError::NoDate(_)) => {
                eprintln!("{}: sin fecha, no se archiva", path.display());
                no_date += 1;
            }
            Err(e) => eprintln!("{}: {e}", path.display()),
        }
    }
    if no_date > 0 {
        println!("No se pudieron archivar {no_date} archivos (sin fecha).");
    }
    println!("{done} archivos archivados.");
    if done == 0 && no_date == 0 { 1 } else { 0 }
}

/// (año, mes) UTC de un timestamp en milisegundos.
fn ym_from_ms(ms: i64) -> Option<(u32, u32)> {
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    if !(1970..=2100).contains(&year) {
        return None;
    }
    Some((year as u32, m as u32))
}

fn scan(dir: &std::path::Path) -> i32 {
    let config = Config::default();
    let safe = match SafePath::new(&config.root_dir, dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let mut files = 0usize;
    let mut dirs = 0usize;
    match ddamr_fs::walk(&safe) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(e) => {
                        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            dirs += 1;
                        } else {
                            files += 1;
                        }
                    }
                    Err(e) => eprintln!("advertencia: no se pudo leer una entrada: {e}"),
                }
            }
            println!(
                "{}: {} archivos, {} directorios (streaming, sin cargar la carpeta entera)",
                safe.path().display(),
                files,
                dirs
            );
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn index(dir: &std::path::Path) -> i32 {
    let config = Config::default();
    let safe = match SafePath::new(&config.root_dir, dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let db_path = ddamr_index::default_db_path();
    if let Some(parent) = db_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("error: no se pudo crear {}: {e}", parent.display());
        return 1;
    }
    println!(
        "indexando {} en {}",
        safe.path().display(),
        db_path.display()
    );
    match ddamr_index::Indexer::new().scan_dir(&safe, &db_path) {
        Ok(stats) => {
            println!(
                "index: scanned={} indexed={} errors={}",
                stats.scanned, stats.indexed, stats.errors
            );
            0
        }
        Err(e) => {
            eprintln!("index: {e}");
            1
        }
    }
}

fn doctor() -> i32 {
    let config = Config::default();
    match config.validate() {
        Ok(()) => println!("config: ok (raíz {:?})", config.root_dir),
        Err(e) => {
            eprintln!("config: {e}");
            return 1;
        }
    }
    let db_path = std::env::temp_dir().join("ddamr-doctor.db");
    match ddamr_db::open(&db_path) {
        Ok(_) => println!(
            "db: ok (SQLite WAL, user_version {})",
            ddamr_db::SCHEMA_VERSION
        ),
        Err(e) => {
            eprintln!("db: {e}");
            return 1;
        }
    }
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    println!("doctor: ok");
    0
}
