//! ddamr-app: solo iced (fase 1).
//!
//! Estado, mensajes, vistas y subscriptions a los jobs de
//! `ddamr-index`. Nada de SQL ni de `std::fs` directo en `update`:
//! todo lo pesado va en `Task::perform`.

pub mod app;
pub mod virtual_list;

pub use app::{App, Message, ViewMode};
