//! Binario de la GUI. Sin display (como en esta VM) la ventana no
//! se puede abrir: eso es esperado. La lógica se verifica con
//! `cargo test -p ddamr-app` y `cargo check -p ddamr-app`.
use ddamr_app::App;

fn main() -> iced::Result {
    // `App::boot` (no `App::default`): al abrir, la raíz aparece
    // expandida y el panel ya muestra sus elementos.
    iced::application(App::boot, App::update, App::view)
        .title(|_app: &App| "DDAMR".to_string())
        .subscription(App::subscription)
        .run()
}
