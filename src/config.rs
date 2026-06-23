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

/// Un recurso recordado: favorito (marcado con `*`) o reciente (auto-trackeado al
/// drillear). Agnóstico: `view_id`/`key`/`label` son strings opacos para el core (la
/// vista re-interpreta `key` en `on_context`; `label` es lo que se pinta en el menú).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Favorite {
    /// Id de la vista que lo creó (p. ej. `"logs"`).
    pub view_id: String,
    /// Identificador opaco del recurso (group / queue_url / arn / bus…).
    pub key: String,
    /// Rótulo para el menú.
    pub label: String,
    /// `true` = marcado con `*`; `false` = solo reciente (auto-trackeado).
    #[serde(default)]
    pub is_favorite: bool,
}

/// Tope de entradas RECIENTES (no-favoritas) que se conservan; los favoritos nunca
/// se podan.
const FAVORITES_CAP: usize = 50;

/// Estado que la app escribe sola: el último ambiente usado y los favoritos/recientes,
/// para recordarlos al reabrir. Vive en `state.toml` (aparte del `config.toml`
/// hand-editado). La recencia se modela por POSICIÓN en `favorites` (frente = más
/// reciente) → sin reloj.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    /// Último profile activo al salir.
    pub last_profile: Option<String>,
    /// Última región activa al salir.
    pub last_region: Option<String>,
    /// Favoritos (marcados) + recientes (auto), del más reciente al más viejo. El
    /// `#[serde(default)]` del struct hace que un `state.toml` v1 (sin esta clave)
    /// parsee a `Vec` vacío (compatibilidad hacia atrás).
    pub favorites: Vec<Favorite>,
}

impl State {
    /// Alterna el favorito del recurso `(view_id, key)`. Si no existe, lo inserta al
    /// frente como favorito; si existe, alterna su flag (y refresca el rótulo).
    /// Devuelve `true` si quedó marcado como favorito.
    pub fn toggle_favorite(&mut self, view_id: &str, key: &str, label: &str) -> bool {
        if let Some(f) = self
            .favorites
            .iter_mut()
            .find(|f| f.view_id == view_id && f.key == key)
        {
            f.is_favorite = !f.is_favorite;
            f.label = label.to_string();
            return f.is_favorite;
        }
        self.favorites.insert(
            0,
            Favorite {
                view_id: view_id.to_string(),
                key: key.to_string(),
                label: label.to_string(),
                is_favorite: true,
            },
        );
        true
    }

    /// Registra un acceso reciente a `(view_id, key)`: lo mueve al frente conservando su
    /// flag de favorito, y poda los recientes que excedan el tope.
    pub fn record_recent(&mut self, view_id: &str, key: &str, label: &str) {
        let existing = self
            .favorites
            .iter()
            .position(|f| f.view_id == view_id && f.key == key);
        let is_favorite = existing
            .map(|i| self.favorites[i].is_favorite)
            .unwrap_or(false);
        if let Some(i) = existing {
            self.favorites.remove(i);
        }
        self.favorites.insert(
            0,
            Favorite {
                view_id: view_id.to_string(),
                key: key.to_string(),
                label: label.to_string(),
                is_favorite,
            },
        );
        self.prune();
    }

    /// Conserva todos los favoritos y a lo sumo `FAVORITES_CAP` recientes (los más
    /// nuevos por orden; `retain` preserva el orden frente→atrás).
    fn prune(&mut self) {
        let mut recents = 0usize;
        self.favorites.retain(|f| {
            if f.is_favorite {
                return true;
            }
            recents += 1;
            recents <= FAVORITES_CAP
        });
    }
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
        let mut s = State {
            last_profile: Some("prod".to_string()),
            last_region: Some("eu-west-1".to_string()),
            favorites: vec![],
        };
        s.toggle_favorite("logs", "/aws/lambda/api", "/aws/lambda/api");
        s.record_recent("sqs", "https://sqs/q1", "q1");
        let toml = s.to_toml().expect("serializa");
        let back = State::parse(&toml).expect("re-parsea");
        assert_eq!(back.last_profile.as_deref(), Some("prod"));
        assert_eq!(back.last_region.as_deref(), Some("eu-west-1"));
        assert_eq!(
            back.favorites.len(),
            2,
            "favoritos sobreviven el round-trip"
        );
        assert!(
            back.favorites
                .iter()
                .any(|f| f.view_id == "logs" && f.is_favorite)
        );
        assert!(
            back.favorites
                .iter()
                .any(|f| f.view_id == "sqs" && !f.is_favorite)
        );
    }

    #[test]
    fn empty_state_parses_to_default() {
        let s = State::parse("").expect("vacío parsea a default");
        assert!(s.last_profile.is_none() && s.last_region.is_none());
        assert!(s.favorites.is_empty());
    }

    #[test]
    fn state_v1_without_favorites_key_parses() {
        // `state.toml` viejo (sin la clave `favorites`) sigue parseando (serde default).
        let s = State::parse("last_profile = \"prod\"\nlast_region = \"us-east-1\"")
            .expect("v1 parsea");
        assert_eq!(s.last_profile.as_deref(), Some("prod"));
        assert!(s.favorites.is_empty());
    }

    #[test]
    fn toggle_favorite_adds_then_flips() {
        let mut s = State::default();
        assert!(s.toggle_favorite("logs", "g", "g"), "no existía → favorito");
        assert_eq!(s.favorites.len(), 1);
        assert!(s.favorites[0].is_favorite);
        // Toggle de nuevo: deja de ser favorito pero la entrada permanece (como reciente).
        assert!(!s.toggle_favorite("logs", "g", "g"));
        assert_eq!(s.favorites.len(), 1);
        assert!(!s.favorites[0].is_favorite);
    }

    #[test]
    fn record_recent_moves_to_front_and_keeps_favorite_flag() {
        let mut s = State::default();
        s.toggle_favorite("logs", "a", "a"); // favorito
        s.record_recent("sqs", "b", "b"); // reciente, va al frente
        assert_eq!(s.favorites[0].key, "b");
        // Re-acceder a "a": vuelve al frente y conserva is_favorite=true.
        s.record_recent("logs", "a", "a");
        assert_eq!(s.favorites[0].key, "a");
        assert!(
            s.favorites[0].is_favorite,
            "el flag de favorito se conserva"
        );
        assert_eq!(s.favorites.len(), 2, "no duplica");
    }

    #[test]
    fn prune_keeps_favorites_and_caps_recents() {
        let mut s = State::default();
        // Un favorito + CAP+5 recientes.
        s.toggle_favorite("logs", "fav", "fav");
        for i in 0..(FAVORITES_CAP + 5) {
            s.record_recent("sqs", &format!("r{i}"), "r");
        }
        let recents = s.favorites.iter().filter(|f| !f.is_favorite).count();
        let favs = s.favorites.iter().filter(|f| f.is_favorite).count();
        assert_eq!(favs, 1, "el favorito nunca se poda");
        assert_eq!(recents, FAVORITES_CAP, "los recientes se topan en CAP");
    }
}
