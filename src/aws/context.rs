//! `Env { profile, region }` + `AwsContext`: builder/cache lazy de clients del SDK
//! por ambiente (vía `aws-config`), más el parseo de profiles de `~/.aws/config`
//! para el picker. `AwsContext::new` es síncrono; los clients se construyen lazy
//! dentro de las tasks async de `effects`.
//!
//! Se llena en los commits "action/message/env" (Env) y "SDK real" (ClientFactory).
