//! ddamr CLI: el mismo core que usará la UI, sin ventana.
//!
//! `ddamr scan <dir>` lista en streaming (sin cargar la carpeta
//! entera) y `ddamr doctor` verifica DB, config y herramientas.
use ddamr_core::{Config, SafePath};
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
        _ => {
            eprintln!("uso: ddamr scan <dir> | ddamr index <dir> | ddamr doctor");
            2
        }
    }
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
