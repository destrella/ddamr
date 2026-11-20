//! Repositorios de etiquetas, ubicaciones, regiones y asociaciones.
//!
//! Todo con sentencias preparadas y parámetros; el `ORDER BY`
//! sale siempre de un enum, nunca de un valor del usuario.
//! `usage_count` lo mantiene el repositorio (incremento en
//! `add_to_file`, decremento en `remove_from_file`), no se calcula
//! con `COUNT(*)` en cada render.

use ddamr_core::{AppError, Result};
use rusqlite::{Connection, params};

fn db_err(e: rusqlite::Error) -> AppError {
    AppError::Database(e.to_string())
}

/// Id de la fila `files` de una ruta relativa. Público para la
/// UI (regiones) y para los tests; el error distingue "no
/// indexado" de un fallo de DB.
pub fn file_id_for_path(conn: &Connection, rel_path: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM files WHERE path = ?1",
        params![rel_path],
        |r| r.get(0),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            AppError::Database(format!("archivo no indexado en la DB: {rel_path}"))
        }
        other => db_err(other),
    })
}

fn file_id(conn: &Connection, rel_path: &str) -> Result<i64> {
    file_id_for_path(conn, rel_path)
}

/// Redondea a 4 decimales (~11 m): la precisión para dedup de
/// ubicaciones. La precisión completa se guarda en `lat`/`lon`.
pub fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------- tags

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub usage_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSort {
    Alpha,
    AlphaRev,
    Usage,
    UsageRev,
}

pub struct TagRepo;

impl TagRepo {
    /// Crea la etiqueta o devuelve la existente (UNIQUE NOCASE).
    pub fn create(conn: &Connection, name: &str) -> Result<i64> {
        let name = name.trim();
        conn.execute(
            "INSERT OR IGNORE INTO tags (name) VALUES (?1)",
            params![name],
        )
        .map_err(db_err)?;
        conn.query_row("SELECT id FROM tags WHERE name = ?1", params![name], |r| {
            r.get(0)
        })
        .map_err(db_err)
    }

    pub fn list_sorted(conn: &Connection, sort: TagSort) -> Result<Vec<Tag>> {
        let order = match sort {
            TagSort::Alpha => "name ASC",
            TagSort::AlphaRev => "name DESC",
            TagSort::Usage => "usage_count DESC, name ASC",
            TagSort::UsageRev => "usage_count ASC, name ASC",
        };
        let sql = format!("SELECT id, name, usage_count FROM tags ORDER BY {order}");
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Tag {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    usage_count: r.get(2)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }

    /// Etiquetas asociadas a un archivo, por nombre.
    pub fn list_for_file(conn: &Connection, rel_path: &str) -> Result<Vec<Tag>> {
        let fid = file_id(conn, rel_path)?;
        let mut stmt = conn
            .prepare(
                "SELECT tags.id, tags.name, tags.usage_count FROM tags
                 JOIN file_tags ON file_tags.tag_id = tags.id
                 WHERE file_tags.file_id = ?1 ORDER BY tags.name ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![fid], |r| {
                Ok(Tag {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    usage_count: r.get(2)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }

    /// Asocia una etiqueta a un archivo y sube su contador (solo
    /// si la asociación no existía ya).
    pub fn add_to_file(conn: &Connection, rel_path: &str, tag_id: i64) -> Result<()> {
        let fid = file_id(conn, rel_path)?;
        let n = conn
            .execute(
                "INSERT OR IGNORE INTO file_tags (file_id, tag_id) VALUES (?1, ?2)",
                params![fid, tag_id],
            )
            .map_err(db_err)?;
        if n > 0 {
            conn.execute(
                "UPDATE tags SET usage_count = usage_count + 1 WHERE id = ?1",
                params![tag_id],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }

    pub fn remove_from_file(conn: &Connection, rel_path: &str, tag_id: i64) -> Result<()> {
        let fid = file_id(conn, rel_path)?;
        let n = conn
            .execute(
                "DELETE FROM file_tags WHERE file_id = ?1 AND tag_id = ?2",
                params![fid, tag_id],
            )
            .map_err(db_err)?;
        if n > 0 {
            conn.execute(
                "UPDATE tags SET usage_count = MAX(0, usage_count - 1) WHERE id = ?1",
                params![tag_id],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }

    /// Borra las etiquetas sin ningún archivo asociado. Devuelve
    /// cuántas se borraron.
    pub fn cleanup_orphans(conn: &Connection) -> Result<usize> {
        let n = conn
            .execute(
                "DELETE FROM tags WHERE NOT EXISTS (
                    SELECT 1 FROM file_tags WHERE file_tags.tag_id = tags.id
                )",
                [],
            )
            .map_err(db_err)?;
        Ok(n)
    }
}

// ------------------------------------------------------------ locations

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub id: i64,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub lat_r: f64,
    pub lon_r: f64,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub usage_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationSort {
    Alpha,
    AlphaRev,
    Usage,
    UsageRev,
}

pub struct LocationRepo;

impl LocationRepo {
    /// Crea una ubicación. Si ya existe una con las mismas
    /// coordenadas redondeadas (UNIQUE lat_r/lon_r), devuelve la
    /// existente: es el dedup de "la misma zona general".
    pub fn create(
        conn: &Connection,
        name: &str,
        lat: f64,
        lon: f64,
        city: Option<&str>,
        state: Option<&str>,
        country: Option<&str>,
    ) -> Result<i64> {
        let (lat_r, lon_r) = (round4(lat), round4(lon));
        conn.execute(
            "INSERT OR IGNORE INTO locations
             (name, lat, lon, lat_r, lon_r, city, state, country)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![name.trim(), lat, lon, lat_r, lon_r, city, state, country],
        )
        .map_err(db_err)?;
        conn.query_row(
            "SELECT id FROM locations WHERE lat_r = ?1 AND lon_r = ?2",
            params![lat_r, lon_r],
            |r| r.get(0),
        )
        .map_err(db_err)
    }

    /// Ubicaciones ya guardadas en la misma zona (coordenadas
    /// redondeadas iguales).
    pub fn find_nearby(conn: &Connection, lat: f64, lon: f64) -> Result<Vec<Location>> {
        let (lat_r, lon_r) = (round4(lat), round4(lon));
        let mut stmt = conn
            .prepare(
                "SELECT id, name, lat, lon, lat_r, lon_r, city, state, country, usage_count
                 FROM locations WHERE lat_r = ?1 AND lon_r = ?2",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![lat_r, lon_r], |r| {
                Ok(Location {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    lat: r.get(2)?,
                    lon: r.get(3)?,
                    lat_r: r.get(4)?,
                    lon_r: r.get(5)?,
                    city: r.get(6)?,
                    state: r.get(7)?,
                    country: r.get(8)?,
                    usage_count: r.get(9)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }

    pub fn list_sorted(conn: &Connection, sort: LocationSort) -> Result<Vec<Location>> {
        let order = match sort {
            LocationSort::Alpha => "name ASC",
            LocationSort::AlphaRev => "name DESC",
            LocationSort::Usage => "usage_count DESC, name ASC",
            LocationSort::UsageRev => "usage_count ASC, name ASC",
        };
        let sql = format!(
            "SELECT id, name, lat, lon, lat_r, lon_r, city, state, country, usage_count
             FROM locations ORDER BY {order}"
        );
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Location {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    lat: r.get(2)?,
                    lon: r.get(3)?,
                    lat_r: r.get(4)?,
                    lon_r: r.get(5)?,
                    city: r.get(6)?,
                    state: r.get(7)?,
                    country: r.get(8)?,
                    usage_count: r.get(9)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }

    pub fn set_file_location(conn: &Connection, rel_path: &str, location_id: i64) -> Result<()> {
        let fid = file_id(conn, rel_path)?;
        conn.execute(
            "INSERT INTO file_locations (file_id, location_id) VALUES (?1, ?2)
             ON CONFLICT(file_id) DO UPDATE SET location_id = excluded.location_id",
            params![fid, location_id],
        )
        .map_err(db_err)?;
        conn.execute(
            "UPDATE locations SET usage_count = usage_count + 1 WHERE id = ?1",
            params![location_id],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Ubicación asignada a un archivo, si la tiene.
    pub fn file_location(conn: &Connection, rel_path: &str) -> Result<Option<Location>> {
        let fid = file_id(conn, rel_path)?;
        match conn.query_row(
            "SELECT locations.id, locations.name, locations.lat, locations.lon,
                    locations.lat_r, locations.lon_r, locations.city,
                    locations.state, locations.country, locations.usage_count
             FROM locations
             JOIN file_locations ON file_locations.location_id = locations.id
             WHERE file_locations.file_id = ?1",
            params![fid],
            |r| {
                Ok(Location {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    lat: r.get(2)?,
                    lon: r.get(3)?,
                    lat_r: r.get(4)?,
                    lon_r: r.get(5)?,
                    city: r.get(6)?,
                    state: r.get(7)?,
                    country: r.get(8)?,
                    usage_count: r.get(9)?,
                })
            },
        ) {
            Ok(loc) => Ok(Some(loc)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(db_err(e)),
        }
    }

    /// Quita la ubicación de un archivo y baja el contador de uso
    /// de la ubicación anterior. No borra la ubicación en sí.
    pub fn remove_file_location(conn: &Connection, rel_path: &str) -> Result<()> {
        let fid = file_id(conn, rel_path)?;
        let old: Option<i64> = conn
            .query_row(
                "SELECT location_id FROM file_locations WHERE file_id = ?1",
                params![fid],
                |r| r.get(0),
            )
            .ok();
        conn.execute(
            "DELETE FROM file_locations WHERE file_id = ?1",
            params![fid],
        )
        .map_err(db_err)?;
        if let Some(id) = old {
            conn.execute(
                "UPDATE locations SET usage_count = MAX(0, usage_count - 1) WHERE id = ?1",
                params![id],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }
}

// -------------------------------------------------------------- regions

#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub id: i64,
    pub file_id: i64,
    pub name: String,
    /// Coordenadas normalizadas 0.0–1.0, nunca en píxeles.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

pub struct RegionRepo;

impl RegionRepo {
    pub fn add(
        conn: &Connection,
        file_id: i64,
        name: &str,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Result<i64> {
        for (label, v) in [("x", x), ("y", y), ("w", w), ("h", h)] {
            if !(0.0..=1.0).contains(&v) {
                return Err(AppError::Other(format!(
                    "la región {label}={v} está fuera de 0..1"
                )));
            }
        }
        // La región no puede desbordar la imagen: x+w e y+h ≤ 1.
        if x + w > 1.0 || y + h > 1.0 {
            return Err(AppError::Other(format!(
                "la región ({x}, {y}, {w}, {h}) desborda la imagen"
            )));
        }
        conn.execute(
            "INSERT OR IGNORE INTO regions (file_id, name, x, y, w, h)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![file_id, name.trim(), x, y, w, h],
        )
        .map_err(db_err)?;
        conn.query_row(
            "SELECT id FROM regions WHERE file_id = ?1 AND name = ?2 AND x = ?3 AND y = ?4 AND w = ?5 AND h = ?6",
            params![file_id, name.trim(), x, y, w, h],
            |r| r.get(0),
        )
        .map_err(db_err)
    }

    /// Borra una región por id. Devuelve `true` si existía.
    pub fn remove(conn: &Connection, id: i64) -> Result<bool> {
        let n = conn
            .execute("DELETE FROM regions WHERE id = ?1", params![id])
            .map_err(db_err)?;
        Ok(n > 0)
    }

    pub fn list_for_file(conn: &Connection, file_id: i64) -> Result<Vec<Region>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, file_id, name, x, y, w, h FROM regions
                 WHERE file_id = ?1 ORDER BY name ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![file_id], |r| {
                Ok(Region {
                    id: r.get(0)?,
                    file_id: r.get(1)?,
                    name: r.get(2)?,
                    x: r.get(3)?,
                    y: r.get(4)?,
                    w: r.get(5)?,
                    h: r.get(6)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }
}

// ---------------------------------------------------------- associations

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Association {
    pub id: i64,
    pub pattern: String,
    pub suggestions: String,
}

pub struct AssociationRepo;

impl AssociationRepo {
    pub fn create(conn: &Connection, pattern: &str, suggestions: &str) -> Result<i64> {
        conn.execute(
            "INSERT INTO associations (pattern, suggestions) VALUES (?1, ?2)",
            params![pattern.trim(), suggestions.trim()],
        )
        .map_err(db_err)?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list(conn: &Connection) -> Result<Vec<Association>> {
        let mut stmt = conn
            .prepare("SELECT id, pattern, suggestions FROM associations ORDER BY pattern ASC")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Association {
                    id: r.get(0)?,
                    pattern: r.get(1)?,
                    suggestions: r.get(2)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{insert_files_batch, open};
    use ddamr_fs::FileEntry;
    use tempfile::TempDir;

    fn db_with_file() -> (TempDir, Connection, String) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let mut conn = open(&db_path).unwrap();
        let p = dir.path().join("foto.jpg");
        std::fs::write(&p, b"x").unwrap();
        let found = std::fs::read_dir(dir.path())
            .unwrap()
            .find(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.file_name().to_string_lossy() == "foto.jpg")
            })
            .unwrap()
            .unwrap();
        let entry = FileEntry::from_dir_entry(dir.path(), &found).unwrap();
        insert_files_batch(&mut conn, &[entry]).unwrap();
        (dir, conn, "foto.jpg".into())
    }

    #[test]
    fn migration_002_creates_tables_and_version_is_2() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("t.db")).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        for table in [
            "file_tags",
            "locations",
            "regions",
            "file_locations",
            "associations",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "falta la tabla {table}");
        }
        // Las columnas nuevas existen en files.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(files)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        for c in [
            "dhash_image",
            "dhash_video",
            "width",
            "height",
            "duration_ms",
        ] {
            assert!(cols.iter().any(|x| x == c), "falta la columna {c}");
        }
    }

    #[test]
    fn locations_dedup_by_rounded_coords() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("t.db")).unwrap();
        // Dos puntos a ~2 m: mismo redondeo a 4 decimales.
        let a = LocationRepo::create(&conn, "Playa A", 41.387_400, 2.168_900, None, None, None)
            .unwrap();
        let b = LocationRepo::create(&conn, "Playa B", 41.387_418, 2.168_915, None, None, None)
            .unwrap();
        assert_eq!(a, b, "a 2 m deben colisionar en UNIQUE(lat_r, lon_r)");
        // Un punto lejos es otra ubicación.
        let c = LocationRepo::create(&conn, "Montaña", 42.0, 3.0, None, None, None).unwrap();
        assert_ne!(c, a);
        assert_eq!(
            LocationRepo::find_nearby(&conn, 41.387_41, 2.168_91)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            LocationRepo::list_sorted(&conn, LocationSort::Alpha)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn tags_usage_count_and_cleanup_orphans() {
        let (_dir, conn, rel) = db_with_file();
        let tag = TagRepo::create(&conn, "Playa").unwrap();
        assert_eq!(
            TagRepo::create(&conn, "playa").unwrap(),
            tag,
            "NOCASE dedup"
        );
        TagRepo::add_to_file(&conn, &rel, tag).unwrap();
        TagRepo::add_to_file(&conn, &rel, tag).unwrap(); // no duplica
        let tags = TagRepo::list_sorted(&conn, TagSort::Usage).unwrap();
        assert_eq!(tags[0].usage_count, 1);
        TagRepo::remove_from_file(&conn, &rel, tag).unwrap();
        assert_eq!(TagRepo::cleanup_orphans(&conn).unwrap(), 1);
        assert!(
            TagRepo::list_sorted(&conn, TagSort::Alpha)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn regions_validate_range_and_dedup() {
        let (_dir, conn, _rel) = db_with_file();
        let fid: i64 = conn
            .query_row("SELECT id FROM files WHERE path='foto.jpg'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(RegionRepo::add(&conn, fid, "Ana", 0.1, 0.1, 0.2, 0.3).is_ok());
        assert!(RegionRepo::add(&conn, fid, "Ana", 0.1, 0.1, 0.2, 0.3).is_ok()); // dedup
        assert_eq!(RegionRepo::list_for_file(&conn, fid).unwrap().len(), 1);
        assert!(RegionRepo::add(&conn, fid, "Fuera", 0.9, 0.1, 0.5, 0.3).is_err());
    }

    #[test]
    fn tags_list_for_file_and_location_assign_remove() {
        let (_dir, conn, rel) = db_with_file();
        // Etiquetas: añadir dos, listar, quitar una.
        let a = TagRepo::create(&conn, "Playa").unwrap();
        let b = TagRepo::create(&conn, "Sol").unwrap();
        TagRepo::add_to_file(&conn, &rel, a).unwrap();
        TagRepo::add_to_file(&conn, &rel, b).unwrap();
        let tags = TagRepo::list_for_file(&conn, &rel).unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name, "Playa");
        TagRepo::remove_from_file(&conn, &rel, a).unwrap();
        assert_eq!(TagRepo::list_for_file(&conn, &rel).unwrap().len(), 1);
        // Ubicación: sin asignar, asignar, quitar.
        assert!(LocationRepo::file_location(&conn, &rel).unwrap().is_none());
        let loc = LocationRepo::create(
            &conn,
            "Playa A",
            41.387_4,
            2.168_9,
            Some("Barcelona"),
            None,
            Some("España"),
        )
        .unwrap();
        LocationRepo::set_file_location(&conn, &rel, loc).unwrap();
        let got = LocationRepo::file_location(&conn, &rel).unwrap().unwrap();
        assert_eq!(got.name, "Playa A");
        assert_eq!(got.city.as_deref(), Some("Barcelona"));
        assert_eq!(got.usage_count, 1);
        LocationRepo::remove_file_location(&conn, &rel).unwrap();
        assert!(LocationRepo::file_location(&conn, &rel).unwrap().is_none());
        let after = LocationRepo::list_sorted(&conn, LocationSort::Alpha).unwrap();
        assert_eq!(after[0].usage_count, 0);
    }

    #[test]
    fn regions_add_list_and_remove() {
        let (_dir, conn, _rel) = db_with_file();
        let fid: i64 = conn
            .query_row("SELECT id FROM files WHERE path='foto.jpg'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let id = RegionRepo::add(&conn, fid, "Ana", 0.1, 0.1, 0.2, 0.3).unwrap();
        assert_eq!(RegionRepo::list_for_file(&conn, fid).unwrap().len(), 1);
        assert!(RegionRepo::remove(&conn, id).unwrap());
        assert!(RegionRepo::list_for_file(&conn, fid).unwrap().is_empty());
        assert!(!RegionRepo::remove(&conn, id).unwrap(), "dos veces no");
    }

    #[test]
    fn associations_crud() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("t.db")).unwrap();
        AssociationRepo::create(&conn, "playa,mar", "vacaciones,verano").unwrap();
        let list = AssociationRepo::list(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pattern, "playa,mar");
    }
}
