//! `effects` — el dispatcher y la ÚNICA frontera con el SDK. Recibe un `Action`,
//! hace `tokio::spawn` de una task contra la fuente de datos correcta y manda un
//! `Message` de vuelta por el canal mpsc, etiquetado con el `epoch` del `Env` que
//! lo lanzó.
//!
//! En este commit la fuente es `Backend::Mock` (datos en memoria + un delay
//! artificial para ejercitar el path async y hacer observable el epoch guard).
//! El commit "SDK real" agrega `Backend::Real(AwsContext)` sin tocar vistas ni
//! `app.rs`, porque los `Message`/DTOs no cambian.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::action::Action;
use crate::aws::context::Env;
use crate::message::{Envelope, LogGroupDto, LogStreamDto, Message};

/// Fuente de datos. Arranca en `Mock`; "SDK real" agrega `Real`.
enum Backend {
    Mock,
}

/// Traduce `Action`s de efecto en tasks async. Mantiene la identidad del ambiente
/// activo (en mock, para etiquetar la data; en real, el `AwsContext` cacheado) y
/// el `Sender` por donde regresan los `Message`.
pub struct Effects {
    tx: mpsc::Sender<Envelope>,
    env: Env,
    backend: Backend,
}

impl Effects {
    pub fn new(tx: mpsc::Sender<Envelope>, env: Env) -> Self {
        Self {
            tx,
            env,
            backend: Backend::Mock,
        }
    }

    /// Cambia el ambiente activo de la fuente. En mock solo re-etiqueta la data;
    /// en real reconstruirá el cliente cacheado.
    pub fn set_env(&mut self, env: Env) {
        self.env = env;
    }

    /// Despacha una `Action` de efecto: lanza la task y retorna de inmediato (no
    /// bloquea el render). Las `Action` core las maneja el `App`; si alguna llega
    /// aquí por error, se ignora.
    pub fn dispatch(&self, action: Action, epoch: u64) {
        match action {
            Action::LoadLogGroups => self.load_log_groups(epoch),
            Action::LoadLogStreams { group } => self.load_log_streams(group, epoch),
            Action::Quit | Action::ActivateView(_) | Action::SwitchEnv(_) => {}
        }
    }

    fn load_log_groups(&self, epoch: u64) {
        let tx = self.tx.clone();
        let env = self.env.clone();
        match &self.backend {
            Backend::Mock => {
                tokio::spawn(async move {
                    // Delay artificial: ejercita el path async y hace observable el
                    // epoch guard (cambiar de ambiente con un request en vuelo).
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let msg = Message::LogGroupsLoaded(mock_log_groups(&env));
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_log_streams(&self, group: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let streams = mock_log_streams(&group);
                    let msg = Message::LogStreamsLoaded { group, streams };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }
}

// --- Datos mock (en memoria) ---------------------------------------------------

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
            // pseudo-tamaño determinista (sin RNG, para builds reproducibles).
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
            let id = seed.wrapping_mul(0x9E37_79B9).wrapping_add((i as u64) * 0x1000);
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
        let fx = Effects::new(tx, Env::new("dev", "us-east-1"));

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
        let fx = Effects::new(tx, Env::new("dev", "us-east-1"));

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
