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
use crate::virtual_list::{ThumbCache, page_slice};
use ddamr_core::{
    AppError, Config, FileFilter, Page, PageResult, SafePath, SortKey, SortOrder, format_size,
    format_timestamp_ms,
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
use ddamr_metadata::{BasicMetadata, read_basic, write_metadata};
use iced::keyboard::{Key, key::Named};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use std::collections::HashSet;
use std::path::PathBuf;

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
    /// Caché LRU de miniaturas, lista para la iteración que las dibuje.
    #[allow(dead_code)]
    pub thumb_cache: ThumbCache,
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
            thumb_cache: ThumbCache::new(128),
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

    /// Carga la página actual del directorio seleccionado, en un Task.
    fn load_entries_task(&self) -> Task<Message> {
        let dir = self.selected_dir.clone();
        let filter = self.filter.clone();
        let sort = self.sort;
        let order = self.sort_order;
        let page = self.page;
        Task::perform(
            async move { list_dir(&dir, &filter, sort, order, page) },
            move |result| match result {
                Ok(page_result) => Message::EntriesLoaded(page_result),
                Err(_) => Message::EntriesLoaded(PageResult::empty(page)),
            },
        )
    }

    /// Metadatos del archivo seleccionado, en un Task.
    fn metadata_task(&self) -> Task<Message> {
        let Some(entry) = self.entries.items.get(self.selected_index) else {
            return Task::none();
        };
        let path = entry.path.clone();
        Task::perform(
            async move { read_basic(&path) },
            Message::SelectedMetadataLoaded,
        )
    }

    /// Al cambiar de selección, los cambios sin guardar se
    /// descartan silenciosamente y el formulario, las secciones
    /// de la DB y los campos de alta se vacían: se recargan con
    /// la nueva selección (metadatos → detalles).
    fn selection_changed(&mut self) {
        self.edit_form = EditForm::default();
        self.edit_form_path = None;
        self.selected_details = None;
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

    fn move_selection(&mut self, delta: isize) -> Task<Message> {
        if self.entries.items.is_empty() {
            return Task::none();
        }
        let max = self.entries.items.len() - 1;
        let next = self.selected_index as isize + delta;
        let next = next.clamp(0, max as isize) as usize;
        if next != self.selected_index {
            self.selected_index = next;
            self.selection_changed();
        }
        self.metadata_task()
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
                self.entries = page_result;
                self.selected_index = 0;
                self.selected_metadata = None;
                self.selection_changed();
                self.status_msg = format!(
                    "{} archivos · página {} de {}",
                    total,
                    self.page.index() + 1,
                    self.total_pages()
                );
                self.metadata_task()
            }
            Message::SelectNext => self.move_selection(1),
            Message::SelectPrev => self.move_selection(-1),
            Message::ToggleSelect(index) => {
                if index < self.entries.items.len() {
                    self.selected_index = index;
                    self.selection_changed();
                    let _ = self.toggle_current();
                    self.metadata_task()
                } else {
                    Task::none()
                }
            }
            Message::SetSearch(query) => {
                self.filter.search = query;
                self.page = Page::first();
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
                self.load_entries_task()
            }
            Message::ToggleHideFolders => {
                self.filter.hide_folders = !self.filter.hide_folders;
                self.page = Page::first();
                self.load_entries_task()
            }
            Message::ToggleOnlyMedia => {
                self.filter.only_media = !self.filter.only_media;
                self.page = Page::first();
                self.load_entries_task()
            }
            Message::ToggleShowHidden => {
                self.filter.show_hidden = !self.filter.show_hidden;
                self.page = Page::first();
                self.load_entries_task()
            }
            Message::NextPage => {
                if self.page.index() + 1 < self.total_pages() {
                    self.page = Page::at(self.page.index() + 1);
                    self.load_entries_task()
                } else {
                    Task::none()
                }
            }
            Message::PrevPage => {
                if self.page.index() > 0 {
                    self.page = Page::at(self.page.index() - 1);
                    self.load_entries_task()
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
            let icon = match node.state {
                DirState::Closed => "[>]",
                DirState::Loaded => "[v]",
                DirState::Loading => "[...]",
            };
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
        scrollable(col).width(Length::Fill).into()
    }

    fn list_view(&self) -> Element<'_, Message> {
        let mut col = column![].spacing(2);
        for (i, entry) in page_slice(&self.entries.items, self.entries.page)
            .iter()
            .enumerate()
        {
            let marker = if i == self.selected_index {
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
            col = col.push(
                button(text(line).size(13))
                    .on_press(Message::ToggleSelect(i))
                    .width(Length::Fill),
            );
        }
        if self.entries.items.is_empty() {
            col = col.push(text("Sin archivos en esta carpeta con los filtros actuales.").size(13));
        }
        scrollable(col).width(Length::Fill).into()
    }

    /// Columna derecha (fase 2b): información de solo lectura
    /// arriba, formulario de edición de metadatos, etiquetas,
    /// ubicación y regiones. Sin miniatura dibujada todavía: las
    /// regiones son lista + formulario, sin overlay gráfico.
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
        let mut col = column![
            text("Información").size(14),
            text(format!("Nombre: {}", entry.name)).size(13),
            text(format!("Ruta: {}", entry.path.relative().display())).size(13),
            text(format!("Tamaño: {}", format_size(entry.size_bytes))).size(13),
            text(format!("Tipo: {}", entry.kind.as_str())).size(13),
            text(format!("Dimensiones: {dims}")).size(13),
            text(format!("Fecha: {created}")).size(13),
            text(format!(
                "Etiquetas: {}",
                flag(meta.is_some_and(|m| m.has_keywords))
            ))
            .size(13),
            text(format!(
                "Ubicación: {}",
                flag(meta.is_some_and(|m| m.has_location))
            ))
            .size(13),
            text(format!(
                "Descripción: {}",
                flag(meta.is_some_and(|m| m.has_description))
            ))
            .size(13),
        ]
        .spacing(6);
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
        // ---- Etiquetas ----
        col = col.push(text("Etiquetas").size(14));
        col = self.push_tags_section(col);
        // ---- Ubicación ----
        col = col.push(text("Ubicación").size(14));
        col = self.push_location_section(col);
        // ---- Regiones ----
        col = col.push(text("Regiones").size(14));
        col = self.push_regions_section(col);
        scrollable(col).width(Length::Fill).into()
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
            col = col.push(
                row![
                    text(format!(
                        "{} — {:.1}%, {:.1}% · {:.1}×{:.1}%",
                        region.name,
                        region.x * 100.0,
                        region.y * 100.0,
                        region.w * 100.0,
                        region.h * 100.0
                    ))
                    .size(12),
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
            let icon = if collapsed { "[>]" } else { "[v]" };
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
        scrollable(col).width(Length::Fill).into()
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

    #[test]
    fn ym_from_ms_known_dates() {
        assert_eq!(ym_from_ms(0), Some((1970, 1)));
        // 2024-01-02 03:04:05 UTC = 1704164645 s.
        assert_eq!(ym_from_ms(1_704_164_645_000), Some((2024, 1)));
        // 2026-11-19 12:00 UTC = 1795128000 s.
        assert_eq!(ym_from_ms(1_795_128_000_000), Some((2026, 11)));
    }
}
