//! Config en disco (respeta `XDG_CONFIG_HOME`):
//! - `~/.config/awsdeck/config.toml` — **load-only**, hand-editado por el usuario; si no
//!   existe o no parsea, defaults sin romper el arranque.
//! - `~/.config/awsdeck/state.toml` — **estado** que la app escribe sola (último ambiente
//!   usado). Archivo aparte para NO clobberear los comentarios del `config.toml`.
//!
//! Ejemplo de `config.toml`:
//! ```toml
//! default_profile = "prod"
//! default_region = "eu-west-1"
//! default_tail_window = "6h"   # 15m · 1h · 6h · 24h · 3d · 7d
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Profile por defecto si no hay `AWS_PROFILE` en el entorno.
    pub default_profile: Option<String>,
    /// Región por defecto si no hay `AWS_REGION`/`AWS_DEFAULT_REGION`.
    pub default_region: Option<String>,
    /// Ventana de tiempo por defecto del tail de `logs` (etiqueta de preset).
    pub default_tail_window: Option<String>,
}

impl Config {
    /// Carga la config del disco; si no existe o no parsea, devuelve la default.
    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| Self::parse(&s))
            .unwrap_or_default()
    }

    /// Parsea TOML desde un string. `None` si no parsea. Testeable sin tocar disco.
    fn parse(s: &str) -> Option<Self> {
        toml::from_str(s).ok()
    }

    /// `~/.config/awsdeck/config.toml` (o `$XDG_CONFIG_HOME/awsdeck/config.toml`).
    fn path() -> Option<PathBuf> {
        config_file("config.toml")
    }
}

/// Estado que la app escribe sola: el último ambiente usado, para recordarlo al
/// reabrir. Vive en `state.toml` (aparte del `config.toml` hand-editado).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    /// Último profile activo al salir.
    pub last_profile: Option<String>,
    /// Última región activa al salir.
    pub last_region: Option<String>,
}

impl State {
    /// Carga el estado del disco; si no existe o no parsea, devuelve el default.
    pub fn load() -> Self {
        config_file("state.toml")
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| Self::parse(&s))
            .unwrap_or_default()
    }

    /// Escribe el estado al disco (best-effort: crea el directorio e ignora errores;
    /// no debe romper la salida de la app si el disco no está disponible).
    pub fn save(&self) {
        let Some(path) = config_file("state.toml") else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Some(s) = self.to_toml() {
            let _ = std::fs::write(path, s);
        }
    }

    fn parse(s: &str) -> Option<Self> {
        toml::from_str(s).ok()
    }

    fn to_toml(&self) -> Option<String> {
        toml::to_string(self).ok()
    }
}

/// `$XDG_CONFIG_HOME/awsdeck/<name>` (o `~/.config/awsdeck/<name>`).
fn config_file(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("awsdeck").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let cfg = Config::parse(
            r#"
            default_profile = "prod"
            default_region = "eu-west-1"
            default_tail_window = "6h"
        "#,
        )
        .expect("debe parsear");
        assert_eq!(cfg.default_profile.as_deref(), Some("prod"));
        assert_eq!(cfg.default_region.as_deref(), Some("eu-west-1"));
        assert_eq!(cfg.default_tail_window.as_deref(), Some("6h"));
    }

    #[test]
    fn empty_or_partial_config_is_fine() {
        let cfg = Config::parse("").expect("vacío parsea a default");
        assert!(cfg.default_profile.is_none() && cfg.default_region.is_none());

        let cfg = Config::parse(r#"default_region = "us-west-2""#).expect("parcial parsea");
        assert_eq!(cfg.default_region.as_deref(), Some("us-west-2"));
        assert!(cfg.default_profile.is_none());
    }

    #[test]
    fn invalid_toml_returns_none() {
        assert!(Config::parse("default_region = =bad").is_none());
    }

    #[test]
    fn state_round_trips_through_toml() {
        let s = State {
            last_profile: Some("prod".to_string()),
            last_region: Some("eu-west-1".to_string()),
        };
        let toml = s.to_toml().expect("serializa");
        let back = State::parse(&toml).expect("re-parsea");
        assert_eq!(back.last_profile.as_deref(), Some("prod"));
        assert_eq!(back.last_region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn empty_state_parses_to_default() {
        let s = State::parse("").expect("vacío parsea a default");
        assert!(s.last_profile.is_none() && s.last_region.is_none());
    }
}
