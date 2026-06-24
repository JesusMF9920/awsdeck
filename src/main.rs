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
use crate::config::{Config, State};
use crate::effects::Effects;
use crate::tui::Tui;
use crate::views::Registry;
use crate::views::events::EventsView;
use crate::views::logs::LogsView;
use crate::views::sfn::SfnView;
use crate::views::sqs::SqsView;

#[tokio::main]
async fn main() -> Result<()> {
    // Config opcional del disco (load-only) + estado persistido (último ambiente).
    let config = Config::load();
    let state = State::load();
    let env = initial_env(&config, &state);

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
    // Inyecta los favoritos/recientes persistidos (`state.toml`); la app los muta en
    // memoria y los reescribe al salir.
    app.load_state(state);

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

/// Ambiente inicial leyendo el entorno; delega la precedencia a `pick_env` (puro).
fn initial_env(config: &Config, state: &State) -> Env {
    let env_region = std::env::var("AWS_REGION")
        .ok()
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok());
    pick_env(std::env::var("AWS_PROFILE").ok(), env_region, config, state)
}

/// Precedencia del ambiente inicial (pura, testeable): entorno > `default_*` del
/// `config.toml` (preferencia explícita) > último ambiente usado (`state.toml`) >
/// defaults sensatos.
fn pick_env(
    env_profile: Option<String>,
    env_region: Option<String>,
    config: &Config,
    state: &State,
) -> Env {
    let profile = env_profile
        .or_else(|| config.default_profile.clone())
        .or_else(|| state.last_profile.clone())
        .unwrap_or_else(|| "default".to_string());
    let region = env_region
        .or_else(|| config.default_region.clone())
        .or_else(|| state.last_region.clone())
        .unwrap_or_else(|| "us-east-1".to_string());
    Env::new(profile, region)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_env_precedence_env_over_config_over_state() {
        let config = Config {
            default_profile: Some("cfg-prof".into()),
            default_region: Some("cfg-region".into()),
            default_tail_window: None,
        };
        let state = State {
            last_profile: Some("state-prof".into()),
            last_region: Some("state-region".into()),
            ..State::default()
        };
        // El entorno gana sobre todo.
        let e = pick_env(
            Some("env-prof".into()),
            Some("env-region".into()),
            &config,
            &state,
        );
        assert_eq!(e, Env::new("env-prof", "env-region"));
        // Sin entorno: gana el default explícito del config.
        let e = pick_env(None, None, &config, &state);
        assert_eq!(e, Env::new("cfg-prof", "cfg-region"));
    }

    #[test]
    fn pick_env_falls_back_to_state_then_default() {
        let config = Config::default();
        let state = State {
            last_profile: Some("last".into()),
            last_region: None,
            ..State::default()
        };
        // Sin entorno ni config: recuerda el último profile; región cae al default.
        let e = pick_env(None, None, &config, &state);
        assert_eq!(e, Env::new("last", "us-east-1"));
    }
}
