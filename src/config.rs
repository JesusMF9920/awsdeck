//! Config opcional en disco: `~/.config/awsdeck/config.toml` (respeta
//! `XDG_CONFIG_HOME`). **Load-only** por ahora: si no existe o no parsea, se usan los
//! defaults sin romper el arranque. Escribir la config queda como follow-up.
//!
//! Ejemplo:
//! ```toml
//! default_profile = "prod"
//! default_region = "eu-west-1"
//! default_tail_window = "6h"   # 15m · 1h · 6h · 24h · 3d · 7d
//! ```

use std::path::PathBuf;

use serde::Deserialize;

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
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("awsdeck").join("config.toml"))
    }
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
}
