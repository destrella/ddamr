//! ddamr-index: cola de jobs con prioridades y cancelación (fase 1).
//!
//! Prioridades: 0 UI (listar, miniatura visible), 1 metadatos de la
//! carpeta visible, 2 hash/dHash, 3 escaneo de fondo y duplicados.
//! Al cambiar de carpeta se cancelan los jobs >= 1 de la anterior.
//! Stub en fase 0.
