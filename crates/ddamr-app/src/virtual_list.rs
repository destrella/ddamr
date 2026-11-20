//! Lista virtualizada por scroll y caché de miniaturas (fase 3a).
//!
//! El `scrollable` de iced construye todos sus hijos: con 20.000
//! filas, cada cambio de estado reconstruiría 20.000 widgets. La
//! solución aquí es una ventana deslizante: la app solo carga
//! `WINDOW_SIZE` filas (60) con `list_dir` paginado y la vista
//! solo construye esas filas, con dos espaciadores que mantienen
//! la altura total del contenido (`total × ROW_HEIGHT_PX`) para
//! que la barra de scroll refleje toda la carpeta. El offset de
//! scroll llega por `scrollable::on_scroll` (iced 0.14) y
//! `window_start_for_offset` decide qué ventana cargar. No es un
//! widget virtualizado propio con layout custom (eso sería 1–2
//! días de trabajo): es virtualización por ventana + espaciadores,
//! suficiente para que el coste por frame no dependa del tamaño
//! de la carpeta.

use ddamr_core::Page;
use ddamr_media::jpeg_dimensions;
use iced::widget::image::Handle;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// La porción de `items` que corresponde a `page`.
/// Los items ya suelen venir paginados del motor; esta función es
/// la garantía en la vista de que nunca se renderiza más de una
/// página, la use quien la use.
pub fn page_slice<T>(items: &[T], page: Page) -> &[T] {
    let start = page.offset.min(items.len());
    let end = start.saturating_add(page.limit).min(items.len());
    &items[start..end]
}

/// Altura de una fila de la lista virtualizada: miniatura de 64 px
/// más padding vertical.
pub const ROW_HEIGHT_PX: f32 = 72.0;

/// Filas por ventana cargada del scroll virtualizado.
pub const WINDOW_SIZE: usize = 60;

/// Filas de margen antes y después de las visibles: se renderizan
/// para que el scroll rápido no deje huecos en blanco.
pub const OVERSCAN_ROWS: usize = 5;

/// Filas a renderizar para un `scroll_offset_px` dado.
///
/// La ventana cubre todas las filas que intersectan el viewport
/// `[offset, offset + viewport_height_px]` más `OVERSCAN_ROWS`
/// filas antes y después, siempre dentro de `[0, total)`.
///
/// - Con `total == 0` devuelve `0..0`.
/// - Un offset negativo se trata como `0`.
/// - Un offset más allá del final devuelve las últimas filas.
pub fn visible_window(
    total: usize,
    scroll_offset_px: f32,
    row_height_px: f32,
    viewport_height_px: f32,
) -> std::ops::Range<usize> {
    if total == 0 || row_height_px <= 0.0 {
        return 0..0;
    }
    let offset = scroll_offset_px.max(0.0);
    let first_visible = (offset / row_height_px).floor() as usize;
    let last_visible_excl = ((offset + viewport_height_px) / row_height_px).ceil() as usize;
    if first_visible >= total {
        // Offset más allá del final: no hay filas visibles; se
        // devuelven las últimas para que la vista no quede vacía.
        return total.saturating_sub(OVERSCAN_ROWS)..total;
    }
    let start = first_visible.saturating_sub(OVERSCAN_ROWS);
    let end = last_visible_excl.saturating_add(OVERSCAN_ROWS).min(total);
    start..end.max(start)
}

/// Página (0-based) a la que pertenece `row` con páginas de
/// `page_size` filas. Con `page_size == 0` devuelve 0, sin pánico.
pub fn page_for_row(row: usize, page_size: usize) -> usize {
    row.checked_div(page_size).unwrap_or(0)
}

/// Primera fila de la ventana de `WINDOW_SIZE` filas que contiene
/// al `scroll_offset_px`, alineada a múltiplos de `WINDOW_SIZE`.
///
/// Cuando `total > WINDOW_SIZE` el inicio se limita a
/// `total - WINDOW_SIZE` para que la ventana no se pase del final;
/// si `total <= WINDOW_SIZE` devuelve 0.
pub fn window_start_for_offset(total: usize, scroll_offset_px: f32, row_height_px: f32) -> usize {
    if total <= WINDOW_SIZE || row_height_px <= 0.0 {
        return 0;
    }
    let first_row = (scroll_offset_px.max(0.0) / row_height_px).floor() as usize;
    let aligned = (first_row / WINDOW_SIZE) * WINDOW_SIZE;
    aligned.min(total - WINDOW_SIZE)
}

/// Una miniatura en caché: el `Handle` de iced creado UNA sola
/// vez a partir de los bytes JPEG, más sus dimensiones reales
/// (leídas de la cabecera JPEG, sin decodificar).
///
/// Por qué se guarda el `Handle` y no los bytes: `Handle::from_bytes`
/// crea `Id::unique()` en cada llamada (verificado en
/// `iced_core-0.14.0/src/image.rs`), así que construirlo dentro de
/// `view()` hace que cada reconstrucción de la vista — un clic,
/// un scroll, cualquier mensaje — cree una imagen "nueva" para la
/// caché de imágenes de iced, que la decodifica de forma asíncrona
/// y tarda ~2 s en reaparecer. Con el `Handle` guardado aquí y
/// clonado en `view()`, la imagen ya está decodificada y no
/// parpadea. Los constructores de `Id` son privados, así que no
/// hay forma de fabricar un id estable por hash: la caché del
/// `Handle` es la única vía.
#[derive(Debug, Clone)]
pub struct CachedThumb {
    pub handle: Handle,
    /// Dimensiones reales de la miniatura (0, 0) si la cabecera
    /// JPEG no se pudo leer (bytes no válidos).
    pub width: u32,
    pub height: u32,
}

/// Caché LRU en memoria para miniaturas.
/// Capacidad acotada: cuando se llena, se expulsa la miniatura
/// que lleva más tiempo sin usarse.
///
/// La clave de caché es (ruta canónica, mtime): cada entrada guarda
/// junto al `Handle` el `mtime` del archivo en el momento en que se
/// generó la miniatura. Si el archivo cambia, su mtime ya no
/// coincide y la entrada se considera inválida (`get_validated`
/// devuelve `None`), así nunca se sirve una miniatura obsoleta.
pub struct ThumbCache {
    capacity: usize,
    map: HashMap<PathBuf, (CachedThumb, Option<i64>)>,
    order: VecDeque<PathBuf>,
}

impl ThumbCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Devuelve la miniatura y la marca como usada recientemente.
    pub fn get(&mut self, path: &Path) -> Option<CachedThumb> {
        if self.map.contains_key(path) {
            self.order.retain(|p| p != path);
            self.order.push_back(path.to_path_buf());
            self.map.get(path).map(|(thumb, _)| thumb.clone())
        } else {
            None
        }
    }

    /// Devuelve la miniatura solo si el `mtime` guardado coincide
    /// con `modified_at`, y la marca como usada recientemente.
    ///
    /// Si el pedido es `None`, solo es válida una entrada guardada
    /// también con `None`; si lo guardado es `None` y el pedido es
    /// `Some`, la entrada no es válida (el archivo cambió desde que
    /// se generó la miniatura o se desconoce su mtime actual).
    pub fn get_validated(&mut self, path: &Path, modified_at: Option<i64>) -> Option<CachedThumb> {
        let valid = self
            .map
            .get(path)
            .is_some_and(|(_, stored)| *stored == modified_at);
        if valid { self.get(path) } else { None }
    }

    pub fn insert(&mut self, path: PathBuf, bytes: Vec<u8>) {
        self.insert_with_mtime(path, bytes, None);
    }

    /// Como `insert`, guardando además el `mtime` del archivo para
    /// poder invalidar la entrada con `get_validated`.
    /// Lectura sin tocar el orden LRU, para usarla desde `view`
    /// (que solo tiene `&self`). Misma regla de validez por mtime
    /// que `get_validated`.
    pub fn peek_validated(&self, path: &Path, modified_at: Option<i64>) -> Option<CachedThumb> {
        let (thumb, stored) = self.map.get(path)?;
        if *stored == modified_at {
            Some(thumb.clone())
        } else {
            None
        }
    }

    pub fn insert_with_mtime(&mut self, path: PathBuf, bytes: Vec<u8>, modified_at: Option<i64>) {
        if self.map.contains_key(&path) {
            self.order.retain(|p| *p != path);
        } else {
            while self.map.len() >= self.capacity {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                } else {
                    break;
                }
            }
        }
        // El `Handle` se crea aquí, una sola vez, y se reutiliza
        // clonado en cada `view()`: ver el comentario de
        // `CachedThumb`. Las dimensiones se leen de la cabecera
        // JPEG (barato) para poder dibujar la miniatura con su
        // proporción real dentro de su cuadrado de 64 px.
        let (width, height) = jpeg_dimensions(&bytes).unwrap_or((0, 0));
        let thumb = CachedThumb {
            handle: Handle::from_bytes(bytes),
            width,
            height,
        };
        self.order.push_back(path.clone());
        self.map.insert(path, (thumb, modified_at));
    }
}

/// Ajusta una imagen de `orig_w` × `orig_h` dentro de un cuadrado
/// de `side` px conservando su proporción, centrada.
///
/// Devuelve `(ancho, alto, offset_x, offset_y)` en px: el tamaño
/// mostrado y su desplazamiento dentro del cuadrado. Con
/// dimensiones desconocidas (0, 0) se devuelve el cuadrado entero.
///
/// No se confía solo en `ContentFit::Contain` (que en iced 0.14
/// también preserva la proporción y centra, y es el ajuste por
/// defecto del widget `Image`): el tamaño explícito es la
/// garantía. `Contain` calcula el ajuste a partir de
/// `measure_image`, que devuelve 0×0 mientras la imagen aún no
/// está decodificada, y el `Image` con ancho y alto fijos
/// distintos de la proporción real se distorsionaría si el modo
/// de ajuste cambiara. Con el tamaño calculado aquí, el widget
/// tiene exactamente la proporción real, pase lo que pase.
pub fn fit_in_square(orig_w: u32, orig_h: u32, side: f32) -> (f32, f32, f32, f32) {
    if orig_w == 0 || orig_h == 0 || side <= 0.0 {
        return (side.max(0.0), side.max(0.0), 0.0, 0.0);
    }
    let scale = side / orig_w.max(orig_h) as f32;
    let w = orig_w as f32 * scale;
    let h = orig_h as f32 * scale;
    ((w), (h), (side - w) / 2.0, (side - h) / 2.0)
}

/// Ventanas adyacentes a precargar para que el scroll no deje
/// huecos: cuando la parte visible del viewport está a menos de
/// `WINDOW_SIZE` filas del borde de la ventana cargada, la ventana
/// vecina debe estar ya cargada al cruzar el borde.
///
/// Devuelve los inicios (alineados a múltiplos de `WINDOW_SIZE`)
/// de las ventanas adyacentes a precargar: como mucho la anterior
/// y la siguiente, nunca la actual, nunca fuera de `[0, total)`.
pub fn prefetch_starts(
    total: usize,
    window_start: usize,
    scroll_offset_px: f32,
    viewport_height_px: f32,
    row_height_px: f32,
) -> Vec<usize> {
    if total <= WINDOW_SIZE || row_height_px <= 0.0 {
        return Vec::new();
    }
    let offset = scroll_offset_px.max(0.0);
    let first_visible = (offset / row_height_px).floor() as usize;
    let last_visible = ((offset + viewport_height_px.max(0.0)) / row_height_px).ceil() as usize;
    let mut out = Vec::new();
    if window_start >= WINDOW_SIZE && first_visible.saturating_sub(WINDOW_SIZE) < window_start {
        out.push(window_start - WINDOW_SIZE);
    }
    let window_end = (window_start + WINDOW_SIZE).min(total);
    if last_visible + WINDOW_SIZE >= window_end {
        let next = window_start + WINDOW_SIZE;
        if next < total {
            out.push(next);
        }
    }
    out
}

/// Escala una región en coordenadas normalizadas `0..1` a píxeles
/// del preview de `preview_width_px` × `preview_height_px`.
///
/// Devuelve `(x_px, y_px, w_px, h_px)`. El rectángulo resultante
/// siempre queda dentro del preview: `x_px`/`y_px` se clampan a
/// `[0, ancho]`/`[0, alto]` y `w_px`/`h_px` a lo que cabe desde esa
/// posición hasta el borde, nunca valores negativos ni que excedan
/// el ancho/alto.
pub fn scale_region_to_preview(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    preview_width_px: f32,
    preview_height_px: f32,
) -> (f32, f32, f32, f32) {
    let width = preview_width_px.max(0.0);
    let height = preview_height_px.max(0.0);
    let x_px = ((x * f64::from(width)) as f32).clamp(0.0, width);
    let y_px = ((y * f64::from(height)) as f32).clamp(0.0, height);
    let w_px = ((w * f64::from(width)) as f32).clamp(0.0, width - x_px);
    let h_px = ((h * f64::from(height)) as f32).clamp(0.0, height - y_px);
    (x_px, y_px, w_px, h_px)
}

/// Como `scale_region_to_preview`, pero para una imagen dibujada
/// con su proporción real y centrada dentro de un área cuadrada:
/// la región (`x, y, w, h` normalizados) se escala al tamaño
/// MOSTRADO (`disp_w` × `disp_h`) y se desplaza por el offset de
/// centrado (`offset_x`, `offset_y`) dentro del área. Sin el
/// offset, los rectángulos quedarían desplazados en imágenes
/// panorámicas o verticales.
pub fn scale_region_to_displayed(
    region: (f64, f64, f64, f64),
    displayed: (f32, f32, f32, f32),
) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = region;
    let (offset_x, offset_y, disp_w, disp_h) = displayed;
    let (rx, ry, rw, rh) = scale_region_to_preview(x, y, w, h, disp_w, disp_h);
    (rx + offset_x, ry + offset_y, rw, rh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_slice_never_exceeds_one_page() {
        let items: Vec<u32> = (0..250).collect();
        assert_eq!(page_slice(&items, Page::first()).len(), 200);
        assert_eq!(page_slice(&items, Page::at(1)).len(), 50);
        assert_eq!(page_slice(&items, Page::at(2)).len(), 0);
    }

    #[test]
    fn thumb_cache_peek_does_not_refresh_lru_order() {
        let mut cache = ThumbCache::new(2);
        cache.insert(PathBuf::from("a"), vec![1]);
        cache.insert(PathBuf::from("b"), vec![2]);
        // `peek_validated` lee sin marcar como usada: "a" sigue
        // siendo la menos reciente y es la expulsada al insertar.
        assert!(cache.peek_validated(Path::new("a"), None).is_some());
        cache.insert(PathBuf::from("c"), vec![3]);
        assert!(cache.get(Path::new("a")).is_none());
        assert!(cache.get(Path::new("b")).is_some());
        assert!(cache.get(Path::new("c")).is_some());
    }

    #[test]
    fn thumb_cache_evicts_least_recently_used() {
        let mut cache = ThumbCache::new(2);
        cache.insert(PathBuf::from("a"), vec![1]);
        cache.insert(PathBuf::from("b"), vec![2]);
        assert!(cache.get(Path::new("a")).is_some());
        cache.insert(PathBuf::from("c"), vec![3]);
        assert_eq!(cache.len(), 2);
        // "a" se usó hace poco, así que la expulsada es "b".
        assert!(cache.get(Path::new("b")).is_none());
        assert!(cache.get(Path::new("a")).is_some());
        assert!(cache.get(Path::new("c")).is_some());
    }

    #[test]
    fn thumb_cache_stores_handle_and_dimensions_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("foto.jpg");
        image::RgbImage::new(400, 300).save(&path).unwrap();
        let safe = ddamr_core::SafePath::new(dir.path(), &path).unwrap();
        let bytes = ddamr_media::thumbnail_sized(&safe, 64).unwrap();
        let mut cache = ThumbCache::new(4);
        cache.insert_with_mtime(path.clone(), bytes, Some(1000));
        let thumb = cache
            .peek_validated(&path, Some(1000))
            .expect("la miniatura debe estar en caché");
        // Dimensiones reales de la cabecera JPEG, no un cuadrado:
        // 400×300 a 64 px de lado mayor es 64×48.
        assert_eq!((thumb.width, thumb.height), (64, 48));
        // El `Handle` se reutiliza: dos lecturas devuelven el
        // mismo id (si `from_bytes` se llamara en cada lectura,
        // cada una tendría un id distinto y la imagen parpadearía).
        let again = cache.peek_validated(&path, Some(1000)).unwrap();
        assert_eq!(thumb.handle, again.handle);
    }

    #[test]
    fn visible_window_at_start_middle_and_end() {
        let total = 5000;
        let viewport = 720.0; // 10 filas visibles con ROW_HEIGHT_PX.
        // Al inicio: empieza en 0 y cubre las filas visibles.
        let start = visible_window(total, 0.0, ROW_HEIGHT_PX, viewport);
        assert_eq!(start.start, 0);
        assert!(start.contains(&0) && start.contains(&9));
        assert!(start.end <= total);
        // A la mitad (~180000 px = fila 2500): cubre las visibles.
        let middle = visible_window(total, 180_000.0, ROW_HEIGHT_PX, viewport);
        assert!(middle.contains(&2500) && middle.contains(&2509));
        assert_eq!(middle.start, 2495);
        assert_eq!(middle.end, 2515);
        assert!(middle.end <= total);
        // Al final: llega hasta el total sin pasarse.
        let end_offset = total as f32 * ROW_HEIGHT_PX - viewport;
        let end = visible_window(total, end_offset, ROW_HEIGHT_PX, viewport);
        assert_eq!(end.end, total);
        assert!(end.contains(&4999));
    }

    #[test]
    fn visible_window_empty_total() {
        assert_eq!(visible_window(0, 0.0, ROW_HEIGHT_PX, 720.0), 0..0);
        assert_eq!(visible_window(0, 1000.0, ROW_HEIGHT_PX, 720.0), 0..0);
    }

    #[test]
    fn visible_window_negative_offset_is_zero() {
        assert_eq!(
            visible_window(5000, -500.0, ROW_HEIGHT_PX, 720.0),
            visible_window(5000, 0.0, ROW_HEIGHT_PX, 720.0)
        );
    }

    #[test]
    fn visible_window_offset_beyond_end_returns_last_rows() {
        let window = visible_window(5000, 1_000_000.0, ROW_HEIGHT_PX, 720.0);
        assert_eq!(window.end, 5000);
        assert!(window.start < window.end);
        assert!(window.start >= 5000 - OVERSCAN_ROWS);
    }

    #[test]
    fn page_for_row_boundaries() {
        assert_eq!(page_for_row(0, 200), 0);
        assert_eq!(page_for_row(199, 200), 0);
        assert_eq!(page_for_row(200, 200), 1);
        assert_eq!(page_for_row(5000, 200), 25);
    }

    #[test]
    fn page_for_row_zero_page_size_never_panics() {
        assert_eq!(page_for_row(0, 0), 0);
        assert_eq!(page_for_row(5000, 0), 0);
    }

    #[test]
    fn window_start_for_offset_alignment_and_clamp() {
        // Fila 2500 → ventana alineada a múltiplos de 60.
        assert_eq!(
            window_start_for_offset(5000, 180_000.0, ROW_HEIGHT_PX),
            2460
        );
        // Al inicio es 0.
        assert_eq!(window_start_for_offset(5000, 0.0, ROW_HEIGHT_PX), 0);
        // Cerca del final se limita a total - WINDOW_SIZE.
        let end_offset = 5000.0 * ROW_HEIGHT_PX - 720.0;
        assert_eq!(
            window_start_for_offset(5000, end_offset, ROW_HEIGHT_PX),
            5000 - WINDOW_SIZE
        );
        // Con total pequeño siempre es 0.
        assert_eq!(window_start_for_offset(30, 1000.0, ROW_HEIGHT_PX), 0);
        assert_eq!(window_start_for_offset(60, 1000.0, ROW_HEIGHT_PX), 0);
    }

    #[test]
    fn thumb_cache_mtime_validation() {
        let mut cache = ThumbCache::new(8);
        cache.insert_with_mtime(PathBuf::from("a"), vec![1], Some(1000));
        // Mismo mtime: válida.
        assert!(cache.get_validated(Path::new("a"), Some(1000)).is_some());
        // Mtime distinto: inválida (el archivo cambió).
        assert!(cache.get_validated(Path::new("a"), Some(2000)).is_none());
        // Pedido None contra guardado Some: inválida.
        assert!(cache.get_validated(Path::new("a"), None).is_none());
        // Guardado None y pedido None: válida.
        cache.insert_with_mtime(PathBuf::from("b"), vec![2], None);
        assert!(cache.get_validated(Path::new("b"), None).is_some());
        // Guardado None y pedido Some: inválida.
        assert!(cache.get_validated(Path::new("b"), Some(1000)).is_none());
    }

    #[test]
    fn thumb_cache_classic_insert_still_works() {
        let mut cache = ThumbCache::new(8);
        cache.insert(PathBuf::from("a"), vec![1]);
        // get clásico sigue funcionando tras insert clásico.
        assert!(cache.get(Path::new("a")).is_some());
        // insert clásico guarda mtime None: solo válido con None.
        assert!(cache.get_validated(Path::new("a"), None).is_some());
        assert!(cache.get_validated(Path::new("a"), Some(1000)).is_none());
    }

    #[test]
    fn fit_in_square_keeps_aspect_and_centers() {
        // Panorámica 2:1 en 64 px: 64×32, centrada verticalmente.
        assert_eq!(fit_in_square(400, 200, 64.0), (64.0, 32.0, 0.0, 16.0));
        // Vertical 1:2 en 64 px: 32×64, centrada horizontalmente.
        assert_eq!(fit_in_square(200, 400, 64.0), (32.0, 64.0, 16.0, 0.0));
        // Cuadrada: ocupa todo el cuadrado.
        assert_eq!(fit_in_square(100, 100, 64.0), (64.0, 64.0, 0.0, 0.0));
        // Panorámica 2:1 en el preview de 284 px: 284×142.
        assert_eq!(fit_in_square(400, 200, 284.0), (284.0, 142.0, 0.0, 71.0));
        // Vertical 1:2 en 284 px: 142×284.
        assert_eq!(fit_in_square(200, 400, 284.0), (142.0, 284.0, 71.0, 0.0));
        // Sin dimensiones: el cuadrado entero (fallback).
        assert_eq!(fit_in_square(0, 0, 64.0), (64.0, 64.0, 0.0, 0.0));
    }

    #[test]
    fn scale_region_to_displayed_adds_centering_offset() {
        // Imagen panorámica 2:1 en un área de 284 px: se dibuja
        // en 284×142 con offset vertical 71. La región (0.25,
        // 0.1, 0.5, 0.2) ocupa (71, 14.2, 142, 28.4) dentro de la
        // imagen, así que en el área queda en (71, 85.2).
        let (x, y, w, h) =
            scale_region_to_displayed((0.25, 0.1, 0.5, 0.2), (0.0, 71.0, 284.0, 142.0));
        assert!((x - 71.0).abs() < 0.001);
        assert!((y - 85.2).abs() < 0.001);
        assert!((w - 142.0).abs() < 0.001);
        assert!((h - 28.4).abs() < 0.001);
        // Sin offset coincide con el escalado directo.
        assert_eq!(
            scale_region_to_displayed((0.25, 0.1, 0.5, 0.2), (0.0, 0.0, 320.0, 240.0)),
            scale_region_to_preview(0.25, 0.1, 0.5, 0.2, 320.0, 240.0)
        );
    }

    #[test]
    fn prefetch_starts_near_window_edges() {
        // Al inicio, con el viewport en la fila 0: la siguiente
        // ventana (60) está a menos de 60 filas del final visible.
        assert_eq!(
            prefetch_starts(5000, 0, 0.0, 720.0, ROW_HEIGHT_PX),
            vec![60]
        );
        // A la mitad de una ventana: anterior y siguiente.
        assert_eq!(
            prefetch_starts(5000, 2460, 180_000.0, 720.0, ROW_HEIGHT_PX),
            vec![2400, 2520]
        );
        // Cerca del final: solo la anterior.
        let end_offset = 4940.0 * ROW_HEIGHT_PX;
        assert_eq!(
            prefetch_starts(5000, 4940, end_offset, 720.0, ROW_HEIGHT_PX),
            vec![4880]
        );
        // Carpeta pequeña: no hay nada que precargar.
        assert!(prefetch_starts(30, 0, 0.0, 720.0, ROW_HEIGHT_PX).is_empty());
        assert!(prefetch_starts(60, 0, 0.0, 720.0, ROW_HEIGHT_PX).is_empty());
    }

    #[test]
    fn scale_region_to_preview_basic() {
        let (x, y, w, h) = scale_region_to_preview(0.25, 0.1, 0.5, 0.2, 320.0, 240.0);
        assert!((x - 80.0).abs() < f32::EPSILON);
        assert!((y - 24.0).abs() < f32::EPSILON);
        assert!((w - 160.0).abs() < f32::EPSILON);
        assert!((h - 48.0).abs() < f32::EPSILON);
    }

    #[test]
    fn scale_region_to_preview_clamps_to_preview() {
        // Región que se sale por la derecha: el ancho se recorta.
        let (x, y, w, h) = scale_region_to_preview(0.9, 0.1, 0.5, 0.2, 320.0, 240.0);
        assert!((x - 288.0).abs() < f32::EPSILON);
        assert!((w - 32.0).abs() < f32::EPSILON);
        assert!(x + w <= 320.0);
        assert!(y + h <= 240.0);
        // Valores negativos: se clampan a 0.
        let (x, y, w, h) = scale_region_to_preview(-0.1, -0.2, 0.5, 0.2, 320.0, 240.0);
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
        assert!((w - 160.0).abs() < f32::EPSILON);
        assert!((h - 48.0).abs() < f32::EPSILON);
    }
}
