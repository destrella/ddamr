//! `App`: estado, mensajes y vista de la fase 1.
//!
//! Regla de oro: `update` es pura lógica de estado. Nunca toca el
//! disco ni la DB directamente: el trabajo pesado va en
//! `Task::perform` con funciones de `ddamr-fs` / `ddamr-metadata`,
//! y el resultado vuelve como un `Message`.
use crate::metadata_edit::{
    EditForm, EditOutcome, apply_metadata_edit, build_patch, open_app_db, parse_lat_lon,
    parse_region_pct, sync_tags,
};
use crate::virtual_list::{
    ROW_HEIGHT_PX, ThumbCache, WINDOW_SIZE, fit_in_square, prefetch_starts,
    scale_region_to_displayed, window_start_for_offset,
};
use ddamr_core::{
    AppError, Config, FileFilter, MediaKind, Page, PageResult, SafePath, SortKey, SortOrder,
    format_size, format_timestamp_ms,
};
use ddamr_db::repos::{
    Location, LocationRepo, LocationSort, Region, RegionRepo, Tag, TagRepo, file_id_for_path,
};
use ddamr_fs::{
    DirNode, DirState, FileEntry, SafeOp, find_node_mut, list_dir, load_children, visible_nodes,
};
use ddamr_index::{
    DuplicateGroup, DuplicateKind, FileInfo, default_db_path, find_duplicates_with_root,
};
use ddamr_media::{jpeg_dimensions, thumbnail_cached};
use ddamr_metadata::{BasicMetadata, read_basic, write_metadata};
use iced::keyboard::{Key, key::Named};
use iced::widget::image::Handle;
use iced::widget::{Image, button, column, container, row, scrollable, stack, text, text_input};
use iced::{Element, Length, Task};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Ancho del preview de la columna de info (la columna mide 300 px).
const PREVIEW_WIDTH_PX: f32 = 284.0;

/// Vista previa grande del archivo seleccionado (JPEG 320 px).
///
/// El `Handle` se crea UNA sola vez al llegar los bytes (ver el
/// comentario de `CachedThumb` en `virtual_list.rs`): si se
/// construyera en `view()`, cada reconstrucción de la vista
/// crearía una imagen "nueva" para iced y el preview parpadearía.
/// `width`/`height` son las dimensiones reales de la cabecera
/// JPEG (0, 0) si no se pudieron leer.
#[derive(Debug, Clone)]
pub struct PreviewImage {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub handle: Handle,
    pub width: u32,
    pub height: u32,
}

/// Icono del árbol según el estado del nodo.
///
/// Se usan emojis 📁 (cerrado) y 📂 (abierto): iced usa
/// cosmic-text con fallback a las fuentes del sistema, y en
/// macOS el emoji se resuelve con Apple Color Emoji. Si en algún
/// sistema los emojis salen como cuadrados, el fallback es
/// `▶︎` / `▼`.
pub fn tree_icon(state: DirState) -> &'static str {
    match state {
        DirState::Closed => "📁",
        DirState::Loaded => "📂",
        DirState::Loading => "⏳",
    }
}

/// Vista principal: el explorador de la fase 1 o la vista de
/// duplicados de la fase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Browser,
    Duplicates,
}

/// Diálogo de confirmación abierto (overlay). La papelera nunca se
/// ejecuta sin confirmar: es el único `ConfirmKind` de la fase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Trash,
}

/// Filtro de grupos de duplicados.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DupFilter {
    #[default]
    All,
    Exact,
    ImageDHash,
    VideoDHash,
}

/// Orden de los grupos de duplicados.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DupSort {
    #[default]
    Space,
    Count,
    Name,
}

/// Modo de visualización del panel central. En fase 1 solo lista;
/// la rejilla de miniaturas llegará con el widget virtualizado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    List,
}

#[derive(Debug, Clone)]
pub enum Message {
    TreeToggle(PathBuf),
    TreeSelect(PathBuf),
    DirLoaded(PathBuf, Vec<DirNode>),
    EntriesLoaded(PageResult<FileEntry>),
    SelectNext,
    SelectPrev,
    ToggleSelect(usize),
    SetSearch(String),
    SetSort(SortKey),
    ToggleHideFolders,
    ToggleOnlyMedia,
    ToggleShowHidden,
    NextPage,
    PrevPage,
    Refresh,
    KeyPressed(Key),
    /// Metadatos del archivo seleccionado, cargados en segundo plano.
    SelectedMetadataLoaded(BasicMetadata),
    // ---- Miniaturas y lista virtualizada (fase 3a) ----
    /// Un lote de miniaturas de 64 px: (ruta, mtime, bytes o None
    /// si falló — en ese caso no se reintenta en esta sesión).
    ThumbnailsLoaded(Vec<(PathBuf, Option<i64>, Option<Vec<u8>>)>),
    /// Offset vertical de scroll del panel de contenido y altura
    /// del viewport, en px (la altura decide el prefetch).
    ScrollChanged(f32, f32),
    /// Una ventana adyacente precargada (su inicio absoluto y sus
    /// entradas): al cruzar el borde, la vista la usa sin hueco.
    PrefetchLoaded(usize, PageResult<FileEntry>),
    /// Vista previa grande (320 px) del seleccionado, o None si
    /// no se pudo generar (sin reintento en esta sesión).
    PreviewLoaded(PathBuf, Option<Vec<u8>>),
    /// Región seleccionada en la lista: su rectángulo se resalta
    /// en el preview. Repetir la deselecciona.
    SelectRegion(i64),
    // ---- Vista de duplicados (fase 2) ----
    ShowBrowser,
    ShowDuplicates,
    DuplicatesLoaded(Vec<DuplicateGroup>),
    ToggleDuplicateGroup(usize),
    ExpandAllDuplicates,
    CollapseAllDuplicates,
    ToggleDuplicateFile(usize, usize),
    /// Selecciona el archivo más antiguo del grupo (por fecha de
    /// creación; a igualdad, el primero por nombre).
    SelectOldest(usize),
    /// Selecciona el archivo más reciente del grupo.
    SelectNewest(usize),
    SetDupFilter(DupFilter),
    SetDupSort(DupSort),
    SetMoveDest(String),
    TrashSelected,
    MoveSelected,
    ArchiveSelected,
    DeselectAll,
    ConfirmTrash,
    CancelDialog,
    /// Resultado de enviar los seleccionados a la papelera.
    TrashDone(usize, Vec<String>),
    /// Resultado de mover los seleccionados (movidos, fallos).
    MoveDone(Vec<String>, Vec<String>),
    /// Resultado de archivar (archivados, sin fecha).
    ArchiveDone(usize, usize),
    // ---- Edición de metadatos, etiquetas, ubicaciones y regiones (fase 2b) ----
    /// Etiquetas, ubicación, ubicaciones existentes y regiones del
    /// archivo seleccionado, cargados en segundo plano. El `Err`
    /// que empieza por "DB: " significa que la DB no se pudo abrir.
    SelectedDetailsLoaded(Result<SelectedDetails, String>),
    SetEditDescription(String),
    SetEditKeywords(String),
    SetEditCopyright(String),
    SetEditDate(String),
    SetEditLat(String),
    SetEditLon(String),
    SaveMetadataEdit,
    DiscardMetadataEdit,
    MetadataEditDone(Result<EditOutcome, String>),
    SetNewTag(String),
    AddTag,
    RemoveTag(i64),
    TagOpDone(Result<(), String>),
    SetNewLocName(String),
    SetNewLocLat(String),
    SetNewLocLon(String),
    SetNewLocCity(String),
    SetNewLocState(String),
    SetNewLocCountry(String),
    CreateLocation,
    AssignLocation(i64),
    RemoveFileLocation,
    LocationOpDone(Result<String, String>),
    SetRegionName(String),
    SetRegionX(String),
    SetRegionY(String),
    SetRegionW(String),
    SetRegionH(String),
    AddRegion,
    RemoveRegion(i64),
    RegionOpDone(Result<String, String>),
}

/// Etiquetas, ubicación y regiones del archivo seleccionado, más
/// la lista de ubicaciones existentes para asignar.
#[derive(Debug, Clone, Default)]
pub struct SelectedDetails {
    pub tags: Vec<Tag>,
    pub location: Option<Location>,
    pub locations: Vec<Location>,
    pub regions: Vec<Region>,
    pub file_id: Option<i64>,
    /// `false` cuando el archivo no está indexado en la DB: las
    /// secciones se muestran vacías y las acciones avisan.
    pub indexed: bool,
}

pub struct App {
    pub config: Config,
    pub root: SafePath,
    pub tree: Vec<DirNode>,
    pub selected_dir: SafePath,
    pub entries: PageResult<FileEntry>,
    pub page: Page,
    pub filter: FileFilter,
    pub sort: SortKey,
    pub sort_order: SortOrder,
    pub selected: HashSet<PathBuf>,
    pub selected_index: usize,
    pub status_msg: String,
    pub view_mode: ViewMode,
    pub selected_metadata: Option<BasicMetadata>,
    /// Caché LRU de miniaturas de 64 px (clave: ruta + mtime).
    pub thumb_cache: ThumbCache,
    /// Rutas cuya miniatura falló en esta sesión: no se reintenta.
    pub thumb_failed: HashSet<PathBuf>,
    /// Offset vertical de scroll del panel de contenido, en px.
    pub scroll_offset_px: f32,
    /// Altura del viewport del panel de contenido, en px (llega
    /// con `ScrollChanged`; 720 por defecto antes del primer
    /// scroll). Decide el prefetch cerca del borde.
    pub viewport_height_px: f32,
    /// Ventanas adyacentes ya precargadas, por inicio absoluto:
    /// al cruzar el borde la vista las usa sin esperar a cargar.
    pub prefetched: HashMap<usize, PageResult<FileEntry>>,
    /// Inicios de ventanas con prefetch en vuelo (para no lanzar
    /// el mismo dos veces).
    pub prefetch_pending: HashSet<usize>,
    /// Ventana pedida y aún no cargada (su inicio absoluto).
    pub pending_window_start: Option<usize>,
    /// Índice dentro de la ventana pendiente (navegación con
    /// flechas que cruza de ventana).
    pub pending_selected_index: Option<usize>,
    /// Vista previa grande del archivo seleccionado.
    pub preview: Option<PreviewImage>,
    /// Rutas cuya vista previa falló en esta sesión.
    pub preview_failed: HashSet<PathBuf>,
    /// Región resaltada en el preview (id de `regions`).
    pub selected_region: Option<i64>,
    // ---- Vista de duplicados (fase 2) ----
    pub view: View,
    pub confirm_dialog: Option<ConfirmKind>,
    pub duplicates: Vec<DuplicateGroup>,
    pub duplicates_loading: bool,
    pub collapsed_groups: HashSet<usize>,
    /// (grupo, archivo) seleccionados en la vista de duplicados.
    pub dup_selected: HashSet<(usize, usize)>,
    pub dup_filter: DupFilter,
    pub dup_sort: DupSort,
    /// Destino de "Mover seleccionados", como texto validado con SafePath.
    pub move_dest: String,
    // ---- Fase 2b: edición de metadatos y secciones de la DB ----
    /// Ruta de la DB (`$DDAMR_DB` o `~/.cache/ddamr/ddamr.db`).
    pub db_path: PathBuf,
    /// `false` cuando la DB no se pudo abrir: las secciones de
    /// etiquetas/ubicaciones/regiones muestran "sin base de datos".
    pub db_available: bool,
    pub selected_details: Option<SelectedDetails>,
    pub db_details_error: Option<String>,
    /// Formulario de edición del archivo seleccionado.
    pub edit_form: EditForm,
    /// Ruta del archivo para el que se cargó `edit_form`; al
    /// cambiar de selección se descarta y se recarga.
    pub edit_form_path: Option<PathBuf>,
    pub new_tag: String,
    pub new_loc_name: String,
    pub new_loc_lat: String,
    pub new_loc_lon: String,
    pub new_loc_city: String,
    pub new_loc_state: String,
    pub new_loc_country: String,
    pub region_name: String,
    pub region_x: String,
    pub region_y: String,
    pub region_w: String,
    pub region_h: String,
}

impl Default for App {
    fn default() -> Self {
        let config = Config::default();
        let root = SafePath::from_root(&config.root_dir)
            .or_else(|_| SafePath::from_root(std::path::Path::new("/tmp")))
            .expect("ni el home ni /tmp son legibles como raíz");
        let tree = vec![DirNode::new(root.clone())];
        let filter = FileFilter {
            hide_folders: config.hide_folders,
            only_media: config.only_media,
            show_hidden: config.show_hidden,
            recursive: config.recursive,
            ..FileFilter::default()
        };
        Self {
            config,
            root: root.clone(),
            tree,
            selected_dir: root,
            entries: PageResult::empty(Page::first()),
            page: Page::first(),
            filter,
            sort: SortKey::Name,
            sort_order: SortOrder::Asc,
            selected: HashSet::new(),
            selected_index: 0,
            status_msg: "Listo. Selecciona una carpeta en el árbol.".into(),
            view_mode: ViewMode::List,
            selected_metadata: None,
            // 512 entradas: con ventanas de 60 filas, al hacer
            // scroll no se expulsan las miniaturas de las ventanas
            // cercanas (con 128 se expulsaban al ir y volver).
            thumb_cache: ThumbCache::new(512),
            thumb_failed: HashSet::new(),
            scroll_offset_px: 0.0,
            viewport_height_px: 720.0,
            prefetched: HashMap::new(),
            prefetch_pending: HashSet::new(),
            pending_window_start: None,
            pending_selected_index: None,
            preview: None,
            preview_failed: HashSet::new(),
            selected_region: None,
            view: View::Browser,
            confirm_dialog: None,
            duplicates: Vec::new(),
            duplicates_loading: false,
            collapsed_groups: HashSet::new(),
            dup_selected: HashSet::new(),
            dup_filter: DupFilter::All,
            dup_sort: DupSort::Space,
            move_dest: String::new(),
            db_path: default_db_path(),
            db_available: true,
            selected_details: None,
            db_details_error: None,
            edit_form: EditForm::default(),
            edit_form_path: None,
            new_tag: String::new(),
            new_loc_name: String::new(),
            new_loc_lat: String::new(),
            new_loc_lon: String::new(),
            new_loc_city: String::new(),
            new_loc_state: String::new(),
            new_loc_country: String::new(),
            region_name: String::new(),
            region_x: String::new(),
            region_y: String::new(),
            region_w: String::new(),
            region_h: String::new(),
        }
    }
}

impl App {
    /// Arranque de la app (función `boot` de iced 0.14): la raíz
    /// aparece EXPANDIDA y el panel de contenido ya muestra sus
    /// elementos, sin esperar a que el usuario pulse nada.
    ///
    /// La raíz se marca `Loading` y se lanza la carga de sus
    /// hijos; en paralelo se carga la primera ventana de entradas
    /// del directorio raíz. `DirLoaded` pondrá la raíz en `Loaded`
    /// y `EntriesLoaded` llenará el panel.
    pub fn boot() -> (Self, Task<Message>) {
        let mut app = Self::default();
        let root_path = app.root.path().to_path_buf();
        if let Some(node) = app.tree.first_mut() {
            node.state = DirState::Loading;
        }
        let root = app.root.clone();
        app.status_msg = "Cargando…".into();
        let children_task =
            Task::perform(
                async move { load_children(&root) },
                move |result| match result {
                    Ok(children) => Message::DirLoaded(root_path.clone(), children),
                    Err(_) => Message::DirLoaded(root_path.clone(), Vec::new()),
                },
            );
        let entries_task = app.load_entries_task();
        (app, Task::batch([children_task, entries_task]))
    }

    /// Páginas totales según el total cargado y la página actual.
    /// No se usa `entries.total_pages()`: `entries.page` es la
    /// página con la que se cargaron esos datos y queda obsoleta
    /// en cuanto se avanza de página, antes de que llegue la carga.
    fn total_pages(&self) -> usize {
        if self.entries.total == 0 {
            1
        } else {
            self.entries.total.div_ceil(self.page.limit.max(1))
        }
    }

    /// Fila absoluta (en todo el directorio) de la primera fila
    /// de la ventana cargada en `entries`.
    fn window_start(&self) -> usize {
        self.entries.page.offset
    }

    /// Carga la ventana de `WINDOW_SIZE` filas que empieza en
    /// `start`, en un Task. `self.page` sigue siendo la "página"
    /// de 200 para la paginación y el contador de páginas.
    fn load_window(&mut self, start: usize) -> Task<Message> {
        self.page = Page::new(start, Page::DEFAULT_LIMIT);
        self.load_entries_task()
    }

    /// Cambia la ventana visible a una ya precargada, al instante
    /// y sin hueco. No toca `selected_index`: quien llama lo
    /// recalcula desde la fila absoluta. Devuelve `false` si esa
    /// ventana no estaba precargada.
    fn swap_to_window(&mut self, start: usize) -> bool {
        let Some(page_result) = self.prefetched.remove(&start) else {
            return false;
        };
        self.entries = page_result;
        self.page = Page::new(start, Page::DEFAULT_LIMIT);
        self.pending_window_start = None;
        true
    }

    /// Carga la ventana actual del directorio seleccionado, en un
    /// Task. La ventana es de `WINDOW_SIZE` filas, no la página
    /// entera de 200: con 5.000 archivos nunca se materializan
    /// más de 60 entradas a la vez.
    fn load_entries_task(&self) -> Task<Message> {
        let dir = self.selected_dir.clone();
        let filter = self.filter.clone();
        let sort = self.sort;
        let order = self.sort_order;
        let page = Page::new(self.page.offset, WINDOW_SIZE);
        Task::perform(
            async move { list_dir(&dir, &filter, sort, order, page) },
            move |result| match result {
                Ok(page_result) => Message::EntriesLoaded(page_result),
                Err(_) => Message::EntriesLoaded(PageResult::empty(page)),
            },
        )
    }

    /// Carga una ventana adyacente en segundo plano (prefetch):
    /// al cruzar el borde, la vista ya tiene sus filas y no deja
    /// un hueco vacío hasta que cargue el lote. No toca `entries`
    /// ni `self.page`: el resultado llega como `PrefetchLoaded`.
    fn prefetch_task(&self, start: usize) -> Task<Message> {
        let dir = self.selected_dir.clone();
        let filter = self.filter.clone();
        let sort = self.sort;
        let order = self.sort_order;
        let page = Page::new(start, WINDOW_SIZE);
        Task::perform(
            async move { list_dir(&dir, &filter, sort, order, page) },
            move |result| match result {
                Ok(page_result) => Message::PrefetchLoaded(start, page_result),
                Err(_) => Message::PrefetchLoaded(start, PageResult::empty(page)),
            },
        )
    }

    /// Lanza el prefetch de las ventanas adyacentes a la cargada
    /// cuando el viewport está cerca de su borde (ver
    /// `prefetch_starts`). Solo las que no están ya cargadas ni
    /// en vuelo. Devuelve `Task::none()` si no toca precargar.
    fn maybe_prefetch(&mut self) -> Task<Message> {
        let starts = prefetch_starts(
            self.entries.total,
            self.window_start(),
            self.scroll_offset_px,
            self.viewport_height_px,
            ROW_HEIGHT_PX,
        );
        let mut tasks = Vec::new();
        for start in starts {
            if self.prefetched.contains_key(&start) || self.prefetch_pending.contains(&start) {
                continue;
            }
            self.prefetch_pending.insert(start);
            tasks.push(self.prefetch_task(start));
        }
        Task::batch(tasks)
    }

    /// Descarta las ventanas precargadas: el directorio, el
    /// filtro o el orden cambiaron y ya no valen.
    fn clear_prefetch(&mut self) {
        self.prefetched.clear();
        self.prefetch_pending.clear();
    }

    /// Metadatos del archivo seleccionado, en un Task, junto con
    /// su vista previa grande (que se genera en paralelo).
    fn metadata_task(&self) -> Task<Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return Task::none();
        };
        let path = entry.path.clone();
        let meta = Task::perform(
            async move { read_basic(&path) },
            Message::SelectedMetadataLoaded,
        );
        Task::batch([meta, self.preview_task()])
    }

    /// Filas de `items` que aún no tienen miniatura (imagen o
    /// vídeo, sin fallos previos en esta sesión), como mucho 12:
    /// la generación va en lotes para no saturar el arranque de
    /// la ventana.
    fn thumbs_needed_for(&self, items: &[FileEntry]) -> Vec<(SafePath, MediaKind, Option<i64>)> {
        items
            .iter()
            .filter(|e| matches!(e.kind, MediaKind::Image | MediaKind::Video))
            .filter(|e| !self.thumb_failed.contains(e.path.path()))
            .filter(|e| {
                self.thumb_cache
                    .peek_validated(e.path.path(), e.modified_at)
                    .is_none()
            })
            .take(12)
            .map(|e| (e.path.clone(), e.kind, e.modified_at))
            .collect()
    }

    /// Filas de la ventana visible que aún no tienen miniatura.
    fn thumbs_needed(&self) -> Vec<(SafePath, MediaKind, Option<i64>)> {
        self.thumbs_needed_for(&self.entries.items)
    }

    /// Genera un lote de miniaturas de 64 px, en un Task.
    ///
    /// Dos aceleradores frente a la generación secuencial de
    /// antes (12 miniaturas en serie, ~3-4 s en la primera carga):
    /// (1) `thumbnail_cached` consulta primero la caché en disco
    /// (`~/.cache/ddamr/thumbs`, la misma de `ddamr thumbs`) y
    /// solo genera si falta; (2) el lote se reparte entre hilos
    /// de trabajo (`std::thread::scope`, hasta 8) dentro del
    /// bloque async, cada uno con su parte del lote.
    fn thumbs_task_for_items(needed: Vec<(SafePath, MediaKind, Option<i64>)>) -> Task<Message> {
        if needed.is_empty() {
            return Task::none();
        }
        Task::perform(
            async move {
                let n_threads = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
                    .clamp(1, 8);
                let chunk = needed.len().div_ceil(n_threads).max(1);
                let mut out = Vec::with_capacity(needed.len());
                std::thread::scope(|scope| {
                    let mut handles = Vec::new();
                    for part in needed.chunks(chunk) {
                        handles.push(scope.spawn(move || {
                            part.iter()
                                .map(|(path, kind, mtime)| {
                                    let bytes = thumbnail_cached(path, *kind, 64).ok();
                                    (path.path().to_path_buf(), *mtime, bytes)
                                })
                                .collect::<Vec<_>>()
                        }));
                    }
                    for h in handles {
                        out.extend(h.join().unwrap_or_default());
                    }
                });
                Message::ThumbnailsLoaded(out)
            },
            |m| m,
        )
    }

    /// Genera el siguiente lote de miniaturas de la ventana
    /// visible, en un Task.
    fn thumbs_task(&self) -> Task<Message> {
        Self::thumbs_task_for_items(self.thumbs_needed())
    }

    /// Vista previa grande (320 px) del archivo seleccionado, en
    /// un Task. No se genera si ya está cargada para esa ruta o
    /// si ya falló en esta sesión; tampoco para no-imágenes. Usa
    /// la misma caché en disco que las miniaturas.
    fn preview_task(&self) -> Task<Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return Task::none();
        };
        if !matches!(entry.kind, MediaKind::Image | MediaKind::Video) {
            return Task::none();
        }
        let path = entry.path.path().to_path_buf();
        if self.preview.as_ref().is_some_and(|p| p.path == path)
            || self.preview_failed.contains(&path)
        {
            return Task::none();
        }
        let safe = entry.path.clone();
        let kind = entry.kind;
        Task::perform(
            async move {
                let bytes = thumbnail_cached(&safe, kind, 320).ok();
                Message::PreviewLoaded(path, bytes)
            },
            |m| m,
        )
    }

    /// Al cambiar de selección, los cambios sin guardar se
    /// descartan silenciosamente y el formulario, las secciones
    /// de la DB y los campos de alta se vacían: se recargan con
    /// la nueva selección (metadatos → detalles).
    ///
    /// La vista previa NO se descarta cuando el archivo
    /// seleccionado es el mismo que el del preview cargado (un
    /// clic en la fila ya seleccionada, o una recarga de la
    /// ventana con la misma selección): regenerarla cuesta
    /// ~1-2 s y era la causa de que el preview apareciera y
    /// desapareciera varias veces al hacer scroll o clic. Solo
    /// se descarta al cambiar de archivo, y la región resaltada
    /// sigue la misma regla.
    fn selection_changed(&mut self) {
        let current = self
            .entries
            .items
            .get(self.selected_index)
            .map(|e| e.path.path().to_path_buf());
        let same_file = self
            .preview
            .as_ref()
            .is_some_and(|p| Some(&p.path) == current.as_ref());
        self.edit_form = EditForm::default();
        self.edit_form_path = None;
        self.selected_details = None;
        if !same_file {
            self.preview = None;
            self.selected_region = None;
        }
        self.db_details_error = None;
        self.new_tag.clear();
        self.new_loc_name.clear();
        self.new_loc_lat.clear();
        self.new_loc_lon.clear();
        self.new_loc_city.clear();
        self.new_loc_state.clear();
        self.new_loc_country.clear();
        self.region_name.clear();
        self.region_x.clear();
        self.region_y.clear();
        self.region_w.clear();
        self.region_h.clear();
    }

    /// Rellena el formulario desde los metadatos y las etiquetas
    /// ya cargadas. Solo se llama cuando el formulario no tiene
    /// cambios sin guardar del usuario.
    fn load_form_from_current(&mut self) {
        let Some(meta) = self.selected_metadata.clone() else {
            return;
        };
        let tags: Vec<String> = self
            .selected_details
            .as_ref()
            .map(|d| d.tags.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();
        self.edit_form = EditForm::from_metadata(&meta, &tags);
        self.edit_form_path = self
            .entries
            .items
            .get(self.selected_index)
            .map(|e| e.path.path().to_path_buf());
    }

    /// Carga etiquetas, ubicación, ubicaciones y regiones del
    /// archivo seleccionado desde la DB, en un Task. Si el archivo
    /// no está indexado, las secciones quedan vacías pero la lista
    /// de ubicaciones se carga igual; solo un fallo al abrir la DB
    /// devuelve un error "DB: …" y marca la DB como no disponible.
    fn load_details_task(&self) -> Task<Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return Task::none();
        };
        let rel = entry.path.relative().to_string_lossy().into_owned();
        let db_path = self.db_path.clone();
        Task::perform(
            async move {
                let result = (|| -> Result<SelectedDetails, String> {
                    let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                    let locations = LocationRepo::list_sorted(&conn, LocationSort::Alpha)
                        .map_err(|e| e.to_string())?;
                    let file_id = file_id_for_path(&conn, &rel).ok();
                    let (tags, location, regions, indexed) = match file_id {
                        Some(fid) => (
                            TagRepo::list_for_file(&conn, &rel).map_err(|e| e.to_string())?,
                            LocationRepo::file_location(&conn, &rel).map_err(|e| e.to_string())?,
                            RegionRepo::list_for_file(&conn, fid).map_err(|e| e.to_string())?,
                            true,
                        ),
                        None => (Vec::new(), None, Vec::new(), false),
                    };
                    Ok(SelectedDetails {
                        tags,
                        location,
                        locations,
                        regions,
                        file_id,
                        indexed,
                    })
                })();
                Message::SelectedDetailsLoaded(result)
            },
            |m| m,
        )
    }

    /// Mueve la selección por filas absolutas del directorio. Si
    /// la fila destino está en la ventana cargada, solo cambia el
    /// índice; si cae en otra ventana, carga esa ventana y deja
    /// la selección pendiente hasta que llegue (`EntriesLoaded`
    /// la aplica). En los extremos no se mueve.
    fn move_selection(&mut self, delta: isize) -> Task<Message> {
        if self.entries.items.is_empty() || self.entries.total == 0 {
            return Task::none();
        }
        let abs = self.window_start() + self.selected_index;
        let next_abs = (abs as isize + delta).clamp(0, self.entries.total as isize - 1) as usize;
        if next_abs == abs {
            return Task::none();
        }
        let start = self.window_start();
        if next_abs >= start && next_abs < start + self.entries.items.len() {
            self.selected_index = next_abs - start;
            self.selection_changed();
            return self.metadata_task();
        }
        let new_start = window_start_for_offset(
            self.entries.total,
            next_abs as f32 * ROW_HEIGHT_PX,
            ROW_HEIGHT_PX,
        );
        if self.prefetched.contains_key(&new_start) {
            // La ventana destino ya estaba precargada: cambio
            // inmediato, sin esperar a cargar.
            self.swap_to_window(new_start);
            self.selected_index = next_abs - new_start;
            self.selection_changed();
            return self.metadata_task();
        }
        self.pending_selected_index = Some(next_abs - new_start);
        self.pending_window_start = Some(new_start);
        self.selection_changed();
        self.load_window(new_start)
    }

    fn toggle_current(&mut self) -> Task<Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return Task::none();
        };
        let path = entry.path.path().to_path_buf();
        if !self.selected.insert(path) {
            self.selected.remove(entry.path.path());
        }
        Task::none()
    }

    // ---- Duplicados (fase 2) ----

    /// Carga los duplicados en un Task: abre la DB y busca con la
    /// raíz, calculando los hashes que falten.
    fn load_duplicates_task(&self) -> Task<Message> {
        let root = self.root.clone();
        Task::perform(
            async move {
                let conn = ddamr_db::open(&default_db_path())
                    .map_err(|e| e.to_string())
                    .and_then(|c| find_duplicates_with_root(&c, &root).map_err(|e| e.to_string()));
                conn.unwrap_or_default()
            },
            Message::DuplicatesLoaded,
        )
    }

    /// Archivos seleccionados en la vista de duplicados, con su
    /// (grupo, índice) para poder quitarlos de la selección tras
    /// actuar sobre ellos.
    fn selected_dup_files(&self) -> Vec<(usize, usize, FileInfo)> {
        let mut out = Vec::new();
        for &(g, i) in &self.dup_selected {
            if let Some(f) = self.duplicates.get(g).and_then(|gr| gr.files.get(i)) {
                out.push((g, i, f.clone()));
            }
        }
        out.sort_by_key(|(g, i, _)| (*g, *i));
        out
    }

    /// Índice del archivo más antiguo / más reciente de un grupo,
    /// por fecha de creación (sin fecha se trata como la más
    /// antigua); desempata por nombre.
    fn oldest_in_group(&self, group: usize) -> Option<usize> {
        self.duplicates.get(group).and_then(|g| {
            g.files
                .iter()
                .enumerate()
                .min_by_key(|(_, f)| (f.created_at.unwrap_or(i64::MIN), f.name.clone()))
                .map(|(i, _)| i)
        })
    }

    fn newest_in_group(&self, group: usize) -> Option<usize> {
        self.duplicates.get(group).and_then(|g| {
            g.files
                .iter()
                .enumerate()
                .max_by_key(|(_, f)| (f.created_at.unwrap_or(i64::MIN), f.name.clone()))
                .map(|(i, _)| i)
        })
    }

    /// Grupos visibles según filtro y orden. Devuelve índices en el
    /// orden de presentación (no se reordena el vector guardado).
    fn visible_dup_groups(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = self
            .duplicates
            .iter()
            .enumerate()
            .filter(|(_, g)| match self.dup_filter {
                DupFilter::All => true,
                DupFilter::Exact => g.kind == DuplicateKind::Exact,
                DupFilter::ImageDHash => g.kind == DuplicateKind::ImageDHash,
                DupFilter::VideoDHash => g.kind == DuplicateKind::VideoDHash,
            })
            .map(|(i, _)| i)
            .collect();
        match self.dup_sort {
            DupSort::Space => {
                idx.sort_by_key(|&i| std::cmp::Reverse(self.duplicates[i].total_bytes))
            }
            DupSort::Count => {
                idx.sort_by_key(|&i| std::cmp::Reverse(self.duplicates[i].files.len()));
            }
            DupSort::Name => idx.sort_by_key(|&i| {
                self.duplicates[i]
                    .files
                    .first()
                    .map(|f| f.name.clone())
                    .unwrap_or_default()
            }),
        }
        idx
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TreeToggle(path) => {
                let Some(node) = find_node_mut(&mut self.tree, &path) else {
                    return Task::none();
                };
                match node.state {
                    DirState::Closed => {
                        node.state = DirState::Loading;
                        self.status_msg = format!("Cargando {}…", node.name());
                        let dir = node.path.clone();
                        Task::perform(
                            async move { load_children(&dir) },
                            move |result| match result {
                                Ok(children) => Message::DirLoaded(path.clone(), children),
                                Err(_) => Message::DirLoaded(path.clone(), Vec::new()),
                            },
                        )
                    }
                    DirState::Loaded => {
                        node.state = DirState::Closed;
                        node.children.clear();
                        Task::none()
                    }
                    DirState::Loading => Task::none(),
                }
            }
            Message::TreeSelect(path) => {
                let Ok(dir) = SafePath::new(self.root.root(), &path) else {
                    return Task::none();
                };
                self.selected_dir = dir;
                self.page = Page::first();
                self.selected_index = 0;
                self.selected.clear();
                self.selected_metadata = None;
                self.selection_changed();
                self.clear_prefetch();
                self.entries = PageResult::empty(self.page);
                self.status_msg = "Cargando…".into();
                self.load_entries_task()
            }
            Message::DirLoaded(path, children) => {
                if let Some(node) = find_node_mut(&mut self.tree, &path) {
                    node.children = children;
                    node.state = DirState::Loaded;
                    self.status_msg = "Listo.".into();
                }
                Task::none()
            }
            Message::EntriesLoaded(page_result) => {
                let total = page_result.total;
                // Si esta ventana llegó también por prefetch, la
                // copia precargada ya no vale: la cargada manda.
                self.prefetched.remove(&page_result.page.offset);
                self.entries = page_result;
                // Navegación con flechas que cruzó de ventana: la
                // selección pendiente se aplica ahora, dentro de
                // la ventana recién cargada.
                self.selected_index = self
                    .pending_selected_index
                    .take()
                    .unwrap_or(0)
                    .min(self.entries.items.len().saturating_sub(1));
                self.pending_window_start = None;
                self.selected_metadata = None;
                self.selection_changed();
                self.status_msg = format!(
                    "{} archivos · página {} de {}",
                    total,
                    self.page.index() + 1,
                    self.total_pages()
                );
                let meta = self.metadata_task();
                let thumbs = self.thumbs_task();
                let prefetch = self.maybe_prefetch();
                Task::batch([meta, thumbs, prefetch])
            }
            Message::ScrollChanged(offset, viewport_height) => {
                self.scroll_offset_px = offset;
                self.viewport_height_px = viewport_height;
                if self.entries.total == 0 {
                    return Task::none();
                }
                let start = window_start_for_offset(self.entries.total, offset, ROW_HEIGHT_PX);
                if start == self.window_start() {
                    // Dentro de la ventana: solo prefetch cerca
                    // del borde, sin recargar nada.
                    return self.maybe_prefetch();
                }
                if self.prefetched.contains_key(&start) {
                    // La ventana ya estaba precargada: se cambia
                    // a ella al instante, sin hueco vacío. La
                    // selección absoluta se conserva.
                    let abs = self.window_start() + self.selected_index;
                    self.swap_to_window(start);
                    self.selected_index = abs
                        .saturating_sub(start)
                        .min(self.entries.items.len().saturating_sub(1));
                    self.selection_changed();
                    let meta = self.metadata_task();
                    let thumbs = self.thumbs_task();
                    let prefetch = self.maybe_prefetch();
                    return Task::batch([meta, thumbs, prefetch]);
                }
                if self.pending_window_start != Some(start) {
                    self.pending_window_start = Some(start);
                    self.pending_selected_index = None;
                    let load = self.load_window(start);
                    let prefetch = self.maybe_prefetch();
                    return Task::batch([load, prefetch]);
                }
                self.maybe_prefetch()
            }
            Message::PrefetchLoaded(start, page_result) => {
                self.prefetch_pending.remove(&start);
                if start == self.window_start() || page_result.total != self.entries.total {
                    // Ya es la ventana visible, o el directorio
                    // cambió mientras se precargaba: se descarta.
                    return Task::none();
                }
                // Las miniaturas de la ventana precargada se
                // empiezan a generar ya, no cuando se vuelva
                // visible: al cruzar el borde ya están (o casi).
                let thumbs =
                    Self::thumbs_task_for_items(self.thumbs_needed_for(&page_result.items));
                self.prefetched.insert(start, page_result);
                thumbs
            }
            Message::ThumbnailsLoaded(results) => {
                for (path, mtime, bytes) in results {
                    match bytes {
                        Some(b) => self.thumb_cache.insert_with_mtime(path, b, mtime),
                        None => {
                            self.thumb_failed.insert(path);
                        }
                    }
                }
                // Seguir con el siguiente lote de la ventana, si
                // quedan filas sin miniatura.
                self.thumbs_task()
            }
            Message::PreviewLoaded(path, bytes) => {
                let Some(entry) = self.entries.items.get(self.selected_index) else {
                    return Task::none();
                };
                if *entry.path.path() != path {
                    return Task::none(); // la selección ya cambió
                }
                match bytes {
                    Some(b) => {
                        // El `Handle` y las dimensiones se crean
                        // aquí, una sola vez: `view()` solo clona
                        // el `Handle` (ver `CachedThumb`).
                        let (width, height) = jpeg_dimensions(&b).unwrap_or((0, 0));
                        let handle = Handle::from_bytes(b.clone());
                        self.preview = Some(PreviewImage {
                            path,
                            bytes: b,
                            handle,
                            width,
                            height,
                        });
                    }
                    None => {
                        self.preview_failed.insert(path);
                    }
                }
                Task::none()
            }
            Message::SelectRegion(id) => {
                if self.selected_region == Some(id) {
                    self.selected_region = None;
                } else {
                    self.selected_region = Some(id);
                }
                Task::none()
            }
            Message::SelectNext => self.move_selection(1),
            Message::SelectPrev => self.move_selection(-1),
            Message::ToggleSelect(abs) => {
                // `abs` es la fila ABSOLUTA del directorio (las
                // filas de ventanas precargadas también se pueden
                // pulsar). Si cae en una ventana precargada, se
                // cambia a ella al instante.
                let start = self.window_start();
                if abs >= start && abs < start + self.entries.items.len() {
                    self.selected_index = abs - start;
                    self.selection_changed();
                    let _ = self.toggle_current();
                    self.metadata_task()
                } else {
                    let target = window_start_for_offset(
                        self.entries.total,
                        abs as f32 * ROW_HEIGHT_PX,
                        ROW_HEIGHT_PX,
                    );
                    if self.prefetched.contains_key(&target) {
                        self.swap_to_window(target);
                        self.selected_index =
                            (abs - target).min(self.entries.items.len().saturating_sub(1));
                        self.selection_changed();
                        let _ = self.toggle_current();
                        self.metadata_task()
                    } else {
                        Task::none()
                    }
                }
            }
            Message::SetSearch(query) => {
                self.filter.search = query;
                self.page = Page::first();
                self.clear_prefetch();
                self.load_entries_task()
            }
            Message::SetSort(key) => {
                if self.sort == key {
                    self.sort_order = self.sort_order.flip();
                } else {
                    self.sort = key;
                    self.sort_order = SortOrder::Asc;
                }
                self.page = Page::first();
                self.clear_prefetch();
                self.load_entries_task()
            }
            Message::ToggleHideFolders => {
                self.filter.hide_folders = !self.filter.hide_folders;
                self.page = Page::first();
                self.clear_prefetch();
                self.load_entries_task()
            }
            Message::ToggleOnlyMedia => {
                self.filter.only_media = !self.filter.only_media;
                self.page = Page::first();
                self.clear_prefetch();
                self.load_entries_task()
            }
            Message::ToggleShowHidden => {
                self.filter.show_hidden = !self.filter.show_hidden;
                self.page = Page::first();
                self.clear_prefetch();
                self.load_entries_task()
            }
            Message::NextPage => {
                if self.page.index() + 1 < self.total_pages() {
                    // Salto de una página (200 filas): la ventana
                    // cargada empieza en ese offset.
                    let start = (self.page.index() + 1) * Page::DEFAULT_LIMIT;
                    self.load_window(start)
                } else {
                    Task::none()
                }
            }
            Message::PrevPage => {
                if self.page.index() > 0 {
                    let start = (self.page.index() - 1) * Page::DEFAULT_LIMIT;
                    self.load_window(start)
                } else {
                    Task::none()
                }
            }
            Message::Refresh => self.load_entries_task(),
            Message::KeyPressed(key) => match key {
                Key::Named(Named::ArrowDown) => self.move_selection(1),
                Key::Named(Named::ArrowUp) => self.move_selection(-1),
                Key::Named(Named::Space) => self.toggle_current(),
                _ => Task::none(),
            },
            Message::SelectedMetadataLoaded(meta) => {
                self.selected_metadata = Some(meta);
                // Primera carga del formulario para esta selección:
                // las palabras clave se rellenan cuando lleguen los
                // detalles (las etiquetas viven en la DB).
                if self.edit_form_path.is_none() {
                    self.load_form_from_current();
                }
                self.load_details_task()
            }
            Message::SelectedDetailsLoaded(result) => {
                match result {
                    Ok(details) => {
                        self.db_available = true;
                        self.db_details_error = None;
                        self.selected_details = Some(details);
                        // Si el formulario se cargó antes que las
                        // etiquetas y el usuario aún no ha editado
                        // nada, rellenar las palabras clave ahora.
                        if !self.edit_form.dirty {
                            self.load_form_from_current();
                        }
                    }
                    Err(e) => {
                        if e.starts_with("DB: ") {
                            self.db_available = false;
                        }
                        self.db_details_error = Some(e);
                        self.selected_details = None;
                    }
                }
                Task::none()
            }
            // ---- Duplicados (fase 2) ----
            Message::ShowBrowser => {
                self.view = View::Browser;
                Task::none()
            }
            Message::ShowDuplicates => {
                self.view = View::Duplicates;
                self.duplicates_loading = true;
                self.status_msg = "Buscando duplicados…".into();
                self.load_duplicates_task()
            }
            Message::DuplicatesLoaded(groups) => {
                let total_files: usize = groups.iter().map(|g| g.files.len()).sum();
                self.duplicates = groups;
                self.duplicates_loading = false;
                self.collapsed_groups.clear();
                self.dup_selected.clear();
                self.status_msg = format!(
                    "{} grupos de duplicados · {} archivos",
                    self.duplicates.len(),
                    total_files
                );
                Task::none()
            }
            Message::ToggleDuplicateGroup(i) => {
                if !self.collapsed_groups.remove(&i) {
                    self.collapsed_groups.insert(i);
                }
                Task::none()
            }
            Message::ExpandAllDuplicates => {
                self.collapsed_groups.clear();
                Task::none()
            }
            Message::CollapseAllDuplicates => {
                self.collapsed_groups = (0..self.duplicates.len()).collect();
                Task::none()
            }
            Message::ToggleDuplicateFile(g, i) => {
                if !self.dup_selected.remove(&(g, i)) {
                    self.dup_selected.insert((g, i));
                }
                Task::none()
            }
            Message::SelectOldest(g) => {
                if let Some(i) = self.oldest_in_group(g) {
                    self.dup_selected.insert((g, i));
                }
                Task::none()
            }
            Message::SelectNewest(g) => {
                if let Some(i) = self.newest_in_group(g) {
                    self.dup_selected.insert((g, i));
                }
                Task::none()
            }
            Message::SetDupFilter(f) => {
                self.dup_filter = f;
                Task::none()
            }
            Message::SetDupSort(s) => {
                self.dup_sort = s;
                Task::none()
            }
            Message::SetMoveDest(s) => {
                self.move_dest = s;
                Task::none()
            }
            Message::TrashSelected => {
                if self.dup_selected.is_empty() {
                    self.status_msg = "No hay archivos seleccionados.".into();
                    Task::none()
                } else {
                    // La papelera nunca se ejecuta sin confirmar.
                    self.confirm_dialog = Some(ConfirmKind::Trash);
                    Task::none()
                }
            }
            Message::ConfirmTrash => {
                let Some(ConfirmKind::Trash) = self.confirm_dialog else {
                    return Task::none();
                };
                self.confirm_dialog = None;
                let files = self.selected_dup_files();
                let root = self.root.clone();
                Task::perform(
                    async move {
                        let ops = SafeOp::new(&root);
                        let mut done = 0usize;
                        let mut errors: Vec<String> = Vec::new();
                        for (_, _, f) in files {
                            match root.join(std::path::Path::new(&f.path)) {
                                Ok(p) => match ops.trash_file(&p) {
                                    Ok(()) => done += 1,
                                    Err(e) => errors.push(format!("{}: {e}", f.path)),
                                },
                                Err(e) => errors.push(format!("{}: {e}", f.path)),
                            }
                        }
                        Message::TrashDone(done, errors)
                    },
                    |m| m,
                )
            }
            Message::CancelDialog => {
                self.confirm_dialog = None;
                Task::none()
            }
            Message::TrashDone(done, errors) => {
                self.dup_selected.clear();
                if errors.is_empty() {
                    self.status_msg = format!("{done} archivos enviados a la papelera.");
                } else {
                    self.status_msg = format!(
                        "{done} a la papelera · {} errores: {}",
                        errors.len(),
                        errors.join("; ")
                    );
                }
                self.duplicates_loading = true;
                self.load_duplicates_task()
            }
            Message::MoveSelected => {
                if self.dup_selected.is_empty() {
                    self.status_msg = "No hay archivos seleccionados.".into();
                    return Task::none();
                }
                let dest_text = self.move_dest.trim().to_string();
                if dest_text.is_empty() {
                    self.status_msg = "Escribe la carpeta destino para mover.".into();
                    return Task::none();
                }
                let Ok(dest_dir) = SafePath::new(self.root.root(), &PathBuf::from(&dest_text))
                else {
                    self.status_msg =
                        format!("La ruta destino {dest_text:?} no está dentro de la raíz.");
                    return Task::none();
                };
                if !dest_dir.path().is_dir() {
                    self.status_msg = format!(
                        "La carpeta destino {} no existe.",
                        dest_dir.path().display()
                    );
                    return Task::none();
                }
                let files = self.selected_dup_files();
                let root = self.root.clone();
                Task::perform(
                    async move {
                        let ops = SafeOp::new(&root);
                        let mut moved: Vec<String> = Vec::new();
                        let mut failed: Vec<String> = Vec::new();
                        for (_, _, f) in files {
                            let src = match root.join(std::path::Path::new(&f.path)) {
                                Ok(p) => p,
                                Err(e) => {
                                    failed.push(format!("{}: {e}", f.path));
                                    continue;
                                }
                            };
                            let dst_path = dest_dir.path().join(&f.name);
                            match SafePath::new(root.root(), &dst_path) {
                                Ok(dst) => match ops.move_file(&src, &dst) {
                                    Ok(_) => moved.push(f.path.clone()),
                                    Err(e) => failed.push(format!("{}: {e}", f.path)),
                                },
                                Err(e) => failed.push(format!("{}: {e}", f.path)),
                            }
                        }
                        Message::MoveDone(moved, failed)
                    },
                    |m| m,
                )
            }
            Message::MoveDone(moved, failed) => {
                self.dup_selected.clear();
                if failed.is_empty() {
                    self.status_msg = format!("{} archivos movidos.", moved.len());
                } else {
                    self.status_msg = format!(
                        "{} movidos · {} fallos: {}",
                        moved.len(),
                        failed.len(),
                        failed.join("; ")
                    );
                }
                self.duplicates_loading = true;
                self.load_duplicates_task()
            }
            Message::ArchiveSelected => {
                if self.dup_selected.is_empty() {
                    self.status_msg = "No hay archivos seleccionados.".into();
                    return Task::none();
                }
                let files = self.selected_dup_files();
                let root = self.root.clone();
                let archive_root = match SafePath::new(root.root(), &self.config.archive_dir) {
                    Ok(a) => a,
                    Err(e) => {
                        self.status_msg = format!("El archivado no es válido: {e}");
                        return Task::none();
                    }
                };
                Task::perform(
                    async move {
                        let ops = SafeOp::new(&root);
                        let mut done = 0usize;
                        let mut no_date = 0usize;
                        for (_, _, f) in files {
                            // Fecha de los metadatos ya indexados; sin
                            // fecha no se mueve: se cuenta (NoDate).
                            let date = f.created_at.and_then(ym_from_ms);
                            let Ok(src) = root.join(std::path::Path::new(&f.path)) else {
                                continue;
                            };
                            match ops.archive_file(&src, &archive_root, date) {
                                Ok(_) => done += 1,
                                Err(AppError::NoDate(_)) => no_date += 1,
                                Err(_) => no_date += 1,
                            }
                        }
                        Message::ArchiveDone(done, no_date)
                    },
                    |m| m,
                )
            }
            Message::ArchiveDone(done, no_date) => {
                self.dup_selected.clear();
                self.status_msg = if no_date == 0 {
                    format!("{done} archivos archivados.")
                } else {
                    format!(
                        "{done} archivos archivados · No se pudieron archivar {no_date} archivos (sin fecha)."
                    )
                };
                self.duplicates_loading = true;
                self.load_duplicates_task()
            }
            Message::DeselectAll => {
                self.dup_selected.clear();
                self.selected.clear();
                Task::none()
            }
            // ---- Fase 2b: edición de metadatos, etiquetas, ubicaciones, regiones ----
            Message::SetEditDescription(s) => {
                self.edit_form.description = s;
                self.edit_form.dirty = true;
                Task::none()
            }
            Message::SetEditKeywords(s) => {
                self.edit_form.keywords = s;
                self.edit_form.dirty = true;
                Task::none()
            }
            Message::SetEditCopyright(s) => {
                self.edit_form.copyright = s;
                self.edit_form.dirty = true;
                Task::none()
            }
            Message::SetEditDate(s) => {
                self.edit_form.datetime_original = s;
                self.edit_form.dirty = true;
                Task::none()
            }
            Message::SetEditLat(s) => {
                self.edit_form.lat = s;
                self.edit_form.dirty = true;
                Task::none()
            }
            Message::SetEditLon(s) => {
                self.edit_form.lon = s;
                self.edit_form.dirty = true;
                Task::none()
            }
            Message::SaveMetadataEdit => {
                let Some(entry) = self.entries.items.get(self.selected_index).cloned() else {
                    self.status_msg = "Sin selección.".into();
                    return Task::none();
                };
                let patch = match build_patch(&self.edit_form) {
                    Ok(p) => p,
                    Err(e) => {
                        self.status_msg = format!("No se guardó: {e}");
                        return Task::none();
                    }
                };
                if patch.is_empty() {
                    self.status_msg = "Sin cambios para guardar.".into();
                    return Task::none();
                }
                let keywords = patch.keywords.clone().unwrap_or_default();
                let rel = entry.path.relative().to_string_lossy().into_owned();
                let root = self.root.clone();
                let src = entry.path.clone();
                let db_path = self.db_path.clone();
                self.status_msg = "Guardando metadatos…".into();
                Task::perform(
                    async move {
                        match apply_metadata_edit(&root, &src, &patch, write_metadata) {
                            Err(e) => Message::MetadataEditDone(Err(e.to_string())),
                            Ok(mut out) => {
                                if !keywords.is_empty()
                                    && let Err(e) = sync_tags(&db_path, &rel, &keywords)
                                {
                                    out.warning = format!(" (etiquetas no sincronizadas: {e})");
                                }
                                Message::MetadataEditDone(Ok(out))
                            }
                        }
                    },
                    |m| m,
                )
            }
            Message::DiscardMetadataEdit => {
                self.load_form_from_current();
                self.edit_form.dirty = false;
                self.status_msg = "Cambios descartados.".into();
                Task::none()
            }
            Message::MetadataEditDone(result) => match result {
                Ok(out) => {
                    self.edit_form.dirty = false;
                    if let Some(entry) = self.entries.items.get_mut(self.selected_index) {
                        entry.size_bytes = out.size_bytes;
                        entry.modified_at = out.modified_at;
                    }
                    self.status_msg = format!("Metadatos guardados.{}", out.warning);
                    self.metadata_task()
                }
                Err(e) => {
                    self.status_msg = format!("No se guardaron los metadatos: {e}");
                    Task::none()
                }
            },
            Message::SetNewTag(s) => {
                self.new_tag = s;
                Task::none()
            }
            Message::AddTag => {
                let name = self.new_tag.trim().to_string();
                if name.is_empty() {
                    self.status_msg = "Escribe una etiqueta para añadirla.".into();
                    return Task::none();
                }
                let Some(entry) = self.entries.items.get(self.selected_index) else {
                    self.status_msg = "Sin selección.".into();
                    return Task::none();
                };
                let rel = entry.path.relative().to_string_lossy().into_owned();
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<(), String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            let id = TagRepo::create(&conn, &name).map_err(|e| e.to_string())?;
                            TagRepo::add_to_file(&conn, &rel, id).map_err(|e| e.to_string())?;
                            Ok(())
                        })();
                        Message::TagOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::RemoveTag(tag_id) => {
                let Some(entry) = self.entries.items.get(self.selected_index) else {
                    return Task::none();
                };
                let rel = entry.path.relative().to_string_lossy().into_owned();
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<(), String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            TagRepo::remove_from_file(&conn, &rel, tag_id)
                                .map_err(|e| e.to_string())?;
                            Ok(())
                        })();
                        Message::TagOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::TagOpDone(result) => match result {
                Ok(()) => {
                    self.new_tag.clear();
                    self.status_msg = "Etiqueta actualizada.".into();
                    self.load_details_task()
                }
                Err(e) => {
                    if e.starts_with("DB: ") {
                        self.db_available = false;
                    }
                    self.status_msg = format!("No se pudo actualizar la etiqueta: {e}");
                    Task::none()
                }
            },
            Message::SetNewLocName(s) => {
                self.new_loc_name = s;
                Task::none()
            }
            Message::SetNewLocLat(s) => {
                self.new_loc_lat = s;
                Task::none()
            }
            Message::SetNewLocLon(s) => {
                self.new_loc_lon = s;
                Task::none()
            }
            Message::SetNewLocCity(s) => {
                self.new_loc_city = s;
                Task::none()
            }
            Message::SetNewLocState(s) => {
                self.new_loc_state = s;
                Task::none()
            }
            Message::SetNewLocCountry(s) => {
                self.new_loc_country = s;
                Task::none()
            }
            Message::CreateLocation => {
                let name = self.new_loc_name.trim().to_string();
                if name.is_empty() {
                    self.status_msg = "Escribe un nombre para la ubicación.".into();
                    return Task::none();
                }
                let (lat, lon) = match parse_lat_lon(&self.new_loc_lat, &self.new_loc_lon) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        self.status_msg = "Escribe la latitud y la longitud.".into();
                        return Task::none();
                    }
                    Err(e) => {
                        self.status_msg = format!("No se creó la ubicación: {e}");
                        return Task::none();
                    }
                };
                let opt = |s: &str| {
                    let t = s.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                };
                let city = opt(&self.new_loc_city);
                let state = opt(&self.new_loc_state);
                let country = opt(&self.new_loc_country);
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<String, String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            let nearby = LocationRepo::find_nearby(&conn, lat, lon)
                                .map_err(|e| e.to_string())?;
                            let id = LocationRepo::create(
                                &conn,
                                &name,
                                lat,
                                lon,
                                city.as_deref(),
                                state.as_deref(),
                                country.as_deref(),
                            )
                            .map_err(|e| e.to_string())?;
                            if let Some(existing) = nearby.iter().find(|l| l.id == id) {
                                Ok(format!(
                                    "Ya existía una ubicación en esas coordenadas («{}»): se usó la existente.",
                                    existing.name
                                ))
                            } else {
                                Ok("Ubicación creada.".into())
                            }
                        })();
                        Message::LocationOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::AssignLocation(loc_id) => {
                let Some(entry) = self.entries.items.get(self.selected_index) else {
                    return Task::none();
                };
                let rel = entry.path.relative().to_string_lossy().into_owned();
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<String, String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            LocationRepo::set_file_location(&conn, &rel, loc_id)
                                .map_err(|e| e.to_string())?;
                            Ok("Ubicación asignada.".into())
                        })();
                        Message::LocationOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::RemoveFileLocation => {
                let Some(entry) = self.entries.items.get(self.selected_index) else {
                    return Task::none();
                };
                let rel = entry.path.relative().to_string_lossy().into_owned();
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<String, String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            LocationRepo::remove_file_location(&conn, &rel)
                                .map_err(|e| e.to_string())?;
                            Ok("Ubicación quitada.".into())
                        })();
                        Message::LocationOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::LocationOpDone(result) => match result {
                Ok(msg) => {
                    self.new_loc_name.clear();
                    self.new_loc_lat.clear();
                    self.new_loc_lon.clear();
                    self.new_loc_city.clear();
                    self.new_loc_state.clear();
                    self.new_loc_country.clear();
                    self.status_msg = msg;
                    self.load_details_task()
                }
                Err(e) => {
                    if e.starts_with("DB: ") {
                        self.db_available = false;
                    }
                    self.status_msg = format!("Ubicación: {e}");
                    Task::none()
                }
            },
            Message::SetRegionName(s) => {
                self.region_name = s;
                Task::none()
            }
            Message::SetRegionX(s) => {
                self.region_x = s;
                Task::none()
            }
            Message::SetRegionY(s) => {
                self.region_y = s;
                Task::none()
            }
            Message::SetRegionW(s) => {
                self.region_w = s;
                Task::none()
            }
            Message::SetRegionH(s) => {
                self.region_h = s;
                Task::none()
            }
            Message::AddRegion => {
                let name = self.region_name.trim().to_string();
                if name.is_empty() {
                    self.status_msg = "Escribe un nombre para la región.".into();
                    return Task::none();
                }
                let rect = match parse_region_pct(
                    &self.region_x,
                    &self.region_y,
                    &self.region_w,
                    &self.region_h,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        self.status_msg = format!("No se añadió la región: {e}");
                        return Task::none();
                    }
                };
                let Some(entry) = self.entries.items.get(self.selected_index) else {
                    return Task::none();
                };
                let rel = entry.path.relative().to_string_lossy().into_owned();
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<String, String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            let fid = file_id_for_path(&conn, &rel).map_err(|e| e.to_string())?;
                            RegionRepo::add(&conn, fid, &name, rect.0, rect.1, rect.2, rect.3)
                                .map_err(|e| e.to_string())?;
                            Ok("Región añadida.".into())
                        })();
                        Message::RegionOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::RemoveRegion(id) => {
                let db_path = self.db_path.clone();
                Task::perform(
                    async move {
                        let r = (|| -> Result<String, String> {
                            let conn = open_app_db(&db_path).map_err(|e| format!("DB: {e}"))?;
                            RegionRepo::remove(&conn, id).map_err(|e| e.to_string())?;
                            Ok("Región eliminada.".into())
                        })();
                        Message::RegionOpDone(r)
                    },
                    |m| m,
                )
            }
            Message::RegionOpDone(result) => match result {
                Ok(msg) => {
                    self.region_name.clear();
                    self.region_x.clear();
                    self.region_y.clear();
                    self.region_w.clear();
                    self.region_h.clear();
                    self.status_msg = msg;
                    self.load_details_task()
                }
                Err(e) => {
                    if e.starts_with("DB: ") {
                        self.db_available = false;
                    }
                    self.status_msg = format!("Región: {e}");
                    Task::none()
                }
            },
        }
    }

    pub fn subscription(&self) -> iced::Subscription<Message> {
        // iced 0.14 no tiene `keyboard::on_key_press`: se escucha
        // el evento genérico y se filtran las teclas de interés.
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, .. }) => match key {
                Key::Named(Named::ArrowDown)
                | Key::Named(Named::ArrowUp)
                | Key::Named(Named::Space) => Some(Message::KeyPressed(key)),
                _ => None,
            },
            _ => None,
        })
    }

    fn tree_view(&self) -> Element<'_, Message> {
        let mut col = column![text("Carpetas").size(14)].spacing(2);
        for (depth, node) in visible_nodes(&self.tree) {
            let icon = tree_icon(node.state);
            let path = node.path.path().to_path_buf();
            let toggle = button(text(icon).size(13)).on_press(Message::TreeToggle(path.clone()));
            let name = button(text(node.name()).size(13)).on_press(Message::TreeSelect(path));
            let row_widget = row![toggle, name].spacing(4).padding(iced::Padding {
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
                left: depth as f32 * 16.0,
            });
            col = col.push(row_widget);
        }
        // `.spacing(4)`: en iced 0.14 el scrollbar flota por
        // defecto SOBRE el contenido; con `spacing` queda
        // incrustado — ocupa su propio espacio (ancho + 4 px de
        // separación) y nunca tapa los nombres. Lo mismo en los
        // demás `scrollable` de la app.
        scrollable(col).width(Length::Fill).spacing(4.0).into()
    }

    /// Lista virtualizada: solo se construyen los widgets de la
    /// ventana cargada (`WINDOW_SIZE` filas como máximo) más las
    /// ventanas adyacentes precargadas, si el viewport las
    /// alcanza. Dos espaciadores con la altura de las filas no
    /// cargadas — antes y después — mantienen la altura total del
    /// contenido en `total × 72 px`, así la barra de scroll
    /// refleja toda la carpeta y `on_scroll` (iced 0.14, con
    /// `Viewport::absolute_offset` y `Viewport::bounds`) carga la
    /// ventana que toca al desplazarse. Con 5.000 archivos nunca
    /// se construyen 5.000 filas, solo las ~60 de la ventana (y
    /// hasta 60 más precargadas a cada lado).
    fn list_view(&self) -> Element<'_, Message> {
        let total = self.entries.total;
        let start = self.window_start();
        let mut col = column![].spacing(0);
        if self.entries.items.is_empty() {
            col = col.push(text("Sin archivos en esta carpeta con los filtros actuales.").size(13));
        } else {
            // Segmentos a renderizar, en orden: la ventana
            // precargada anterior (si existe), la ventana actual
            // y la precargada siguiente. Cada segmento aporta sus
            // filas; los espaciadores cubren el resto.
            let mut segments: Vec<(usize, &[FileEntry])> = Vec::new();
            if start >= WINDOW_SIZE
                && let Some(prev) = self.prefetched.get(&(start - WINDOW_SIZE))
            {
                segments.push((start - WINDOW_SIZE, &prev.items));
            }
            segments.push((start, &self.entries.items));
            if let Some(next) = self.prefetched.get(&(start + WINDOW_SIZE)) {
                segments.push((start + WINDOW_SIZE, &next.items));
            }
            let first_start = segments.first().map(|(s, _)| *s).unwrap_or(start);
            let top_h = first_start as f32 * ROW_HEIGHT_PX;
            if top_h > 0.0 {
                col = col.push(Self::spacer(top_h));
            }
            let mut rendered_end = first_start;
            for (seg_start, items) in segments {
                for (i, entry) in items.iter().enumerate() {
                    col = col.push(self.entry_row(seg_start + i, entry));
                }
                rendered_end = seg_start + items.len();
            }
            let bottom_h = total.saturating_sub(rendered_end) as f32 * ROW_HEIGHT_PX;
            if bottom_h > 0.0 {
                col = col.push(Self::spacer(bottom_h));
            }
        }
        scrollable(col)
            .width(Length::Fill)
            .spacing(4.0)
            .on_scroll(|viewport| {
                Message::ScrollChanged(viewport.absolute_offset().y, viewport.bounds().height)
            })
            .into()
    }

    /// Espaciador invisible de altura fija (filas no cargadas).
    fn spacer(height_px: f32) -> Element<'static, Message> {
        container(text(""))
            .height(Length::Fixed(height_px))
            .width(Length::Fill)
            .into()
    }

    /// Una fila de 72 px: miniatura de 64 px (o placeholder con
    /// el tipo) a la izquierda del nombre. `abs_index` es la fila
    /// absoluta del directorio (las filas de ventanas precargadas
    /// también se dibujan y se pueden pulsar).
    ///
    /// La miniatura se lee de la caché sin tocar el orden LRU
    /// (`peek_validated`): `view` solo tiene `&self`. El `Handle`
    /// guardado en la caché se clona tal cual — nunca se crea uno
    /// nuevo aquí (ver `CachedThumb`): así la imagen no se
    /// redecodifica en cada reconstrucción de la vista.
    ///
    /// El cuadrado de 64×64 es solo el CONTENEDOR: la imagen se
    /// dibuja dentro con su proporción real (`fit_in_square`),
    /// centrada horizontal y verticalmente.
    fn entry_row(&self, abs_index: usize, entry: &FileEntry) -> Element<'_, Message> {
        let thumb: Element<'_, Message> = match entry.kind {
            MediaKind::Image | MediaKind::Video => {
                match self
                    .thumb_cache
                    .peek_validated(entry.path.path(), entry.modified_at)
                {
                    Some(thumb) => {
                        let (w, h, _, _) = fit_in_square(thumb.width, thumb.height, 64.0);
                        container(
                            Image::new(thumb.handle.clone())
                                .width(Length::Fixed(w))
                                .height(Length::Fixed(h)),
                        )
                        .width(Length::Fixed(64.0))
                        .height(Length::Fixed(64.0))
                        .center_x(Length::Fixed(64.0))
                        .center_y(Length::Fixed(64.0))
                        .into()
                    }
                    None => {
                        let label = if entry.kind == MediaKind::Video {
                            "[VID]"
                        } else {
                            "[IMG]"
                        };
                        container(text(label).size(11))
                            .width(Length::Fixed(64.0))
                            .height(Length::Fixed(64.0))
                            .center_x(Length::Fixed(64.0))
                            .center_y(Length::Fixed(64.0))
                            .into()
                    }
                }
            }
            _ => container(text("[ ]").size(11))
                .width(Length::Fixed(64.0))
                .height(Length::Fixed(64.0))
                .center_x(Length::Fixed(64.0))
                .center_y(Length::Fixed(64.0))
                .into(),
        };
        let marker = if abs_index == self.window_start() + self.selected_index {
            "▶ "
        } else if self.selected.contains(entry.path.path()) {
            "✓ "
        } else {
            "  "
        };
        let line = format!(
            "{marker}{}  .{}  {}  {}",
            entry.name_without_ext,
            entry.ext,
            format_size(entry.size_bytes),
            format_timestamp_ms(entry.modified_at)
        );
        button(
            row![thumb, text(line).size(13)]
                .spacing(8)
                .height(Length::Fixed(ROW_HEIGHT_PX)),
        )
        .on_press(Message::ToggleSelect(abs_index))
        .width(Length::Fill)
        .height(Length::Fixed(ROW_HEIGHT_PX))
        .into()
    }

    /// Columna derecha. Orden fijo:
    /// (1) vista previa, (2) información del archivo,
    /// (3) formulario de metadatos, (4) botones de acciones
    /// (a futuro; hoy deshabilitados) y después las secciones
    /// de etiquetas, ubicación y regiones.
    fn info_view(&self) -> Element<'_, Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return column![text("Sin selección").size(14)].spacing(6).into();
        };
        let meta = self.selected_metadata.as_ref();
        let dims = match meta.and_then(|m| m.width.zip(m.height)) {
            Some((w, h)) => format!("{w} × {h} px"),
            None => "-".into(),
        };
        let created = format_timestamp_ms(meta.and_then(|m| m.created_at).or(entry.created_at));
        let flag = |b: bool| if b { "sí" } else { "no" };
        // (1) Vista previa grande con overlay de regiones.
        let mut col = column![text("Vista previa").size(14), self.preview_view(),].spacing(6);
        // (2) Información del archivo.
        col = col.push(text("Información").size(14));
        col = col.push(text(format!("Nombre: {}", entry.name)).size(13));
        col = col.push(text(format!("Ruta: {}", entry.path.relative().display())).size(13));
        col = col.push(text(format!("Tamaño: {}", format_size(entry.size_bytes))).size(13));
        col = col.push(text(format!("Tipo: {}", entry.kind.as_str())).size(13));
        col = col.push(text(format!("Dimensiones: {dims}")).size(13));
        col = col.push(text(format!("Fecha: {created}")).size(13));
        col = col.push(
            text(format!(
                "Etiquetas: {}",
                flag(meta.is_some_and(|m| m.has_keywords))
            ))
            .size(13),
        );
        col = col.push(
            text(format!(
                "Ubicación: {}",
                flag(meta.is_some_and(|m| m.has_location))
            ))
            .size(13),
        );
        col = col.push(
            text(format!(
                "Descripción: {}",
                flag(meta.is_some_and(|m| m.has_description))
            ))
            .size(13),
        );
        // ---- Edición de metadatos ----
        col = col.push(text("Editar metadatos").size(14));
        col = col.push(
            text_input("Descripción…", &self.edit_form.description)
                .on_input(Message::SetEditDescription),
        );
        col = col.push(
            text_input(
                "Palabras clave, separadas por comas…",
                &self.edit_form.keywords,
            )
            .on_input(Message::SetEditKeywords),
        );
        col = col.push(
            text_input("Copyright…", &self.edit_form.copyright).on_input(Message::SetEditCopyright),
        );
        col = col.push(
            text_input(
                "Fecha original YYYY:MM:DD HH:MM:SS",
                &self.edit_form.datetime_original,
            )
            .on_input(Message::SetEditDate),
        );
        col = col.push(
            row![
                text_input("Latitud", &self.edit_form.lat)
                    .on_input(Message::SetEditLat)
                    .width(Length::FillPortion(1)),
                text_input("Longitud", &self.edit_form.lon)
                    .on_input(Message::SetEditLon)
                    .width(Length::FillPortion(1)),
            ]
            .spacing(6),
        );
        col = col.push(
            row![
                button(text("Guardar metadatos").size(12)).on_press(Message::SaveMetadataEdit),
                button(text("Descartar").size(12)).on_press(Message::DiscardMetadataEdit),
            ]
            .spacing(6),
        );
        if self.edit_form.dirty {
            col = col.push(text("Hay cambios sin guardar.").size(12));
        }
        col = col.push(text("Vacío = sin cambios. Guardar necesita ExifTool instalado.").size(11));
        // (4) Botones de acciones: a futuro (recortar, mover,
        // borrar). Hoy deshabilitados — botones sin `on_press` —
        // para reservar su sitio en el orden de la columna.
        col = col.push(text("Acciones").size(14));
        col = col.push(
            row![
                button(text("Recortar").size(12)),
                button(text("Mover").size(12)),
                button(text("Borrar").size(12)),
            ]
            .spacing(6),
        );
        col = col.push(text("Las acciones llegan en la fase 3.").size(11));
        // ---- Etiquetas ----
        col = col.push(text("Etiquetas").size(14));
        col = self.push_tags_section(col);
        // ---- Ubicación ----
        col = col.push(text("Ubicación").size(14));
        col = self.push_location_section(col);
        // ---- Regiones ----
        col = col.push(text("Regiones").size(14));
        col = self.push_regions_section(col);
        scrollable(col).width(Length::Fill).spacing(4.0).into()
    }

    /// Vista previa grande del archivo seleccionado, con las
    /// regiones dibujadas encima como rectángulos con su nombre.
    ///
    /// Elección de implementación: un `stack` con la imagen de
    /// fondo y, por cada región, un `container` de tamaño fijo
    /// (el del preview) con `padding` izquierdo/superior igual
    /// a la posición escalada, cuyo hijo es el rectángulo con
    /// borde. Se descartó `canvas`: dibujar la imagen en un
    /// `canvas::Frame` en iced 0.14 exige convertir el JPEG a
    /// `Handle` igualmente y gestionar el `Program` con su
    /// propio estado, sin ventaja frente al `stack` para
    /// rectángulos estáticos; el `stack` compone con el resto
    /// de la vista sin estado extra.
    fn preview_view(&self) -> Element<'_, Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return text("Sin selección").size(12).into();
        };
        if !matches!(entry.kind, MediaKind::Image | MediaKind::Video) {
            return text("Sin vista previa para este tipo de archivo.")
                .size(12)
                .into();
        }
        let Some(preview) = &self.preview else {
            if self.preview_failed.contains(entry.path.path()) {
                return text("No se pudo generar la vista previa.").size(12).into();
            }
            return text("Cargando vista previa…").size(12).into();
        };
        if preview.path != *entry.path.path() {
            return text("Cargando vista previa…").size(12).into();
        }
        // Tamaño mostrado: el área reservada es un cuadrado de
        // `PREVIEW_WIDTH_PX` y la imagen se dibuja dentro con su
        // proporción real, centrada (`fit_in_square`). Las
        // dimensiones se toman de la cabecera JPEG guardada en el
        // preview y, si no se pudieron leer, de los metadatos;
        // nunca de un cuadrado por defecto: un (320, 320)
        // inventado distorsionaba las imágenes no cuadradas.
        // Último recurso (sin cabecera ni metadatos): el área
        // cuadrada con `ContentFit::Contain`, que preserva la
        // proporción una vez decodificada la imagen.
        let (w, h) = if preview.width > 0 && preview.height > 0 {
            (preview.width, preview.height)
        } else {
            self.selected_metadata
                .as_ref()
                .and_then(|m| m.width.zip(m.height))
                .unwrap_or((0, 0))
        };
        let (disp_w, disp_h, off_x, off_y) = fit_in_square(w, h, PREVIEW_WIDTH_PX);
        let img = container(
            Image::new(preview.handle.clone())
                .width(Length::Fixed(disp_w))
                .height(Length::Fixed(disp_h)),
        )
        .width(Length::Fixed(PREVIEW_WIDTH_PX))
        .height(Length::Fixed(PREVIEW_WIDTH_PX))
        .center_x(Length::Fixed(PREVIEW_WIDTH_PX))
        .center_y(Length::Fixed(PREVIEW_WIDTH_PX));
        let mut st = stack![img];
        if let Some(details) = &self.selected_details {
            for region in &details.regions {
                // Las regiones se escalan al tamaño MOSTRADO de
                // la imagen y se desplazan por el offset de
                // centrado: sin él quedarían desplazadas en
                // imágenes panorámicas o verticales.
                let (x, y, rw, rh) = scale_region_to_displayed(
                    (region.x, region.y, region.w, region.h),
                    (off_x, off_y, disp_w, disp_h),
                );
                if rw <= 0.0 || rh <= 0.0 {
                    continue;
                }
                let highlighted = self.selected_region == Some(region.id);
                let border_color = if highlighted {
                    iced::Color::from_rgb(1.0, 0.25, 0.25)
                } else {
                    iced::Color::from_rgb(0.2, 0.6, 1.0)
                };
                let border_width = if highlighted { 3.0 } else { 1.5 };
                let rect = container(text(&region.name).size(10))
                    .width(Length::Fixed(rw.max(1.0)))
                    .height(Length::Fixed(rh.max(1.0)))
                    .style(move |_theme: &iced::Theme| container::Style {
                        text_color: Some(border_color),
                        background: None,
                        border: iced::Border {
                            color: border_color,
                            width: border_width,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    });
                // El `container` exterior tiene el tamaño del
                // área del preview y su `padding` coloca el
                // rectángulo en su posición: el hijo se dibuja
                // dentro del área de contenido, desplazado (x, y).
                let placed = container(rect)
                    .width(Length::Fixed(PREVIEW_WIDTH_PX))
                    .height(Length::Fixed(PREVIEW_WIDTH_PX))
                    .padding(iced::Padding {
                        top: y,
                        right: 0.0,
                        bottom: 0.0,
                        left: x,
                    });
                st = st.push(placed);
            }
        }
        st.into()
    }

    /// Sección de etiquetas dentro de `info_view`.
    fn push_tags_section<'a>(
        &'a self,
        mut col: iced::widget::Column<'a, Message>,
    ) -> iced::widget::Column<'a, Message> {
        if !self.db_available {
            return col.push(text("Sin base de datos.").size(12));
        }
        if let Some(err) = &self.db_details_error {
            return col.push(text(format!("Error de base de datos: {err}")).size(12));
        }
        let Some(details) = &self.selected_details else {
            return col.push(text("Cargando…").size(12));
        };
        if !details.indexed {
            col = col.push(
                text("Archivo no indexado: indexa la carpeta para guardar etiquetas.").size(12),
            );
        }
        if details.tags.is_empty() {
            col = col.push(text("Sin etiquetas.").size(12));
        }
        for tag in &details.tags {
            col = col.push(
                row![
                    text(&tag.name).size(12),
                    button(text("×").size(12)).on_press(Message::RemoveTag(tag.id)),
                ]
                .spacing(6),
            );
        }
        col.push(
            row![
                text_input("Nueva etiqueta…", &self.new_tag)
                    .on_input(Message::SetNewTag)
                    .width(Length::Fill),
                button(text("Añadir").size(12)).on_press(Message::AddTag),
            ]
            .spacing(6),
        )
    }

    /// Sección de ubicación dentro de `info_view`.
    fn push_location_section<'a>(
        &'a self,
        mut col: iced::widget::Column<'a, Message>,
    ) -> iced::widget::Column<'a, Message> {
        if !self.db_available {
            return col.push(text("Sin base de datos.").size(12));
        }
        if let Some(err) = &self.db_details_error {
            return col.push(text(format!("Error de base de datos: {err}")).size(12));
        }
        let Some(details) = &self.selected_details else {
            return col.push(text("Cargando…").size(12));
        };
        match &details.location {
            Some(loc) => {
                let place = [loc.city.clone(), loc.country.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ");
                let label = if place.is_empty() {
                    loc.name.clone()
                } else {
                    format!("{} ({})", loc.name, place)
                };
                col = col.push(
                    row![
                        text(label).size(12),
                        button(text("Quitar").size(12)).on_press(Message::RemoveFileLocation),
                    ]
                    .spacing(6),
                );
            }
            None => {
                col = col.push(text("Sin ubicación asignada.").size(12));
            }
        }
        if !details.indexed {
            col = col.push(
                text("Archivo no indexado: indexa la carpeta para asignar ubicaciones.").size(12),
            );
        } else if details.locations.is_empty() {
            col = col.push(text("No hay ubicaciones guardadas todavía.").size(12));
        } else {
            col = col.push(text("Asignar una existente:").size(12));
            for loc in details.locations.iter().take(15) {
                col = col.push(
                    button(text(format!("{} ({:.4}, {:.4})", loc.name, loc.lat, loc.lon)).size(12))
                        .on_press(Message::AssignLocation(loc.id)),
                );
            }
            if details.locations.len() > 15 {
                col = col.push(text(format!("…y {} más", details.locations.len() - 15)).size(12));
            }
        }
        col = col.push(text("Crear ubicación nueva:").size(12));
        col = col.push(text_input("Nombre…", &self.new_loc_name).on_input(Message::SetNewLocName));
        col = col.push(
            row![
                text_input("Latitud", &self.new_loc_lat)
                    .on_input(Message::SetNewLocLat)
                    .width(Length::FillPortion(1)),
                text_input("Longitud", &self.new_loc_lon)
                    .on_input(Message::SetNewLocLon)
                    .width(Length::FillPortion(1)),
            ]
            .spacing(6),
        );
        col = col.push(
            row![
                text_input("Ciudad", &self.new_loc_city)
                    .on_input(Message::SetNewLocCity)
                    .width(Length::FillPortion(1)),
                text_input("Estado", &self.new_loc_state)
                    .on_input(Message::SetNewLocState)
                    .width(Length::FillPortion(1)),
                text_input("País", &self.new_loc_country)
                    .on_input(Message::SetNewLocCountry)
                    .width(Length::FillPortion(1)),
            ]
            .spacing(6),
        );
        col.push(button(text("Crear ubicación").size(12)).on_press(Message::CreateLocation))
    }

    /// Sección de regiones dentro de `info_view`.
    fn push_regions_section<'a>(
        &'a self,
        mut col: iced::widget::Column<'a, Message>,
    ) -> iced::widget::Column<'a, Message> {
        if !self.db_available {
            return col.push(text("Sin base de datos.").size(12));
        }
        if let Some(err) = &self.db_details_error {
            return col.push(text(format!("Error de base de datos: {err}")).size(12));
        }
        let Some(details) = &self.selected_details else {
            return col.push(text("Cargando…").size(12));
        };
        if !details.indexed {
            col = col.push(
                text("Archivo no indexado: indexa la carpeta para guardar regiones.").size(12),
            );
        }
        if details.regions.is_empty() {
            col = col.push(text("Sin regiones.").size(12));
        }
        for region in &details.regions {
            // Pulsar el nombre resalta su rectángulo en el preview.
            let mark = if self.selected_region == Some(region.id) {
                "▶ "
            } else {
                ""
            };
            col = col.push(
                row![
                    button(
                        text(format!(
                            "{mark}{} — {:.1}%, {:.1}% · {:.1}×{:.1}%",
                            region.name,
                            region.x * 100.0,
                            region.y * 100.0,
                            region.w * 100.0,
                            region.h * 100.0
                        ))
                        .size(12)
                    )
                    .on_press(Message::SelectRegion(region.id)),
                    button(text("×").size(12)).on_press(Message::RemoveRegion(region.id)),
                ]
                .spacing(6),
            );
        }
        col = col.push(text("Añadir región (%, 0..100):").size(12));
        col = col.push(text_input("Nombre…", &self.region_name).on_input(Message::SetRegionName));
        col = col.push(
            row![
                text_input("x", &self.region_x)
                    .on_input(Message::SetRegionX)
                    .width(Length::FillPortion(1)),
                text_input("y", &self.region_y)
                    .on_input(Message::SetRegionY)
                    .width(Length::FillPortion(1)),
                text_input("ancho", &self.region_w)
                    .on_input(Message::SetRegionW)
                    .width(Length::FillPortion(1)),
                text_input("alto", &self.region_h)
                    .on_input(Message::SetRegionH)
                    .width(Length::FillPortion(1)),
            ]
            .spacing(6),
        );
        col.push(button(text("Añadir región").size(12)).on_press(Message::AddRegion))
    }

    /// Vista de duplicados: grupos colapsables con preview,
    /// filtros, orden y barra de selección.
    fn duplicates_view(&self) -> Element<'_, Message> {
        let filter_btn = |f: DupFilter, label: &str| {
            let mark = if self.dup_filter == f { "● " } else { "○ " };
            button(text(format!("{mark}{label}")).size(12)).on_press(Message::SetDupFilter(f))
        };
        let sort_btn = |s: DupSort, label: &str| {
            let mark = if self.dup_sort == s { "● " } else { "○ " };
            button(text(format!("{mark}{label}")).size(12)).on_press(Message::SetDupSort(s))
        };
        let mut col = column![
            row![
                button(text("◀ Explorador").size(12)).on_press(Message::ShowBrowser),
                text("Duplicados").size(14),
                button(text("Buscar de nuevo").size(12)).on_press(Message::ShowDuplicates),
            ]
            .spacing(8),
            row![
                filter_btn(DupFilter::All, "Todos"),
                filter_btn(DupFilter::Exact, "Exactos"),
                filter_btn(DupFilter::ImageDHash, "dHash imagen"),
                filter_btn(DupFilter::VideoDHash, "dHash vídeo"),
            ]
            .spacing(6),
            row![
                text("Orden:").size(12),
                sort_btn(DupSort::Space, "Espacio total"),
                sort_btn(DupSort::Count, "Nº de duplicados"),
                sort_btn(DupSort::Name, "Nombre"),
                button(text("Expandir todos").size(12)).on_press(Message::ExpandAllDuplicates),
                button(text("Colapsar todos").size(12)).on_press(Message::CollapseAllDuplicates),
            ]
            .spacing(6),
        ]
        .spacing(6);
        if self.duplicates_loading {
            col = col.push(text("Buscando duplicados…").size(13));
        } else if self.duplicates.is_empty() {
            col = col.push(text("Sin duplicados en la base de datos.").size(13));
        }
        for g_idx in self.visible_dup_groups() {
            let g = &self.duplicates[g_idx];
            let kind_label = match g.kind {
                DuplicateKind::Exact => "Exactos",
                DuplicateKind::ImageDHash => "Similares (imagen)",
                DuplicateKind::VideoDHash => "Similares (vídeo)",
            };
            let collapsed = self.collapsed_groups.contains(&g_idx);
            let icon = if collapsed { "▶︎" } else { "▼" };
            let header = row![
                button(text(icon).size(13)).on_press(Message::ToggleDuplicateGroup(g_idx)),
                text(format!(
                    "{kind_label} · {} archivos · {}",
                    g.files.len(),
                    format_size(g.total_bytes)
                ))
                .size(13),
                button(text("Más antiguo").size(12)).on_press(Message::SelectOldest(g_idx)),
                button(text("Más nuevo").size(12)).on_press(Message::SelectNewest(g_idx)),
            ]
            .spacing(6);
            col = col.push(header);
            if !collapsed {
                for (f_idx, f) in g.files.iter().enumerate() {
                    let mark = if self.dup_selected.contains(&(g_idx, f_idx)) {
                        "✓ "
                    } else {
                        "○ "
                    };
                    let dims = match (f.width, f.height) {
                        (Some(w), Some(h)) => format!("{w}×{h}"),
                        _ => "-".into(),
                    };
                    let dur = match f.duration_ms {
                        Some(ms) => format!("{} s", ms / 1000),
                        None => "-".into(),
                    };
                    let date = f
                        .created_at
                        .map(|ts| format_timestamp_ms(Some(ts)))
                        .unwrap_or_else(|| "-".into());
                    let line = format!(
                        "{mark}{}  {}  {}  {}  {}",
                        f.name,
                        format_size(f.size_bytes),
                        dims,
                        dur,
                        date
                    );
                    let file_row = row![
                        button(text(line).size(12))
                            .on_press(Message::ToggleDuplicateFile(g_idx, f_idx))
                            .width(Length::Fill),
                        text(f.path.clone()).size(11),
                    ]
                    .spacing(6);
                    col = col.push(file_row);
                }
            }
        }
        // Barra inferior de selección (solo si hay selección).
        if !self.dup_selected.is_empty() {
            col = col.push(self.selection_bar());
        }
        // Diálogo de confirmación de papelera (overlay en columna).
        // (El scrollable de esta vista también lleva `.spacing(4)`
        // para que la barra no tape el contenido; ver `tree_view`.)
        if let Some(ConfirmKind::Trash) = self.confirm_dialog {
            col = col.push(
                container(
                    column![
                        text(format!(
                            "¿Enviar {} archivos seleccionados a la papelera?",
                            self.dup_selected.len()
                        ))
                        .size(13),
                        row![
                            button(text("Sí, a la papelera").size(12))
                                .on_press(Message::ConfirmTrash),
                            button(text("Cancelar").size(12)).on_press(Message::CancelDialog),
                        ]
                        .spacing(8),
                    ]
                    .spacing(6),
                )
                .padding(10),
            );
        }
        scrollable(col).width(Length::Fill).spacing(4.0).into()
    }

    /// Barra inferior: mover / archivar / papelera / deseleccionar.
    fn selection_bar(&self) -> Element<'_, Message> {
        row![
            text(format!("{} seleccionados", self.dup_selected.len())).size(12),
            text_input("Carpeta destino dentro de la raíz…", &self.move_dest)
                .on_input(Message::SetMoveDest)
                .width(Length::Fixed(220.0)),
            button(text("Mover").size(12)).on_press(Message::MoveSelected),
            button(text("Archivar").size(12)).on_press(Message::ArchiveSelected),
            button(text("Papelera…").size(12)).on_press(Message::TrashSelected),
            button(text("Deseleccionar todo").size(12)).on_press(Message::DeselectAll),
        ]
        .spacing(6)
        .into()
    }

    pub fn view(&self) -> Element<'_, Message> {
        if self.view == View::Duplicates {
            let status = text(&self.status_msg).size(12);
            return column![self.duplicates_view(), status]
                .spacing(6)
                .padding(8)
                .into();
        }
        let sort_label = |key: SortKey, label: &str| {
            let arrow = if self.sort == key {
                match self.sort_order {
                    SortOrder::Asc => " ↑",
                    SortOrder::Desc => " ↓",
                }
            } else {
                ""
            };
            format!("{label}{arrow}")
        };
        let controls = row![
            text(self.selected_dir.path().display().to_string()).size(13),
            text_input("Buscar por nombre…", &self.filter.search)
                .on_input(Message::SetSearch)
                .width(Length::Fixed(180.0)),
            button(text(sort_label(SortKey::Name, "Nombre")).size(12))
                .on_press(Message::SetSort(SortKey::Name)),
            button(text(sort_label(SortKey::Extension, "Ext")).size(12))
                .on_press(Message::SetSort(SortKey::Extension)),
            button(text(sort_label(SortKey::Size, "Tamaño")).size(12))
                .on_press(Message::SetSort(SortKey::Size)),
            button(text(sort_label(SortKey::Date, "Fecha")).size(12))
                .on_press(Message::SetSort(SortKey::Date)),
            button(text("Recargar").size(12)).on_press(Message::Refresh),
            button(text("Duplicados").size(12)).on_press(Message::ShowDuplicates),
        ]
        .spacing(6);
        let filters = row![
            button(
                text(format!(
                    "Ocultar carpetas: {}",
                    if self.filter.hide_folders {
                        "sí"
                    } else {
                        "no"
                    }
                ))
                .size(12)
            )
            .on_press(Message::ToggleHideFolders),
            button(
                text(format!(
                    "Solo multimedia: {}",
                    if self.filter.only_media { "sí" } else { "no" }
                ))
                .size(12)
            )
            .on_press(Message::ToggleOnlyMedia),
            button(
                text(format!(
                    "Mostrar ocultos: {}",
                    if self.filter.show_hidden { "sí" } else { "no" }
                ))
                .size(12)
            )
            .on_press(Message::ToggleShowHidden),
        ]
        .spacing(6);
        let pagination = row![
            button(text("◀ Anterior").size(12)).on_press(Message::PrevPage),
            text(format!(
                "Página {} de {} · {} archivos · {} seleccionados",
                self.page.index() + 1,
                self.total_pages(),
                self.entries.total,
                self.selected.len()
            ))
            .size(12),
            button(text("Siguiente ▶").size(12)).on_press(Message::NextPage),
        ]
        .spacing(8);
        let center = column![controls, filters, self.list_view(), pagination,]
            .spacing(6)
            .width(Length::Fill);
        let status = text(&self.status_msg).size(12);
        let center_with_status = column![center, status].spacing(6).width(Length::Fill);
        row![
            container(self.tree_view()).width(Length::Fixed(300.0)),
            center_with_status,
            container(self.info_view()).width(Length::Fixed(300.0)),
        ]
        .spacing(8)
        .padding(8)
        .into()
    }
}

/// (año, mes) UTC de un timestamp en milisegundos, sin
/// dependencias externas (inverso de `days_from_civil`).
fn ym_from_ms(ms: i64) -> Option<(u32, u32)> {
    let days = ms.div_euclid(86_400_000);
    // civil from days (Howard Hinnant).
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

#[cfg(test)]
mod tests {
    use super::*;
    use ddamr_fs::FileEntry;
    use tempfile::TempDir;

    fn app_with_entries(names: &[&str]) -> (App, TempDir) {
        let dir = TempDir::new().unwrap();
        for name in names {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let root = SafePath::from_root(dir.path()).unwrap();
        let entries = list_dir(
            &root,
            &FileFilter {
                only_media: false,
                ..FileFilter::default()
            },
            SortKey::Name,
            SortOrder::Asc,
            Page::first(),
        )
        .unwrap();
        let app = App {
            root: root.clone(),
            selected_dir: root.clone(),
            tree: vec![DirNode::new(root)],
            entries,
            ..App::default()
        };
        (app, dir)
    }

    fn paged_app(total: usize) -> App {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        let found = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let entry = FileEntry::from_dir_entry(dir.path(), &found).unwrap();
        App {
            entries: PageResult::new(vec![entry; 200], total, Page::first()),
            ..App::default()
        }
    }

    #[test]
    fn select_next_prev_clamps_at_bounds() {
        let (mut app, _dir) = app_with_entries(&["a.jpg", "b.jpg", "c.jpg"]);
        assert_eq!(app.selected_index, 0);
        let _ = app.update(Message::SelectNext);
        assert_eq!(app.selected_index, 1);
        let _ = app.update(Message::SelectNext);
        assert_eq!(app.selected_index, 2);
        let _ = app.update(Message::SelectNext);
        assert_eq!(app.selected_index, 2, "no pasa del último");
        let _ = app.update(Message::SelectPrev);
        assert_eq!(app.selected_index, 1);
        let _ = app.update(Message::SelectPrev);
        let _ = app.update(Message::SelectPrev);
        assert_eq!(app.selected_index, 0, "no baja de 0");
    }

    #[test]
    fn key_pressed_moves_selection_and_space_toggles() {
        let (mut app, _dir) = app_with_entries(&["a.jpg", "b.jpg"]);
        let _ = app.update(Message::KeyPressed(Key::Named(Named::ArrowDown)));
        assert_eq!(app.selected_index, 1);
        let _ = app.update(Message::KeyPressed(Key::Named(Named::Space)));
        assert_eq!(app.selected.len(), 1);
        let _ = app.update(Message::KeyPressed(Key::Named(Named::Space)));
        assert_eq!(app.selected.len(), 0);
        let _ = app.update(Message::KeyPressed(Key::Named(Named::ArrowUp)));
        assert_eq!(app.selected_index, 0);
    }

    #[test]
    fn next_prev_page_changes_page() {
        let mut app = paged_app(250);
        assert_eq!(app.page.index(), 0);
        let _ = app.update(Message::NextPage);
        assert_eq!(app.page.index(), 1);
        let _ = app.update(Message::NextPage);
        assert_eq!(app.page.index(), 1, "no hay tercera página");
        let _ = app.update(Message::PrevPage);
        assert_eq!(app.page.index(), 0);
        let _ = app.update(Message::PrevPage);
        assert_eq!(app.page.index(), 0, "no hay página -1");
    }

    #[test]
    fn set_search_updates_filter_and_resets_page() {
        let mut app = paged_app(250);
        let _ = app.update(Message::NextPage);
        assert_eq!(app.page.index(), 1);
        let _ = app.update(Message::SetSearch("vaca".into()));
        assert_eq!(app.filter.search, "vaca");
        assert_eq!(app.page.index(), 0);
    }

    #[test]
    fn set_sort_same_key_flips_order() {
        let (mut app, _dir) = app_with_entries(&["a.jpg"]);
        assert_eq!(app.sort_order, SortOrder::Asc);
        let _ = app.update(Message::SetSort(SortKey::Name));
        assert_eq!(app.sort_order, SortOrder::Desc);
        let _ = app.update(Message::SetSort(SortKey::Size));
        assert_eq!(app.sort, SortKey::Size);
        assert_eq!(app.sort_order, SortOrder::Asc);
    }

    #[test]
    fn toggle_filters_flip_state() {
        let (mut app, _dir) = app_with_entries(&["a.jpg"]);
        assert!(app.filter.hide_folders);
        let _ = app.update(Message::ToggleHideFolders);
        assert!(!app.filter.hide_folders);
        let _ = app.update(Message::ToggleOnlyMedia);
        assert!(!app.filter.only_media);
    }

    // ---- Fase 2: vista de duplicados ----

    fn file_info(name: &str, created_at: Option<i64>) -> FileInfo {
        FileInfo {
            path: name.to_string(),
            name: name.to_string(),
            size_bytes: 100,
            kind: ddamr_core::MediaKind::Image,
            hash_blake3: None,
            dhash_image: None,
            dhash_video: None,
            width: Some(64),
            height: Some(64),
            duration_ms: None,
            created_at,
            modified_at: created_at,
        }
    }

    fn app_with_duplicates() -> App {
        App {
            duplicates: vec![DuplicateGroup {
                kind: DuplicateKind::Exact,
                files: vec![
                    file_info("vieja.jpg", Some(1_000_000)),
                    file_info("nueva.jpg", Some(2_000_000)),
                ],
                total_bytes: 200,
            }],
            ..App::default()
        }
    }

    #[test]
    fn show_duplicates_and_loaded_groups() {
        let mut app = app_with_duplicates();
        let _ = app.update(Message::ShowDuplicates);
        assert_eq!(app.view, View::Duplicates);
        assert!(app.duplicates_loading);
        let groups = vec![DuplicateGroup {
            kind: DuplicateKind::Exact,
            files: vec![file_info("a.jpg", None), file_info("b.jpg", None)],
            total_bytes: 200,
        }];
        let _ = app.update(Message::DuplicatesLoaded(groups));
        assert!(!app.duplicates_loading);
        assert_eq!(app.duplicates.len(), 1);
        assert_eq!(app.duplicates[0].files.len(), 2);
        assert!(app.status_msg.contains("1 grupos"));
    }

    #[test]
    fn toggle_duplicate_group_collapses_and_expands() {
        let mut app = app_with_duplicates();
        let _ = app.update(Message::ToggleDuplicateGroup(0));
        assert!(app.collapsed_groups.contains(&0));
        let _ = app.update(Message::ToggleDuplicateGroup(0));
        assert!(!app.collapsed_groups.contains(&0));
        let _ = app.update(Message::CollapseAllDuplicates);
        assert!(app.collapsed_groups.contains(&0));
        let _ = app.update(Message::ExpandAllDuplicates);
        assert!(app.collapsed_groups.is_empty());
    }

    #[test]
    fn select_oldest_and_newest_pick_by_date() {
        let mut app = app_with_duplicates();
        let _ = app.update(Message::SelectOldest(0));
        assert!(
            app.dup_selected.contains(&(0, 0)),
            "el más antiguo es vieja.jpg"
        );
        assert!(!app.dup_selected.contains(&(0, 1)));
        let _ = app.update(Message::SelectNewest(0));
        assert!(
            app.dup_selected.contains(&(0, 1)),
            "el más nuevo es nueva.jpg"
        );
        let _ = app.update(Message::ToggleDuplicateFile(0, 0));
        assert!(!app.dup_selected.contains(&(0, 0)));
        let _ = app.update(Message::DeselectAll);
        assert!(app.dup_selected.is_empty());
    }

    #[test]
    fn trash_requires_confirmation_dialog() {
        let mut app = app_with_duplicates();
        // Sin selección no se abre el diálogo.
        let _ = app.update(Message::TrashSelected);
        assert!(app.confirm_dialog.is_none());
        let _ = app.update(Message::SelectOldest(0));
        let _ = app.update(Message::TrashSelected);
        assert_eq!(app.confirm_dialog, Some(ConfirmKind::Trash));
        let _ = app.update(Message::CancelDialog);
        assert!(app.confirm_dialog.is_none());
        // Confirmar cierra el diálogo (la papelera va por Task).
        let _ = app.update(Message::TrashSelected);
        let _ = app.update(Message::ConfirmTrash);
        assert!(app.confirm_dialog.is_none());
    }

    #[test]
    fn move_selected_without_dest_is_rejected() {
        let mut app = app_with_duplicates();
        let _ = app.update(Message::SelectOldest(0));
        let _ = app.update(Message::MoveSelected);
        assert!(app.status_msg.contains("destino"));
        let _ = app.update(Message::SetMoveDest("/etc".into()));
        let _ = app.update(Message::MoveSelected);
        assert!(app.status_msg.contains("dentro de la raíz"));
    }

    #[test]
    fn archive_done_reports_files_without_date() {
        let mut app = app_with_duplicates();
        let _ = app.update(Message::ArchiveDone(2, 3));
        assert!(
            app.status_msg
                .contains("No se pudieron archivar 3 archivos")
        );
        let _ = app.update(Message::ArchiveDone(2, 0));
        assert!(app.status_msg.contains("2 archivos archivados"));
        assert!(!app.status_msg.contains("No se pudieron"));
    }

    // ---- Fase 2b: formulario de edición, etiquetas, ubicaciones, regiones ----

    fn app_with_meta() -> (App, TempDir) {
        let (mut app, dir) = app_with_entries(&["a.jpg", "b.jpg"]);
        // Simular la carga de metadatos del seleccionado.
        let _ = app.update(Message::SelectedMetadataLoaded(BasicMetadata {
            width: Some(64),
            height: Some(64),
            created_at: Some(1_704_164_645_000),
            has_description: true,
            description: Some("una descripción".into()),
            gps: Some((41.38, 2.16)),
            ..Default::default()
        }));
        (app, dir)
    }

    fn details_with_tags() -> SelectedDetails {
        SelectedDetails {
            tags: vec![
                Tag {
                    id: 1,
                    name: "Playa".into(),
                    usage_count: 1,
                },
                Tag {
                    id: 2,
                    name: "Sol".into(),
                    usage_count: 1,
                },
            ],
            location: None,
            locations: Vec::new(),
            regions: Vec::new(),
            file_id: Some(1),
            indexed: true,
        }
    }

    #[test]
    fn edit_form_loads_from_metadata_and_tags() {
        let (mut app, _dir) = app_with_meta();
        assert_eq!(app.edit_form.description, "una descripción");
        assert_eq!(app.edit_form.datetime_original, "2024:01:02 03:04:05");
        assert_eq!(app.edit_form.lat, "41.380000");
        assert_eq!(app.edit_form.lon, "2.160000");
        assert!(!app.edit_form.dirty);
        // Las palabras clave llegan con los detalles (DB).
        let _ = app.update(Message::SelectedDetailsLoaded(Ok(details_with_tags())));
        assert_eq!(app.edit_form.keywords, "Playa, Sol");
        assert!(!app.edit_form.dirty, "cargar no marca cambios");
    }

    #[test]
    fn details_error_marks_db_unavailable() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SelectedDetailsLoaded(Err(
            "DB: no se pudo abrir".into()
        )));
        assert!(!app.db_available);
        assert!(app.db_details_error.is_some());
        assert!(app.selected_details.is_none());
    }

    #[test]
    fn changing_selection_discards_unsaved_changes() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SetEditDescription("cambio sin guardar".into()));
        assert!(app.edit_form.dirty);
        let _ = app.update(Message::SelectNext);
        assert_eq!(app.selected_index, 1);
        assert!(!app.edit_form.dirty);
        assert!(app.edit_form.description.is_empty());
        assert!(app.edit_form_path.is_none());
    }

    #[test]
    fn save_rejects_invalid_date_and_gps() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SetEditDate("2024:13:01 00:00:00".into()));
        let _ = app.update(Message::SaveMetadataEdit);
        assert!(app.status_msg.contains("No se guardó"));
        assert!(app.status_msg.contains("fecha inválida"));
        let _ = app.update(Message::SetEditDate("2024:05:01 10:30:00".into()));
        let _ = app.update(Message::SetEditLat("100".into()));
        let _ = app.update(Message::SaveMetadataEdit);
        assert!(app.status_msg.contains("No se guardó"));
        assert!(app.status_msg.contains("latitud"));
    }

    #[test]
    fn save_with_empty_form_reports_no_changes() {
        let (mut app, _dir) = app_with_entries(&["a.jpg"]);
        let _ = app.update(Message::SaveMetadataEdit);
        assert!(app.status_msg.contains("Sin cambios"));
    }

    #[test]
    fn discard_restores_form_from_loaded_metadata() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SelectedDetailsLoaded(Ok(details_with_tags())));
        let _ = app.update(Message::SetEditDescription("otra cosa".into()));
        assert!(app.edit_form.dirty);
        let _ = app.update(Message::DiscardMetadataEdit);
        assert!(!app.edit_form.dirty);
        assert_eq!(app.edit_form.description, "una descripción");
        assert_eq!(app.edit_form.keywords, "Playa, Sol");
        assert!(app.status_msg.contains("descartados"));
    }

    #[test]
    fn metadata_edit_done_updates_entry_and_clears_dirty() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SetEditDescription("nueva".into()));
        let _ = app.update(Message::MetadataEditDone(Ok(EditOutcome {
            size_bytes: 1234,
            modified_at: Some(2_000_000),
            warning: String::new(),
        })));
        assert!(!app.edit_form.dirty);
        assert_eq!(app.entries.items[0].size_bytes, 1234);
        assert_eq!(app.entries.items[0].modified_at, Some(2_000_000));
        assert!(app.status_msg.contains("Metadatos guardados"));
        let _ = app.update(Message::MetadataEditDone(Err(
            "exiftool no disponible".into()
        )));
        assert!(app.status_msg.contains("No se guardaron"));
        assert!(app.status_msg.contains("exiftool no disponible"));
    }

    #[test]
    fn tag_op_done_clears_input_and_reports() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SetNewTag("Playa".into()));
        let _ = app.update(Message::TagOpDone(Ok(())));
        assert!(app.new_tag.is_empty());
        assert!(app.status_msg.contains("Etiqueta actualizada"));
        let _ = app.update(Message::TagOpDone(Err(
            "archivo no indexado en la DB: a.jpg".into(),
        )));
        assert!(app.status_msg.contains("no indexado"));
        assert!(app.db_available, "no indexado no es fallo de la DB");
    }

    #[test]
    fn add_region_rejects_overflow_and_accepts_valid() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SetRegionName("Ana".into()));
        let _ = app.update(Message::SetRegionX("90".into()));
        let _ = app.update(Message::SetRegionY("0".into()));
        let _ = app.update(Message::SetRegionW("20".into()));
        let _ = app.update(Message::SetRegionH("10".into()));
        let _ = app.update(Message::AddRegion);
        assert!(app.status_msg.contains("desborda"));
        let _ = app.update(Message::SetRegionX("10".into()));
        // Con x+w<=100 la validación pasa: el trabajo va por Task
        // y `status_msg` no cambia (un fallo de validación sí lo
        // sobrescribiría con el error).
        let before = app.status_msg.clone();
        let _ = app.update(Message::AddRegion);
        assert_eq!(app.status_msg, before);
        let _ = app.update(Message::RegionOpDone(Ok("Región añadida.".into())));
        assert!(app.region_name.is_empty());
        assert!(app.status_msg.contains("Región añadida"));
    }

    #[test]
    fn create_location_rejects_bad_coordinates() {
        let (mut app, _dir) = app_with_meta();
        let _ = app.update(Message::SetNewLocName("Playa".into()));
        let _ = app.update(Message::SetNewLocLat("200".into()));
        let _ = app.update(Message::SetNewLocLon("2".into()));
        let _ = app.update(Message::CreateLocation);
        assert!(app.status_msg.contains("latitud"));
        let _ = app.update(Message::LocationOpDone(Err("DB: boom".into())));
        assert!(!app.db_available);
    }

    // ---- Fase 3a: miniaturas, preview y lista virtualizada ----

    #[test]
    fn thumbnails_loaded_updates_cache_and_marks_failures() {
        let (mut app, _dir) = app_with_entries(&["a.jpg", "b.jpg"]);
        let path_a = app.entries.items[0].path.path().to_path_buf();
        let path_b = app.entries.items[1].path.path().to_path_buf();
        let mtime_a = app.entries.items[0].modified_at;
        let _ = app.update(Message::ThumbnailsLoaded(vec![
            (path_a.clone(), mtime_a, Some(vec![0xFF, 0xD8, 0xFF, 0xD9])),
            (path_b.clone(), None, None),
        ]));
        assert!(
            app.thumb_cache.peek_validated(&path_a, mtime_a).is_some(),
            "la miniatura generada queda en caché"
        );
        assert!(app.thumb_failed.contains(&path_b), "el fallo se marca");
        // Sin reintento en esta sesión: ni la fallida ni la ya
        // cacheada se vuelven a pedir.
        let needed = app.thumbs_needed();
        assert!(
            needed.iter().all(|(p, _, _)| p.path() != path_a),
            "a ya está en caché"
        );
        assert!(
            needed.iter().all(|(p, _, _)| p.path() != path_b),
            "b falló y no se reintenta"
        );
    }

    #[test]
    fn select_region_toggles_highlight() {
        let (mut app, _dir) = app_with_meta();
        assert!(app.selected_region.is_none());
        let _ = app.update(Message::SelectRegion(7));
        assert_eq!(app.selected_region, Some(7));
        let _ = app.update(Message::SelectRegion(7));
        assert!(app.selected_region.is_none(), "repetir deselecciona");
        let _ = app.update(Message::SelectRegion(3));
        assert_eq!(app.selected_region, Some(3));
    }

    #[test]
    fn scroll_changed_requests_new_window_only_when_needed() {
        let mut app = paged_app(5000);
        // Desplazar hasta la fila 300: ventana alineada a 60.
        let _ = app.update(Message::ScrollChanged(72.0 * 300.0, 720.0));
        assert_eq!(app.scroll_offset_px, 72.0 * 300.0);
        assert_eq!(app.pending_window_start, Some(300));
        // Un scroll pequeño dentro de la ventana 0 no pide
        // ventana nueva (sí puede precargar la adyacente).
        let mut app = paged_app(5000);
        let _ = app.update(Message::ScrollChanged(72.0 * 10.0, 720.0));
        assert_eq!(app.pending_window_start, None);
    }

    #[test]
    fn boot_expands_root_and_fills_content_panel() {
        let (mut app, _task) = App::boot();
        assert_eq!(app.tree.len(), 1);
        assert_eq!(
            app.tree[0].state,
            DirState::Loading,
            "la raíz arranca expandiéndose"
        );
        assert_eq!(app.selected_dir, app.root);
        let root_path = app.root.path().to_path_buf();
        let _ = app.update(Message::DirLoaded(
            root_path,
            vec![DirNode::new(app.root.clone())],
        ));
        assert_eq!(app.tree[0].state, DirState::Loaded);
        assert!(
            app.entries.items.is_empty(),
            "antes de EntriesLoaded el panel está vacío"
        );
        let (app2, _dir) = app_with_entries(&["a.jpg", "b.jpg"]);
        let page = app2.entries.clone();
        let _ = app.update(Message::EntriesLoaded(page));
        assert_eq!(
            app.entries.items.len(),
            2,
            "el panel muestra el contenido del raíz al arrancar"
        );
    }

    #[test]
    fn scroll_near_edge_prefetches_and_crossing_has_no_gap() {
        let mut app = paged_app(5000);
        // Scroll dentro de la ventana 0 pero cerca de su final
        // (viewport de 720 px ≈ 10 filas, llega a la fila ~55).
        let _ = app.update(Message::ScrollChanged(72.0 * 45.0, 720.0));
        assert!(
            app.prefetch_pending.contains(&60),
            "la ventana 60 se precarga antes de llegar al borde"
        );
        // Simular su llegada: queda guardada sin tocar la visible.
        let entry = app.entries.items[0].clone();
        let page = Page::new(60, 60);
        let _ = app.update(Message::PrefetchLoaded(
            60,
            PageResult::new(vec![entry; 60], 5000, page),
        ));
        assert!(app.prefetched.contains_key(&60));
        assert_eq!(
            app.window_start(),
            0,
            "la visible no cambia con el prefetch"
        );
        // Al cruzar el borde, la ventana ya está: cambio inmediato.
        let _ = app.update(Message::ScrollChanged(72.0 * 60.0, 720.0));
        assert_eq!(app.window_start(), 60);
        assert!(
            app.pending_window_start.is_none(),
            "sin carga en vuelo: no hay hueco vacío"
        );
    }

    #[test]
    fn clicking_selected_row_keeps_preview_but_changing_clears_it() {
        let (mut app, _dir) = app_with_entries(&["a.jpg", "b.jpg"]);
        let path_a = app.entries.items[0].path.path().to_path_buf();
        let _ = app.update(Message::PreviewLoaded(path_a.clone(), Some(vec![1, 2, 3])));
        assert!(app.preview.is_some());
        // Clic en la misma fila: el preview se conserva.
        let _ = app.update(Message::ToggleSelect(0));
        assert!(
            app.preview.is_some(),
            "el preview no debe regenerarse al repulsar la misma fila"
        );
        // Clic en otra fila: se descarta y se regenera.
        let _ = app.update(Message::ToggleSelect(1));
        assert!(app.preview.is_none());
    }

    #[test]
    fn tree_icons_are_folder_emojis() {
        assert_eq!(tree_icon(DirState::Closed), "📁");
        assert_eq!(tree_icon(DirState::Loaded), "📂");
        assert_eq!(tree_icon(DirState::Loading), "⏳");
    }

    #[test]
    fn move_selection_across_window_loads_next_window() {
        let mut app = paged_app(5000);
        app.selected_index = 199; // última fila de la ventana 0
        let _ = app.update(Message::SelectNext);
        // La fila 200 cae en la ventana 180..240 (alineada a 60).
        assert_eq!(app.pending_window_start, Some(180));
        assert_eq!(app.pending_selected_index, Some(20));
        // Simular la carga de esa ventana: 60 filas desde 180.
        let entry = app.entries.items[0].clone();
        let page = Page::new(180, 60);
        let _ = app.update(Message::EntriesLoaded(PageResult::new(
            vec![entry; 60],
            5000,
            page,
        )));
        assert_eq!(app.selected_index, 20);
        assert!(app.pending_window_start.is_none());
        assert!(app.pending_selected_index.is_none());
    }

    #[test]
    fn preview_loaded_sets_preview_and_ignores_stale_path() {
        let (mut app, _dir) = app_with_entries(&["a.jpg", "b.jpg"]);
        let path_a = app.entries.items[0].path.path().to_path_buf();
        let _ = app.update(Message::PreviewLoaded(path_a.clone(), Some(vec![1, 2, 3])));
        assert!(app.preview.is_some());
        assert_eq!(app.preview.as_ref().unwrap().path, path_a);
        // Un preview de otro archivo (la selección ya cambió)
        // se ignora: no sustituye al actual.
        let path_b = app.entries.items[1].path.path().to_path_buf();
        let _ = app.update(Message::PreviewLoaded(path_b.clone(), Some(vec![9])));
        assert_eq!(app.preview.as_ref().unwrap().path, path_a);
        // Un fallo queda marcado para no reintentarlo.
        let _ = app.update(Message::PreviewLoaded(path_a.clone(), None));
        assert!(app.preview_failed.contains(&path_a));
    }

    #[test]
    fn ym_from_ms_known_dates() {
        assert_eq!(ym_from_ms(0), Some((1970, 1)));
        // 2024-01-02 03:04:05 UTC = 1704164645 s.
        assert_eq!(ym_from_ms(1_704_164_645_000), Some((2024, 1)));
        // 2026-11-19 12:00 UTC = 1795128000 s.
        assert_eq!(ym_from_ms(1_795_128_000_000), Some((2026, 11)));
    }
}
