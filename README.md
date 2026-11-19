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

## Uso (fase 1)

```bash
cargo test --workspace
cargo run -p ddamr-cli -- scan ~/Pictures
cargo run -p ddamr-cli -- index ~/Pictures   # DB en ~/.cache/ddamr/ddamr.db
cargo run -p ddamr-cli -- doctor
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

La GUI (`cargo run -p ddamr-app`) necesita display: en una VM sin
ventana no abre, es esperado. Su lógica se verifica con
`cargo test -p ddamr-app` y `cargo check -p ddamr-app`.

Fase 1 incluye: árbol lazy, lista paginada (200 filas), filtros y
ordenación, indexador cancelable, lectura EXIF nativa, miniaturas
JPEG con `ddamr-media::thumbnail` y caché LRU en `ddamr-app`.
Pendiente para la siguiente iteración: widget virtualizado por
scroll, miniaturas dibujadas en la UI y reproductor de audio/vídeo.

Ver `../your_files/DDAMR-Analisis-y-Arquitectura.md` para el diseño completo.
