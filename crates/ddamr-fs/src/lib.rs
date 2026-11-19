//! ddamr-fs: recorrido en streaming, árbol lazy y listado paginado.
//!
//! Nada aquí materializa una carpeta entera en un `Vec` de contenido:
//! `walk` es un iterador perezoso y `list_dir` solo guarda un
//! `FileEntry` (metadatos del sistema de archivos) por entrada,
//! nunca miniaturas ni metadatos EXIF. Las mutaciones
//! (mover/archivar) irán por el patrón respaldo → copia → verificar
//! → rename atómico (fase 2).
use ddamr_core::{
    AppError, FileFilter, MediaKind, Page, PageResult, Result, SafePath, SortKey, SortOrder,
};
use std::fs::DirEntry;
use std::path::Path;

/// Recorrido no recursivo en streaming de un directorio.
/// Devuelve un iterador; el llamador decide cuántos lotes consume.
/// Los errores de una entrada concreta no abortan el resto.
pub fn walk(dir: &SafePath) -> Result<impl Iterator<Item = Result<DirEntry>>> {
    let rd = std::fs::read_dir(dir.path()).map_err(|e| AppError::Io {
        path: dir.path().to_path_buf(),
        source: e,
    })?;
    Ok(rd.map(|entry| entry.map_err(AppError::IoPlain)))
}

/// Estado de un nodo del árbol de directorios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirState {
    /// Nunca se han leído sus hijos.
    Closed,
    /// Se están leyendo en segundo plano (la UI lo pidió).
    Loading,
    /// Hijos ya leídos (puede ser una lista vacía).
    Loaded,
}

/// Un nodo del árbol de navegación. Solo guarda directorios:
/// los archivos se leen con `list_dir` al seleccionar el nodo.
/// Los hijos no existen hasta que se expande el nodo.
#[derive(Debug, Clone)]
pub struct DirNode {
    pub path: SafePath,
    pub state: DirState,
    pub children: Vec<DirNode>,
}

impl DirNode {
    pub fn new(path: SafePath) -> Self {
        Self {
            path,
            state: DirState::Closed,
            children: Vec::new(),
        }
    }

    pub fn name(&self) -> String {
        self.path
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.path().display().to_string())
    }

    /// Lee solo los subdirectorios inmediatos, en streaming, y los
    /// deja como hijos en estado `Closed`. No toca archivos.
    /// Idempotente: si ya está `Loaded`, no vuelve a leer.
    pub fn load_children(&mut self) -> Result<()> {
        if self.state == DirState::Loaded {
            return Ok(());
        }
        self.children = load_children(&self.path)?;
        self.state = DirState::Loaded;
        Ok(())
    }
}

/// Lee los subdirectorios inmediatos de `dir`, ordenados por nombre.
/// Solo directorios: los archivos no pertenecen al árbol.
pub fn load_children(dir: &SafePath) -> Result<Vec<DirNode>> {
    let mut children = Vec::new();
    for entry in walk(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // una entrada ilegible no aborta el árbol
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue; // directorios ocultos no se muestran en el árbol
        }
        let child = match dir.join(Path::new(&name)) {
            Ok(p) => p,
            Err(_) => continue, // symlink que escapa de la raíz: se ignora
        };
        children.push(DirNode::new(child));
    }
    children.sort_by(|a, b| {
        a.name()
            .to_ascii_lowercase()
            .cmp(&b.name().to_ascii_lowercase())
    });
    Ok(children)
}

/// Una entrada del panel de contenido: metadatos del sistema de
/// archivos, sin miniatura ni EXIF. `created_at`/`modified_at` son
/// epoch en milisegundos (`None` si el sistema no los da).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub name_without_ext: String,
    pub ext: String,
    pub size_bytes: u64,
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub kind: MediaKind,
    pub is_dir: bool,
    /// Banderas de metadatos. En fase 1 solo se rellenan desde el
    /// indexador; el listado en caliente las deja en `false`.
    pub has_keywords: bool,
    pub has_location: bool,
    pub has_description: bool,
    /// Atributo extendido de origen (macOS). En Linux, `false`.
    pub has_xattr_origin: bool,
    pub path: SafePath,
}

impl FileEntry {
    /// Construye un `FileEntry` desde una entrada de `read_dir`.
    /// Un error de metadatos deja tamaño/fechas en 0/`None` en vez
    /// de abortar el listado.
    pub fn from_dir_entry(root: &Path, entry: &DirEntry) -> Result<Self> {
        let path = SafePath::new(root, &entry.path())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().ok();
        let is_dir = file_type.map(|t| t.is_dir()).unwrap_or(false);
        let md = entry.metadata().ok();
        let (size_bytes, created_at, modified_at) = match &md {
            Some(m) => (
                m.len(),
                m.created().ok().and_then(system_time_ms),
                m.modified().ok().and_then(system_time_ms),
            ),
            None => (0, None, None),
        };
        let (stem, ext) = split_name(&name, is_dir);
        let kind = if is_dir {
            MediaKind::Dir
        } else {
            MediaKind::from_ext(&ext)
        };
        Ok(Self {
            name,
            name_without_ext: stem,
            ext,
            size_bytes,
            created_at,
            modified_at,
            kind,
            is_dir,
            has_keywords: false,
            has_location: false,
            has_description: false,
            has_xattr_origin: has_xattr_origin(&path),
            path,
        })
    }
}

fn system_time_ms(t: std::time::SystemTime) -> Option<i64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// "foto.jpg" → ("foto", "jpg"); un directorio no separa extensión.
fn split_name(name: &str, is_dir: bool) -> (String, String) {
    if is_dir {
        return (name.to_string(), String::new());
    }
    match name.rfind('.') {
        Some(0) => (name.to_string(), String::new()), // ".oculto" no tiene ext
        Some(i) => (name[..i].to_string(), name[i + 1..].to_string()),
        None => (name.to_string(), String::new()),
    }
}

/// Origen por atributo extendido. En Linux no hay equivalente
/// directo de `com.apple.metadata:kMDItemWhereFroms`; en macOS se
/// implementará en fase 2 con la crate `xattr`.
fn has_xattr_origin(_path: &SafePath) -> bool {
    #[cfg(target_os = "macos")]
    {
        // Se activará en fase 2; en fase 1 el flag queda en false
        // para no añadir una dependencia solo para esto.
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Lista un directorio: lee en streaming, filtra, ordena por la
/// clave pedida (con el nombre como desempate) y devuelve solo la
/// página pedida. No genera miniaturas ni lee EXIF.
pub fn list_dir(
    dir: &SafePath,
    filter: &FileFilter,
    sort: SortKey,
    order: SortOrder,
    page: Page,
) -> Result<PageResult<FileEntry>> {
    let root = dir.root().to_path_buf();
    let mut entries: Vec<FileEntry> = Vec::new();
    for entry in walk(dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_hidden = name.starts_with('.');
        let ext = if is_dir {
            String::new()
        } else {
            split_name(&name, false).1
        };
        let kind = if is_dir {
            MediaKind::Dir
        } else {
            MediaKind::from_ext(&ext)
        };
        if !filter.matches(&name, is_dir, is_hidden, kind) {
            continue;
        }
        match FileEntry::from_dir_entry(&root, &entry) {
            Ok(fe) => entries.push(fe),
            Err(_) => continue,
        }
    }
    sort_entries(&mut entries, sort, order);
    let total = entries.len();
    let start = page.offset.min(total);
    let end = start.saturating_add(page.limit).min(total);
    let items = entries[start..end].to_vec();
    Ok(PageResult::new(items, total, page))
}

fn sort_entries(entries: &mut [FileEntry], sort: SortKey, order: SortOrder) {
    entries.sort_by(|a, b| {
        let ord = match sort {
            SortKey::Name => a
                .name_without_ext
                .to_ascii_lowercase()
                .cmp(&b.name_without_ext.to_ascii_lowercase()),
            SortKey::Extension => a
                .ext
                .to_ascii_lowercase()
                .cmp(&b.ext.to_ascii_lowercase())
                .then_with(|| {
                    a.name_without_ext
                        .to_ascii_lowercase()
                        .cmp(&b.name_without_ext.to_ascii_lowercase())
                }),
            SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
            SortKey::Date => a.modified_at.cmp(&b.modified_at),
        };
        // Desempate estable por nombre completo para que el orden
        // sea determinista entre páginas.
        let ord = ord.then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
        match order {
            SortOrder::Asc => ord,
            SortOrder::Desc => ord.reverse(),
        }
    });
}

/// Busca un nodo por ruta en un bosque, en profundidad.
///
/// Los nodos guardan rutas canónicas (`SafePath`), pero quien busca
/// puede pasar una ruta sin canonicalizar: en macOS `/var` es un
/// symlink a `/private/var`, así que `TempDir` da `/var/folders/...`
/// mientras el nodo guarda `/private/var/folders/...` y la
/// comparación directa falla. Por eso se canonicaliza la ruta
/// buscada antes de comparar; si no existe, se compara tal cual.
pub fn find_node_mut<'a>(nodes: &'a mut [DirNode], path: &Path) -> Option<&'a mut DirNode> {
    let search = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    find_node_mut_inner(nodes, &search)
}

fn find_node_mut_inner<'a>(nodes: &'a mut [DirNode], search: &Path) -> Option<&'a mut DirNode> {
    for node in nodes {
        if node.path.path() == search {
            return Some(node);
        }
        if let Some(found) = find_node_mut_inner(&mut node.children, search) {
            return Some(found);
        }
    }
    None
}

/// Aplana el árbol visible: solo los nodos expandidos aportan hijos.
/// Devuelve (profundidad, nodo) en orden de visualización.
pub fn visible_nodes(nodes: &[DirNode]) -> Vec<(usize, &DirNode)> {
    let mut out = Vec::new();
    flatten(nodes, 0, &mut out);
    out
}

fn flatten<'a>(nodes: &'a [DirNode], depth: usize, out: &mut Vec<(usize, &'a DirNode)>) {
    for node in nodes {
        out.push((depth, node));
        if node.state == DirState::Loaded {
            flatten(&node.children, depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddamr_core::SafePath;
    use tempfile::TempDir;

    fn fixture_tree() -> TempDir {
        let root = TempDir::new().unwrap();
        let base = root.path();
        // 3 niveles: a/b/c, a/d, e
        for d in ["a", "a/b", "a/b/c", "a/d", "e"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("a/foto.jpg"), b"x").unwrap();
        std::fs::write(base.join("a/doc.pdf"), b"x").unwrap();
        root
    }

    #[test]
    fn tree_is_lazy_children_not_loaded_until_expand() {
        let root = fixture_tree();
        let safe = SafePath::from_root(root.path()).unwrap();
        let mut node = DirNode::new(safe);
        assert_eq!(node.state, DirState::Closed);
        assert!(node.children.is_empty());

        node.load_children().unwrap();
        assert_eq!(node.state, DirState::Loaded);
        let names: Vec<String> = node.children.iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["a".to_string(), "e".to_string()]);
        // Los hijos siguen cerrados: sus nietos no se han leído.
        assert!(node.children.iter().all(|c| c.state == DirState::Closed));
        assert!(node.children.iter().all(|c| c.children.is_empty()));

        // Expandir "a" carga "b" y "d", pero no "c".
        let a = find_node_mut(&mut node.children, &root.path().join("a")).unwrap();
        a.load_children().unwrap();
        assert_eq!(a.children.len(), 2);
        let b = find_node_mut(&mut node.children, &root.path().join("a/b")).unwrap();
        assert_eq!(b.state, DirState::Closed);
        assert!(b.children.is_empty());
    }

    /// Regresión del fallo visto en macOS: el nodo guarda la ruta
    /// canónica (`/private/var/...`) pero la búsqueda llegaba con
    /// la ruta sin canonicalizar (`/var/...`). Buscar con
    /// componentes redundantes (`a/./../a`) debe encontrar el nodo.
    #[test]
    fn find_node_accepts_noncanonical_search_path() {
        let root = fixture_tree();
        let safe = SafePath::from_root(root.path()).unwrap();
        let mut node = DirNode::new(safe);
        node.load_children().unwrap();
        let odd = root.path().join("a/./../a");
        let found = find_node_mut(&mut node.children, &odd);
        assert!(found.is_some(), "buscar {odd:?} debe encontrar el nodo 'a'");
        assert_eq!(found.unwrap().name(), "a");
    }

    #[test]
    fn list_dir_paginates_250_files() {
        let root = TempDir::new().unwrap();
        for i in 0..250 {
            std::fs::write(root.path().join(format!("img-{i:03}.jpg")), b"x").unwrap();
        }
        let safe = SafePath::from_root(root.path()).unwrap();
        let filter = FileFilter::default();
        let p0 = list_dir(&safe, &filter, SortKey::Name, SortOrder::Asc, Page::first()).unwrap();
        assert_eq!(p0.items.len(), 200);
        assert_eq!(p0.total, 250);
        assert_eq!(p0.total_pages(), 2);
        let p1 = list_dir(&safe, &filter, SortKey::Name, SortOrder::Asc, Page::at(1)).unwrap();
        assert_eq!(p1.items.len(), 50);
        assert_eq!(p1.total, 250);
        // Sin solapamiento entre páginas.
        assert_ne!(p0.items[0].name, p1.items[0].name);
        // Orden alfabético en la primera página.
        assert_eq!(p0.items[0].name, "img-000.jpg");
    }

    #[test]
    fn list_dir_filters_hide_folders_and_only_media() {
        let root = fixture_tree();
        let a = SafePath::new(root.path(), &root.path().join("a")).unwrap();

        // Por defecto: sin carpetas, solo multimedia.
        let r = list_dir(
            &a,
            &FileFilter::default(),
            SortKey::Name,
            SortOrder::Asc,
            Page::first(),
        )
        .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].name, "foto.jpg");

        // Sin solo-multimedia: aparece el pdf.
        let f = FileFilter {
            only_media: false,
            ..Default::default()
        };
        let r = list_dir(&a, &f, SortKey::Name, SortOrder::Asc, Page::first()).unwrap();
        assert_eq!(r.total, 2);

        // Sin ocultar carpetas: aparecen b y d además de los archivos.
        let f = FileFilter {
            only_media: false,
            hide_folders: false,
            ..Default::default()
        };
        let r = list_dir(&a, &f, SortKey::Name, SortOrder::Asc, Page::first()).unwrap();
        assert_eq!(r.total, 4);
        assert!(r.items.iter().any(|e| e.is_dir && e.name == "b"));
    }

    #[test]
    fn list_dir_sorts_by_size_and_extension() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("a.jpg"), b"12").unwrap();
        std::fs::write(root.path().join("b.png"), b"12345").unwrap();
        std::fs::write(root.path().join("c.jpg"), b"1").unwrap();
        let safe = SafePath::from_root(root.path()).unwrap();
        let filter = FileFilter {
            only_media: false,
            ..Default::default()
        };
        let r = list_dir(
            &safe,
            &filter,
            SortKey::Size,
            SortOrder::Desc,
            Page::first(),
        )
        .unwrap();
        assert_eq!(r.items[0].name, "b.png");
        assert_eq!(r.items[2].name, "c.jpg");
        // name_without_ext / ext se separan bien.
        let jpg = r.items.iter().find(|e| e.name == "a.jpg").unwrap();
        assert_eq!(jpg.name_without_ext, "a");
        assert_eq!(jpg.ext, "jpg");
    }

    #[test]
    fn list_dir_search_filters_by_name() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("vacaciones.jpg"), b"x").unwrap();
        std::fs::write(root.path().join("trabajo.jpg"), b"x").unwrap();
        let safe = SafePath::from_root(root.path()).unwrap();
        let f = FileFilter {
            search: "vaca".into(),
            ..Default::default()
        };
        let r = list_dir(&safe, &f, SortKey::Name, SortOrder::Asc, Page::first()).unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].name, "vacaciones.jpg");
    }
}
