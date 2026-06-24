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

/// Historial de UN ambiente concreto (clave = `profile` + `region`): sus favoritos y
/// recientes, AISLADOS de los demás ambientes. Cada ambiente ve recursos distintos, así
/// que su historial es propio. La recencia es posicional dentro del bucket (frente = más
/// reciente) y el prune se aplica por bucket.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EnvHistory {
    /// Profile del ambiente.
    pub profile: String,
    /// Región del ambiente.
    pub region: String,
    /// Favoritos (marcados) + recientes (auto) de ESTE ambiente, del más reciente al más
    /// viejo.
    #[serde(default)]
    pub favorites: Vec<Favorite>,
}

/// Estado que la app escribe sola: el último ambiente usado y el historial de
/// favoritos/recientes POR AMBIENTE, para recordarlos al reabrir. Vive en `state.toml`
/// (aparte del `config.toml` hand-editado).
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    /// Último profile activo al salir.
    pub last_profile: Option<String>,
    /// Última región activa al salir.
    pub last_region: Option<String>,
    /// Historial por ambiente, uno por `(profile, región)`. Array-of-tables en TOML
    /// (`[[environments]]`). El `#[serde(default)]` del struct hace que un `state.toml`
    /// sin esta clave parsee a `Vec` vacío (compatibilidad hacia atrás).
    pub environments: Vec<EnvHistory>,
    /// Shim de migración: acepta el listado PLANO de `state.toml` v2 (clave `favorites`)
    /// al leer, lo pliega en `environments` (`migrate_legacy`) y NUNCA lo reescribe
    /// (`skip_serializing` → el archivo se actualiza al esquema por-ambiente al salir).
    /// No es parte de la API lógica de `State` (solo migración); `pub(crate)` para que la
    /// sintaxis `..State::default()` funcione desde otros módulos del crate.
    #[serde(default, rename = "favorites", skip_serializing)]
    pub(crate) legacy_favorites: Vec<Favorite>,
}

impl State {
    /// Bucket del ambiente `(profile, region)`, creándolo al final del `Vec` si no
    /// existe. Privado: solo lo usan los mutadores (crear-al-primer-escribir es correcto).
    fn bucket_mut(&mut self, profile: &str, region: &str) -> &mut EnvHistory {
        if let Some(i) = self
            .environments
            .iter()
            .position(|e| e.profile == profile && e.region == region)
        {
            return &mut self.environments[i];
        }
        self.environments.push(EnvHistory {
            profile: profile.to_string(),
            region: region.to_string(),
            favorites: Vec::new(),
        });
        self.environments.last_mut().unwrap()
    }

    /// Favoritos/recientes del ambiente `(profile, region)` (vacío si no hay bucket).
    /// Solo-lectura: NO crea bucket (se usa en el path de render del menú).
    pub fn favorites_for(&self, profile: &str, region: &str) -> &[Favorite] {
        self.environments
            .iter()
            .find(|e| e.profile == profile && e.region == region)
            .map(|e| e.favorites.as_slice())
            .unwrap_or(&[])
    }

    /// Alterna el favorito del recurso `(view_id, key)` DENTRO del ambiente dado. Si no
    /// existe, lo inserta al frente como favorito; si existe, alterna su flag (y refresca
    /// el rótulo). Devuelve `true` si quedó marcado como favorito.
    pub fn toggle_favorite(
        &mut self,
        profile: &str,
        region: &str,
        view_id: &str,
        key: &str,
        label: &str,
    ) -> bool {
        let bucket = self.bucket_mut(profile, region);
        if let Some(f) = bucket
            .favorites
            .iter_mut()
            .find(|f| f.view_id == view_id && f.key == key)
        {
            f.is_favorite = !f.is_favorite;
            f.label = label.to_string();
            return f.is_favorite;
        }
        bucket.favorites.insert(
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

    /// Registra un acceso reciente a `(view_id, key)` en el ambiente dado: lo mueve al
    /// frente conservando su flag de favorito, y poda los recientes del bucket que
    /// excedan el tope.
    pub fn record_recent(
        &mut self,
        profile: &str,
        region: &str,
        view_id: &str,
        key: &str,
        label: &str,
    ) {
        let bucket = self.bucket_mut(profile, region);
        let existing = bucket
            .favorites
            .iter()
            .position(|f| f.view_id == view_id && f.key == key);
        let is_favorite = existing
            .map(|i| bucket.favorites[i].is_favorite)
            .unwrap_or(false);
        if let Some(i) = existing {
            bucket.favorites.remove(i);
        }
        bucket.favorites.insert(
            0,
            Favorite {
                view_id: view_id.to_string(),
                key: key.to_string(),
                label: label.to_string(),
                is_favorite,
            },
        );
        Self::prune(&mut bucket.favorites);
    }

    /// Pliega el listado plano legacy (`state.toml` v2) en el bucket del último ambiente
    /// usado, preservando el orden (recencia). No-op si no hay legacy; tras correr,
    /// `legacy_favorites` queda vacío y no se reescribe (`skip_serializing`).
    fn migrate_legacy(&mut self) {
        if self.legacy_favorites.is_empty() {
            return;
        }
        let profile = self
            .last_profile
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let region = self
            .last_region
            .clone()
            .unwrap_or_else(|| "us-east-1".to_string());
        let legacy = std::mem::take(&mut self.legacy_favorites);
        let bucket = self.bucket_mut(&profile, &region);
        // Prepende en orden inverso para conservar la recencia original (frente = nuevo).
        for f in legacy.into_iter().rev() {
            bucket.favorites.insert(0, f);
        }
        Self::prune(&mut bucket.favorites);
    }

    /// Conserva todos los favoritos y a lo sumo `FAVORITES_CAP` recientes del bucket (los
    /// más nuevos por orden; `retain` preserva el orden frente→atrás).
    fn prune(favorites: &mut Vec<Favorite>) {
        let mut recents = 0usize;
        favorites.retain(|f| {
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
        let mut state: Self = toml::from_str(s).ok()?;
        state.migrate_legacy();
        Some(state)
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
            ..State::default()
        };
        s.toggle_favorite(
            "prod",
            "eu-west-1",
            "logs",
            "/aws/lambda/api",
            "/aws/lambda/api",
        );
        s.record_recent("prod", "eu-west-1", "sqs", "https://sqs/q1", "q1");
        let toml = s.to_toml().expect("serializa");
        let back = State::parse(&toml).expect("re-parsea");
        assert_eq!(back.last_profile.as_deref(), Some("prod"));
        assert_eq!(back.last_region.as_deref(), Some("eu-west-1"));
        let favs = back.favorites_for("prod", "eu-west-1");
        assert_eq!(favs.len(), 2, "favoritos sobreviven el round-trip");
        assert!(favs.iter().any(|f| f.view_id == "logs" && f.is_favorite));
        assert!(favs.iter().any(|f| f.view_id == "sqs" && !f.is_favorite));
    }

    #[test]
    fn empty_state_parses_to_default() {
        let s = State::parse("").expect("vacío parsea a default");
        assert!(s.last_profile.is_none() && s.last_region.is_none());
        assert!(s.environments.is_empty());
    }

    #[test]
    fn state_v1_without_favorites_key_parses() {
        // `state.toml` viejo (sin historial) sigue parseando (serde default).
        let s = State::parse("last_profile = \"prod\"\nlast_region = \"us-east-1\"")
            .expect("v1 parsea");
        assert_eq!(s.last_profile.as_deref(), Some("prod"));
        assert!(s.environments.is_empty());
    }

    #[test]
    fn toggle_favorite_adds_then_flips() {
        let mut s = State::default();
        assert!(
            s.toggle_favorite("p", "r", "logs", "g", "g"),
            "no existía → favorito"
        );
        assert_eq!(s.favorites_for("p", "r").len(), 1);
        assert!(s.favorites_for("p", "r")[0].is_favorite);
        // Toggle de nuevo: deja de ser favorito pero la entrada permanece (como reciente).
        assert!(!s.toggle_favorite("p", "r", "logs", "g", "g"));
        assert_eq!(s.favorites_for("p", "r").len(), 1);
        assert!(!s.favorites_for("p", "r")[0].is_favorite);
    }

    #[test]
    fn record_recent_moves_to_front_and_keeps_favorite_flag() {
        let mut s = State::default();
        s.toggle_favorite("p", "r", "logs", "a", "a"); // favorito
        s.record_recent("p", "r", "sqs", "b", "b"); // reciente, va al frente
        assert_eq!(s.favorites_for("p", "r")[0].key, "b");
        // Re-acceder a "a": vuelve al frente y conserva is_favorite=true.
        s.record_recent("p", "r", "logs", "a", "a");
        let favs = s.favorites_for("p", "r");
        assert_eq!(favs[0].key, "a");
        assert!(favs[0].is_favorite, "el flag de favorito se conserva");
        assert_eq!(favs.len(), 2, "no duplica");
    }

    #[test]
    fn prune_keeps_favorites_and_caps_recents() {
        let mut s = State::default();
        // Un favorito + CAP+5 recientes en el MISMO ambiente.
        s.toggle_favorite("p", "r", "logs", "fav", "fav");
        for i in 0..(FAVORITES_CAP + 5) {
            s.record_recent("p", "r", "sqs", &format!("r{i}"), "r");
        }
        let favs = s.favorites_for("p", "r");
        let recents = favs.iter().filter(|f| !f.is_favorite).count();
        let favcount = favs.iter().filter(|f| f.is_favorite).count();
        assert_eq!(favcount, 1, "el favorito nunca se poda");
        assert_eq!(recents, FAVORITES_CAP, "los recientes se topan en CAP");
    }

    #[test]
    fn favorites_are_isolated_per_env() {
        let mut s = State::default();
        s.toggle_favorite("prod", "eu-west-1", "logs", "ga", "ga");
        s.record_recent("dev", "us-east-1", "sqs", "qb", "qb");
        let a = s.favorites_for("prod", "eu-west-1");
        let b = s.favorites_for("dev", "us-east-1");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].key, "ga");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].key, "qb");
        // El ambiente A no ve el recurso de B (historiales aislados).
        assert!(a.iter().all(|f| f.key != "qb"));
        assert!(b.iter().all(|f| f.key != "ga"));
    }

    #[test]
    fn prune_is_per_env_not_global() {
        let mut s = State::default();
        for i in 0..(FAVORITES_CAP + 5) {
            s.record_recent("a", "r", "sqs", &format!("a{i}"), "a");
        }
        for i in 0..3 {
            s.record_recent("b", "r", "sqs", &format!("b{i}"), "b");
        }
        assert_eq!(
            s.favorites_for("a", "r").len(),
            FAVORITES_CAP,
            "A se topa dentro de su bucket"
        );
        assert_eq!(
            s.favorites_for("b", "r").len(),
            3,
            "B conserva los suyos (el cap no es global)"
        );
    }

    #[test]
    fn favorites_for_does_not_create_bucket() {
        let s = State::default();
        assert!(s.favorites_for("p", "r").is_empty());
        assert!(s.environments.is_empty(), "leer no crea bucket");
    }

    #[test]
    fn legacy_flat_favorites_migrate_into_last_env_bucket() {
        // `state.toml` v2: listado PLANO `favorites` + último ambiente.
        let v2 = r#"
            last_profile = "prod"
            last_region = "eu-west-1"

            [[favorites]]
            view_id = "logs"
            key = "g1"
            label = "g1"
            is_favorite = true

            [[favorites]]
            view_id = "sqs"
            key = "q1"
            label = "q1"
            is_favorite = false
        "#;
        let s = State::parse(v2).expect("v2 parsea y migra");
        // Las entradas viven en el bucket del último ambiente, en orden.
        let favs = s.favorites_for("prod", "eu-west-1");
        assert_eq!(favs.len(), 2);
        assert_eq!(favs[0].key, "g1");
        assert_eq!(favs[1].key, "q1");
        // Re-serializar emite `[[environments]]` y NO un `[[favorites]]` top-level.
        let out = s.to_toml().expect("serializa");
        assert!(
            out.contains("[[environments"),
            "usa el esquema por-ambiente"
        );
        assert!(
            !out.contains("[[favorites"),
            "no reescribe el listado plano legacy"
        );
    }

    #[test]
    fn legacy_migration_without_last_env_uses_defaults() {
        let v2 = r#"
            [[favorites]]
            view_id = "logs"
            key = "g1"
            label = "g1"
            is_favorite = true
        "#;
        let s = State::parse(v2).expect("v2 sin last_* parsea y migra");
        let favs = s.favorites_for("default", "us-east-1");
        assert_eq!(favs.len(), 1, "cae al ambiente default");
        assert_eq!(favs[0].key, "g1");
    }
}
