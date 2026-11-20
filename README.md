# DDAMR — DEstrella's DAM, versión Rust

Administrador de archivos digitales: navega, indexa, etiqueta y
organiza fotos, vídeos y audio sin perder nunca un archivo.

Rust 1.100.0 (edición 2024), interfaz con iced 0.14, base de datos
SQLite embebida (rusqlite 0.41, SQLite 3.54 con WAL).

## Principios

- **El core no conoce a iced.** La interfaz solo envía comandos y
  recibe eventos; todo lo demás funciona también desde el CLI.
- **Ningún archivo se pierde.** Toda mutación sigue el patrón
  respaldo → trabajar sobre una copia → verificar → rename
  atómico → descartar el respaldo. Si algo falla, el original se
  restaura desde el respaldo. La papelera del sistema se usa en
  vez de borrar.
- **La raíz está dentro del home del usuario** y todas las rutas
  pasan por `SafePath`, que canonicaliza y rechaza escapes (`..`,
  symlinks que salen de la raíz).
- **Eficiencia de memoria.** Recorridos en streaming, listados
  paginados, lista virtualizada (solo se construyen las filas
  visibles) y hashes BLAKE3 en streaming, sin cargar archivos
  enteros. Una carpeta con decenas de miles de archivos no debe
  agotar la memoria.

## Requisitos

- Rust 1.100.0 o superior. El archivo `rust-toolchain.toml` fija
  la versión; con [rustup](https://rustup.rs) se instala sola al
  compilar:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- Opcionales, solo para funciones concretas (sin ellos la app
  funciona y avisa con un error controlado, no se rompe):
  - [ExifTool](https://exiftool.org/) — escribir metadatos
    (descripción, palabras clave, copyright, fecha, GPS).
  - [ffmpeg](https://ffmpeg.org/) — miniaturas y dHash de vídeo.

## Instalación y uso

```bash
git clone https://github.com/destrella/ddamr.git
cd ddamr
cargo run -p ddamr-app        # interfaz gráfica (necesita display)
```

La interfaz abre en el home del usuario: árbol de directorios
lazy a la izquierda, panel de contenido en el centro y columna
de metadatos a la derecha. Atajos: flechas para moverse, espacio
para seleccionar, `Anterior`/`Siguiente` para cambiar de página.

## CLI

El mismo core desde la terminal. El binario se llama `ddamr`
(paquete `ddamr-cli`):

```bash
cargo run -q -p ddamr-cli -- scan ~/Pictures        # lista en streaming
cargo run -q -p ddamr-cli -- index ~/Pictures       # indexa en la DB
cargo run -q -p ddamr-cli -- duplicates             # grupos de duplicados
cargo run -q -p ddamr-cli -- thumbs ~/Pictures      # pre-genera miniaturas
cargo run -q -p ddamr-cli -- archive foto.jpg ...   # archiva en AÑO/MES
cargo run -q -p ddamr-cli -- doctor                 # verifica config y DB
```

Variables de entorno:

| Variable | Defecto | Uso |
| --- | --- | --- |
| `DDAMR_DB` | `~/.cache/ddamr/ddamr.db` | ruta de la base de datos |
| `DDAMR_THUMBS_DIR` | `~/.cache/ddamr/thumbs` | caché de miniaturas |

El archivado usa por defecto `~/archivo_cronológico/AÑO/MES/`
y la fecha de los metadatos del archivo; sin fecha, el archivo
no se mueve y se reporta.

## Qué hace hoy (fases 0–3a)

- **Navegación:** árbol de directorios lazy, lista virtualizada
  por scroll (ventanas de 60 filas, altura total real), filtros
  (solo multimedia, ocultar carpetas, búsqueda por nombre),
  ordenación por nombre/extensión/tamaño/fecha y paginación.
- **Miniaturas:** de 64 px en la lista y preview de 320 px en
  la columna de info, generadas en segundo plano con caché LRU
  validada por fecha de modificación; `ddamr thumbs` las
  pre-genera en disco.
- **Metadatos:** lectura nativa (dimensiones y EXIF sin
  ExifTool) y escritura con ExifTool solo sobre copias, con
  verificación posterior. Formulario en la UI con validación de
  fecha (`YYYY:MM:DD HH:MM:SS`) y GPS.
- **Organización:** etiquetas, ubicaciones (con dedup por
  lat/lon redondeado) y regiones normalizadas 0..1, editables
  desde la UI; las regiones se dibujan sobre el preview.
- **Operaciones seguras:** mover, archivar y papelera con
  `SafeOp` (respaldo, verificación de hash, sin sobrescribir:
  colisiones con sufijo ` (2)`, ` (3)`…).
- **Duplicados:** hash exacto BLAKE3 y dHash de imagen y vídeo
  (frames al 2/25/50/75/98 %), también entre imágenes de
  distinto tamaño de archivo; vista de Duplicados con filtros y
  orden por espacio.
- **Indexador:** cancelable, en segundo plano, con contadores
  (escaneados/indexados/errores).

## Estructura

```text
crates/ddamr-core/      Tipos, SafePath, errores, Config. Sin IO pesado.
crates/ddamr-db/        SQLite: migraciones 001–002, repositorios
                        (tags, ubicaciones, regiones, asociaciones),
                        un solo escritor.
crates/ddamr-fs/        Recorrido en streaming, árbol lazy, listado
                        paginado y SafeOp (mover/archivar/papelera).
crates/ddamr-metadata/  Lectura nativa (imagesize + kamadak-exif) y
                        escritura con ExifTool por argv, con timeout.
crates/ddamr-media/     BLAKE3 en streaming, dHash de imagen y vídeo,
                        miniaturas JPEG.
crates/ddamr-index/     Indexador cancelable y buscador de duplicados.
crates/ddamr-app/       Solo iced: navegador, Duplicados, edición de
                        metadatos, miniaturas y lista virtualizada.
crates/ddamr-cli/       CLI con el mismo core (binario `ddamr`).
```

## Desarrollo

```bash
cargo test --workspace                              # 115 tests, 0 fallos
cargo test -p ddamr-app                             # lógica de la UI
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo check -p ddamr-app
```

La GUI no se puede ejecutar en una máquina sin display (VM, CI):
es esperado. Su lógica se verifica con `cargo test -p ddamr-app`
y `cargo check -p ddamr-app`.

## Pendiente

Reproductor de audio/vídeo, recorte de imagen, fusión y
concatenación de vídeo, extracción de frames a disco,
internacionalización (i18n), vista de Configuración completa,
geocodificación con caché y escaneo de mantenimiento
(reindexar + huérfanos).
