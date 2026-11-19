//! `App`: estado, mensajes y vista de la fase 1.
//!
//! Regla de oro: `update` es pura lógica de estado. Nunca toca el
//! disco ni la DB directamente: el trabajo pesado va en
//! `Task::perform` con funciones de `ddamr-fs` / `ddamr-metadata`,
//! y el resultado vuelve como un `Message`.
use crate::virtual_list::{ThumbCache, page_slice};
use ddamr_core::{
    Config, FileFilter, Page, PageResult, SafePath, SortKey, SortOrder, format_size,
    format_timestamp_ms,
};
use ddamr_fs::{
    DirNode, DirState, FileEntry, find_node_mut, list_dir, load_children, visible_nodes,
};
use ddamr_metadata::{BasicMetadata, read_basic};
use iced::keyboard::{Key, key::Named};
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use std::collections::HashSet;
use std::path::PathBuf;

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

    fn move_selection(&mut self, delta: isize) -> Task<Message> {
        if self.entries.items.is_empty() {
            return Task::none();
        }
        let max = self.entries.items.len() - 1;
        let next = self.selected_index as isize + delta;
        self.selected_index = next.clamp(0, max as isize) as usize;
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
                    self.toggle_current()
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
                Task::none()
            }
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
        column![
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
            text("Solo lectura en fase 1: sin formulario de edición.").size(12),
        ]
        .spacing(6)
        .into()
    }

    pub fn view(&self) -> Element<'_, Message> {
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
}
