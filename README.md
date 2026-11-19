# DDAMR — DEstrella's DAM, versión Rust

Administrador de archivos digitales. Rust 1.100.0 (edición 2024), iced 0.14
(fase 1), SQLite embebido vía rusqlite 0.41.

## Principio

El core no conoce a iced. La UI solo envía comandos y recibe eventos.
Toda mutación de archivos sigue: respaldo → copia → verificar →
rename atómico → descartar respaldo.

## Estructura

```text
crates/ddamr-core/      Tipos, SafePath, errores, Config. Sin IO pesado.
crates/ddamr-db/        SQLite: migraciones, repositorios, un solo escritor.
crates/ddamr-fs/        Walk en streaming, papelera, mover/archivar seguros.
crates/ddamr-metadata/  EXIF/XMP/IPTC, xattrs macOS, regiones normalizadas.
crates/ddamr-media/     BLAKE3 en streaming, dHash, miniaturas.
crates/ddamr-index/     Cola de jobs con prioridades y cancelación.
crates/ddamr-app/       Solo iced (fase 1). Depende de todos los anteriores.
crates/ddamr-cli/       CLI con el mismo core: scan, doctor.
```

## Uso (fase 0)

```bash
cargo test -p ddamr-core
cargo test --workspace
cargo run -p ddamr-cli -- scan ~/Pictures
cargo run -p ddamr-cli -- doctor
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

Ver `../your_files/DDAMR-Analisis-y-Arquitectura.md` para el diseño completo.
