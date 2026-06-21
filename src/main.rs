//! awsdeck — un TUI tipo k9s para los servicios de AWS que uso a diario.
//!
//! Composition root: arma el registry de vistas, registra las vistas concretas
//! (solo aquí se nombran), construye el `App` y corre el loop. Ver `ROADMAP.md`
//! y `CLAUDE.md` para la arquitectura.

mod action;
mod app;
mod aws;
mod effects;
mod message;
mod tui;
mod ui;
mod views;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    // El cableado real (Tui guard + App loop + registro de vistas) llega en commits
    // posteriores. Por ahora el scaffold solo valida que todo el árbol de módulos
    // y las dependencias compilen.
    Ok(())
}
