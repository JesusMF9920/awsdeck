//! awsdeck — un TUI tipo k9s para los servicios de AWS que uso a diario.
//!
//! Composition root: arma el registry de vistas, registra las vistas concretas
//! (solo aquí se nombran), construye el `App` y corre el loop. Ver `ROADMAP.md`
//! y `CLAUDE.md` para la arquitectura.

mod action;
mod app;
mod aws;
mod config;
mod effects;
mod message;
mod tui;
mod ui;
mod util;
mod views;

use color_eyre::eyre::Result;
use tokio::sync::mpsc;

use crate::app::App;
use crate::aws::context::Env;
use crate::config::Config;
use crate::effects::Effects;
use crate::tui::Tui;
use crate::views::Registry;
use crate::views::events::EventsView;
use crate::views::logs::LogsView;
use crate::views::sfn::SfnView;
use crate::views::sqs::SqsView;

#[tokio::main]
async fn main() -> Result<()> {
    // Config opcional del disco (load-only); si no hay, defaults.
    let config = Config::load();
    let env = initial_env(&config);

    // Canal de resultados async: effects manda, App recibe en el select! loop.
    let (tx, rx) = mpsc::channel(64);
    // `AWSDECK_MOCK=1` usa datos falsos en memoria: demo/QA sin red ni credenciales.
    let effects = if std::env::var_os("AWSDECK_MOCK").is_some() {
        Effects::new_mock(tx, env.clone())
    } else {
        Effects::new(tx, env.clone())
    };

    // Registry de vistas. Aquí —y solo aquí, en el composition root— se nombran
    // las vistas concretas. Agregar un servicio = registrar una línea más.
    let mut registry = Registry::new();
    let logs = match config.default_tail_window.as_deref() {
        Some(w) => LogsView::new().with_default_window(w),
        None => LogsView::new(),
    };
    registry.register(Box::new(logs));
    registry.register(Box::new(SqsView::new()));
    registry.register(Box::new(SfnView::new()));
    registry.register(Box::new(EventsView::new()));

    let mut app = App::new(env, registry, effects, rx);

    // Si no fijaste AWS_PROFILE, al iniciar te dejamos elegir con qué profile
    // trabajar (lee ~/.aws/config). Con AWS_PROFILE seteada, arranca directo.
    if std::env::var_os("AWS_PROFILE").is_none() {
        app.start_with_env_picker();
    }

    // El guard de terminal vive hasta el final: restaura en Drop (también si
    // `run` devuelve Err) antes de que color-eyre imprima el reporte.
    let mut tui = Tui::init()?;
    let result = app.run(&mut tui.terminal).await;
    drop(tui);
    result
}

/// Ambiente inicial: entorno > config en disco > defaults sensatos.
fn initial_env(config: &Config) -> Env {
    let profile = std::env::var("AWS_PROFILE")
        .ok()
        .or_else(|| config.default_profile.clone())
        .unwrap_or_else(|| "default".to_string());
    let region = std::env::var("AWS_REGION")
        .ok()
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .or_else(|| config.default_region.clone())
        .unwrap_or_else(|| "us-east-1".to_string());
    Env::new(profile, region)
}
