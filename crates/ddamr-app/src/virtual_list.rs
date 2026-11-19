//! Lista paginada y caché de miniaturas (fase 1).
//!
//! # Por qué paginación y no virtualización por scroll todavía
//!
//! El `scrollable` de iced construye todos sus hijos: con 20.000
//! filas, cada cambio de estado reconstruye 20.000 widgets. La
//! solución real es un widget virtualizado que solo construya las
//! ~30 filas visibles según el offset de scroll, pero eso es un
//! widget custom con su propio layout y su propio estado de scroll:
//! trabajo de 1–2 días que no cabe en esta fase sin hacerlo mal.
//!
//! En fase 1 el compromiso es la paginación: el panel solo recibe
//! y solo renderiza la página actual (200 filas como máximo, ver
//! `Page::DEFAULT_LIMIT` en `ddamr-core`). El coste por frame es
//! constante y no depende del tamaño de la carpeta. El widget
//! virtualizado por scroll llegará en una iteración posterior y
//! reutilizará `list_dir`/`list_files_paginated` tal cual: el cambio
//! será solo de presentación, no del motor.

use ddamr_core::Page;
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

/// Caché LRU en memoria para miniaturas (bytes JPEG).
/// Capacidad acotada: cuando se llena, se expulsa la miniatura
/// que lleva más tiempo sin usarse. En fase 1 la UI aún no dibuja
/// miniaturas; la caché ya está aquí para que la iteración que las
/// dibuje no tenga que inventar el almacenamiento.
pub struct ThumbCache {
    capacity: usize,
    map: HashMap<PathBuf, Vec<u8>>,
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
    pub fn get(&mut self, path: &Path) -> Option<&[u8]> {
        if self.map.contains_key(path) {
            self.order.retain(|p| p != path);
            self.order.push_back(path.to_path_buf());
            self.map.get(path).map(Vec::as_slice)
        } else {
            None
        }
    }

    pub fn insert(&mut self, path: PathBuf, bytes: Vec<u8>) {
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
        self.order.push_back(path.clone());
        self.map.insert(path, bytes);
    }
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
    fn thumb_cache_evicts_least_recently_used() {
        let mut cache = ThumbCache::new(2);
        cache.insert(PathBuf::from("a"), vec![1]);
        cache.insert(PathBuf::from("b"), vec![2]);
        assert_eq!(cache.get(Path::new("a")), Some([1].as_slice()));
        cache.insert(PathBuf::from("c"), vec![3]);
        assert_eq!(cache.len(), 2);
        // "a" se usó hace poco, así que la expulsada es "b".
        assert!(cache.get(Path::new("b")).is_none());
        assert!(cache.get(Path::new("a")).is_some());
        assert!(cache.get(Path::new("c")).is_some());
    }
}
