//! `effects` — el dispatcher y la ÚNICA frontera con el SDK. Recibe un `Action`,
//! hace `tokio::spawn` de una task contra el client correcto y manda un `Message`
//! de vuelta por el canal mpsc, etiquetado con el `epoch` del `Env` que lo lanzó.
//!
//! `Backend::Real` usa `aws-sdk-cloudwatchlogs` (vía `AwsContext`); `Backend::Mock`
//! sirve para tests y desarrollo sin red. Como los `Message`/DTOs son los mismos,
//! ni las vistas ni `app.rs` distinguen entre uno y otro. Cualquier fallo del SDK
//! se reporta como `Message::Error` (lo pinta la status bar, nunca hace panic).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::action::Action;
use crate::aws::context::{AwsContext, Env};
use crate::message::{Envelope, LogGroupDto, LogStreamDto, Message};

/// Fuente de datos.
enum Backend {
    /// Datos en memoria para tests y para `AWSDECK_MOCK=1` (demo sin red).
    Mock(Env),
    Real(Arc<AwsContext>),
}

/// Traduce `Action`s de efecto en tasks async. Mantiene la fuente activa (el
/// `AwsContext` por ambiente, o el `Env` en mock) y el `Sender` de resultados.
pub struct Effects {
    tx: mpsc::Sender<Envelope>,
    backend: Backend,
}

impl Effects {
    /// Effects contra el SDK real (aws-config + aws-sdk-cloudwatchlogs).
    pub fn new(tx: mpsc::Sender<Envelope>, env: Env) -> Self {
        Self {
            tx,
            backend: Backend::Real(Arc::new(AwsContext::new(env))),
        }
    }

    /// Effects contra datos mock en memoria (tests y `AWSDECK_MOCK=1`).
    pub fn new_mock(tx: mpsc::Sender<Envelope>, env: Env) -> Self {
        Self {
            tx,
            backend: Backend::Mock(env),
        }
    }

    /// Cambia el ambiente activo: en real reconstruye el `AwsContext` (cache
    /// fresco de clients); en mock solo re-etiqueta la data.
    pub fn set_env(&mut self, env: Env) {
        self.backend = match &self.backend {
            Backend::Mock(_) => Backend::Mock(env),
            Backend::Real(_) => Backend::Real(Arc::new(AwsContext::new(env))),
        };
    }

    /// Despacha una `Action` de efecto: lanza la task y retorna de inmediato (no
    /// bloquea el render). Las `Action` core las maneja el `App`.
    pub fn dispatch(&self, action: Action, epoch: u64) {
        match action {
            Action::LoadLogGroups => self.load_log_groups(epoch),
            Action::LoadLogStreams { group } => self.load_log_streams(group, epoch),
            Action::Quit | Action::ActivateView(_) | Action::SwitchEnv(_) => {}
        }
    }

    fn load_log_groups(&self, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(env) => {
                let env = env.clone();
                tokio::spawn(async move {
                    // Delay artificial: ejercita el path async y hace observable el
                    // epoch guard (cambiar de ambiente con un request en vuelo).
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let msg = Message::LogGroupsLoaded(mock_log_groups(&env));
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_groups(&ctx).await {
                        Ok(groups) => Message::LogGroupsLoaded(groups),
                        Err(e) => Message::Error(format!("describe_log_groups: {e}")),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_log_streams(&self, group: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let streams = mock_log_streams(&group);
                    let msg = Message::LogStreamsLoaded { group, streams };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_streams(&ctx, &group).await {
                        Ok(streams) => Message::LogStreamsLoaded { group, streams },
                        Err(e) => Message::Error(format!("describe_log_streams: {e}")),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }
}

// --- SDK real -----------------------------------------------------------------

async fn fetch_log_groups(ctx: &AwsContext) -> color_eyre::Result<Vec<LogGroupDto>> {
    let client = ctx.logs().await;
    let mut pages = client.describe_log_groups().into_paginator().items().send();

    let mut groups = Vec::new();
    while let Some(item) = pages.next().await {
        let g = item?;
        groups.push(LogGroupDto {
            name: g.log_group_name().unwrap_or_default().to_string(),
            stored_bytes: g.stored_bytes(),
            arn: g.arn().map(str::to_string),
        });
    }
    Ok(groups)
}

async fn fetch_log_streams(ctx: &AwsContext, group: &str) -> color_eyre::Result<Vec<LogStreamDto>> {
    use aws_sdk_cloudwatchlogs::types::OrderBy;

    let client = ctx.logs().await;
    let mut pages = client
        .describe_log_streams()
        .log_group_name(group)
        .order_by(OrderBy::LastEventTime)
        .descending(true)
        .into_paginator()
        .items()
        .send();

    let mut streams = Vec::new();
    while let Some(item) = pages.next().await {
        let s = item?;
        streams.push(LogStreamDto {
            name: s.log_stream_name().unwrap_or_default().to_string(),
            last_event_ts: s.last_event_timestamp(),
        });
    }
    Ok(streams)
}

// --- Mock (tests / sin red) ---------------------------------------------------

/// Log groups falsos del ambiente. Un par de nombres llevan el `profile` activo
/// para que un cambio de ambiente sea visible en la lista.
fn mock_log_groups(env: &Env) -> Vec<LogGroupDto> {
    let names = [
        format!("/aws/lambda/{}-orders-api", env.profile),
        format!("/aws/lambda/{}-payments-worker", env.profile),
        "/aws/lambda/notifications".to_string(),
        "/aws/apigateway/public-edge".to_string(),
        "/ecs/checkout-service".to_string(),
        "/ecs/inventory-service".to_string(),
        "/aws/rds/postgres-main".to_string(),
        "/aws/stepfunctions/order-saga".to_string(),
        "/aws/events/bus-default".to_string(),
        "/aws/sqs/dlq-monitor".to_string(),
    ];
    names
        .into_iter()
        .map(|name| {
            let stored = (name.len() as i64) * 7_919 % 5_000_000;
            let arn = format!(
                "arn:aws:logs:{}:000000000000:log-group:{}:*",
                env.region, name
            );
            LogGroupDto {
                name,
                stored_bytes: Some(stored),
                arn: Some(arn),
            }
        })
        .collect()
}

/// Log streams falsos de un group. El `group` siembra los ids para que cada group
/// tenga streams distintos de forma determinista.
fn mock_log_streams(group: &str) -> Vec<LogStreamDto> {
    let seed: u64 = group.bytes().map(u64::from).sum::<u64>().max(1);
    let base_ts: i64 = 1_750_000_000_000; // ~2025, epoch millis
    (0..14)
        .map(|i| {
            let id = seed
                .wrapping_mul(0x9E37_79B9)
                .wrapping_add((i as u64) * 0x1000);
            LogStreamDto {
                name: format!("2026/06/20/[$LATEST]{id:016x}"),
                last_event_ts: Some(base_ts - (i as i64) * 137_000),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_log_groups_tag_epoch_and_reflect_profile() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(Action::LoadLogGroups, 7);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 7, "el epoch se propaga al resultado");
        match envelope.message {
            Message::LogGroupsLoaded(groups) => {
                assert!(!groups.is_empty());
                assert!(
                    groups.iter().any(|g| g.name.contains("dev")),
                    "la data mock refleja el profile activo"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_log_streams_echo_group_and_epoch() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadLogStreams {
                group: "/ecs/checkout-service".into(),
            },
            3,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 3);
        match envelope.message {
            Message::LogStreamsLoaded { group, streams } => {
                assert_eq!(group, "/ecs/checkout-service");
                assert!(!streams.is_empty());
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }
}
