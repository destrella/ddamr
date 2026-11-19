//! Test de integración de SafePath: el perímetro de seguridad.
//!
//! Es el primer test que se escribió del proyecto, antes que cualquier
//! ventana: si este test pasa, ninguna operación puede escapar de `~`
//! ni por `..` ni por symlink. Si falla, nada más importa.
use ddamr_core::{AppError, SafePath};
use std::os::unix::fs::symlink;
use tempfile::TempDir;

#[test]
fn symlink_inside_root_pointing_outside_is_rejected() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("real.txt");
    std::fs::write(&target, b"secreto").unwrap();
    symlink(&target, root.path().join("enlace.txt")).unwrap();

    let err = SafePath::new(root.path(), &root.path().join("enlace.txt")).unwrap_err();
    assert!(
        matches!(err, AppError::OutsideRoot(_)),
        "un symlink a fuera de la raíz debe rechazarse, se obtuvo: {err}"
    );
}

#[test]
fn dotdot_escape_is_rejected() {
    let root = TempDir::new().unwrap();
    let err = SafePath::new(root.path(), std::path::Path::new("../fuera.txt")).unwrap_err();
    assert!(matches!(err, AppError::OutsideRoot(_)));
}

#[test]
fn inside_paths_are_accepted_and_relative_is_stored() {
    let root = TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("fotos")).unwrap();
    let p = SafePath::new(root.path(), &root.path().join("fotos")).unwrap();
    assert_eq!(p.relative(), std::path::Path::new("fotos"));
}
