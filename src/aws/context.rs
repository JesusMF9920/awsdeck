//! `Env { profile, region }` + el parseo de profiles de `~/.aws/config` para el
//! picker de ambientes. `AwsContext`/ClientFactory (clients del SDK cacheados por
//! ambiente) llega en el commit "SDK real".

use std::fmt;
use std::path::PathBuf;

use aws_config::{BehaviorVersion, Region};
use aws_sdk_cloudwatchlogs::Client as LogsClient;
use tokio::sync::OnceCell;

/// El ambiente activo: identidad de la cuenta/region contra la que trabajamos.
/// Es estado global de primera clase del `App`; cambiarlo sube el epoch y
/// reconstruye los clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    pub profile: String,
    pub region: String,
}

impl Env {
    pub fn new(profile: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
            region: region.into(),
        }
    }
}

impl fmt::Display for Env {
    /// Render para el header: `profile · region`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} · {}", self.profile, self.region)
    }
}

/// ClientFactory por ambiente: construye y cachea de forma lazy los clients
/// tipados del SDK. `new` es síncrono (solo guarda el `Env`); el client real se
/// construye la primera vez que se usa, dentro de una task async de `effects`.
/// Al cambiar de ambiente se crea un `AwsContext` nuevo (cache fresco).
pub struct AwsContext {
    env: Env,
    logs: OnceCell<LogsClient>,
}

impl AwsContext {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            logs: OnceCell::new(),
        }
    }

    /// Cliente de CloudWatch Logs, construido y cacheado de forma perezosa vía
    /// `aws-config` (profile + región del `Env`). Credenciales/SSO los resuelve
    /// `aws-config`; nunca se hardcodea nada.
    pub async fn logs(&self) -> &LogsClient {
        self.logs
            .get_or_init(|| async {
                let config = aws_config::defaults(BehaviorVersion::latest())
                    .profile_name(&self.env.profile)
                    .region(Region::new(self.env.region.clone()))
                    .load()
                    .await;
                LogsClient::new(&config)
            })
            .await
    }
}

/// Un profile leído de `~/.aws/config`, con su región si la declara.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileEntry {
    pub name: String,
    pub region: Option<String>,
}

/// Lista los profiles de `~/.aws/config` (o `$AWS_CONFIG_FILE`). Devuelve vacío
/// si el archivo no existe o no se puede leer. Credenciales/SSO los resuelve
/// `aws-config` por nombre de profile; aquí solo enumeramos para el picker.
pub fn list_profiles() -> Vec<ProfileEntry> {
    match std::fs::read_to_string(config_path()) {
        Ok(content) => parse_profiles(&content),
        Err(_) => Vec::new(),
    }
}

fn config_path() -> PathBuf {
    if let Ok(path) = std::env::var("AWS_CONFIG_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".aws").join("config")
}

/// Parsea el INI de `~/.aws/config`: cabeceras `[default]` y `[profile NAME]`,
/// tomando `region` dentro de cada sección. Ignora `[sso-session ...]` y otras
/// secciones que no son profiles.
fn parse_profiles(content: &str) -> Vec<ProfileEntry> {
    let mut entries: Vec<ProfileEntry> = Vec::new();
    let mut current: Option<usize> = None;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let header = header.trim();
            let name = if header == "default" {
                Some("default".to_string())
            } else {
                header
                    .strip_prefix("profile ")
                    .map(|n| n.trim().to_string())
            };
            current = name.map(|name| {
                entries.push(ProfileEntry { name, region: None });
                entries.len() - 1
            });
        } else if let Some(idx) = current
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "region"
        {
            entries[idx].region = Some(value.trim().to_string());
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_named_and_region_less_profiles() {
        let cfg = "\
[default]
region = us-east-1

# un comentario
[profile prod]
region = eu-west-1
output = json

[profile no-region]
output = json

[sso-session my-sso]
sso_region = us-east-1
";
        let entries = parse_profiles(cfg);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["default", "prod", "no-region"]);
        assert_eq!(entries[0].region.as_deref(), Some("us-east-1"));
        assert_eq!(entries[1].region.as_deref(), Some("eu-west-1"));
        assert_eq!(entries[2].region, None);
    }
}
