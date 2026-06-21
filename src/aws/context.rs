//! `Env { profile, region }` + `AwsContext`: builder/cache lazy de clients del SDK
//! por ambiente (vía `aws-config`), más el parseo de profiles de `~/.aws/config`
//! para el picker. `AwsContext::new` es síncrono; los clients se construyen lazy
//! dentro de las tasks async de `effects`.
//!
//! En este commit solo vive `Env`. `AwsContext`/ClientFactory llegan en "SDK real".

use std::fmt;

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
