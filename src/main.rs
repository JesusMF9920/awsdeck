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

use color_eyre::eyre::Result;
use tokio::sync::mpsc;

use crate::app::App;
use crate::aws::context::Env;
use crate::effects::Effects;
use crate::tui::Tui;
use crate::views::Registry;

#[tokio::main]
async fn main() -> Result<()> {
    let env = initial_env();

    // Canal de resultados async: effects manda, App recibe en el select! loop.
    let (tx, rx) = mpsc::channel(64);
    let effects = Effects::new(tx, env.clone());

    // Registry de vistas. El commit "logs view" registra aquí la primera vista
    // concreta: `registry.register(Box::new(LogsView::new()));`. Es el único
    // punto donde se nombra un servicio en el arranque.
    let registry = Registry::new();

    let mut app = App::new(env, registry, effects, rx);

    // El guard de terminal vive hasta el final: restaura en Drop (también si
    // `run` devuelve Err) antes de que color-eyre imprima el reporte.
    let mut tui = Tui::init()?;
    let result = app.run(&mut tui.terminal).await;
    drop(tui);
    result
}

/// Ambiente inicial tomado del entorno, con defaults sensatos.
fn initial_env() -> Env {
    let profile = std::env::var("AWS_PROFILE").unwrap_or_else(|_| "default".to_string());
    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string());
    Env::new(profile, region)
}
