//! `Message` — resultados que regresan del mundo async, más los DTOs planos que
//! viajan a las vistas. Los DTOs **no** contienen tipos del SDK: por eso las
//! vistas nunca importan `aws-sdk-*` y se pueden testear inyectando `Message`s.
//!
//! Todo `Message` viaja envuelto en `Envelope { epoch }` por el canal mpsc; el
//! `App` descarta los envelopes cuyo epoch ya no coincide con el ambiente activo
//! (no pintar datos de la cuenta anterior tras un switch).

/// Un log group de CloudWatch, ya aplanado a datos planos para la vista.
#[derive(Clone, Debug)]
pub struct LogGroupDto {
    pub name: String,
    pub stored_bytes: Option<i64>,
    pub arn: Option<String>,
}

/// Un log stream dentro de un log group.
#[derive(Clone, Debug)]
pub struct LogStreamDto {
    pub name: String,
    /// Epoch en milisegundos del último evento, si lo hay.
    pub last_event_ts: Option<i64>,
}

/// Resultado de una operación async. Específico de servicio a propósito
/// (`message.rs` es frontera permitida para nombrar servicios).
#[derive(Clone, Debug)]
pub enum Message {
    /// Se cargaron los log groups del ambiente activo.
    LogGroupsLoaded(Vec<LogGroupDto>),
    /// Se cargaron los streams de `group` (se incluye `group` para que la vista
    /// confirme que corresponden al drill actual).
    LogStreamsLoaded {
        group: String,
        streams: Vec<LogStreamDto>,
    },
    /// Algo falló: se muestra en la status bar, nunca hace panic.
    Error(String),
}

/// Sobre con el que viaja cada `Message`: lleva el `epoch` del `Env` que lanzó la
/// petición, para que el `App` descarte respuestas stale tras un cambio de ambiente.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub epoch: u64,
    pub message: Message,
}

impl Envelope {
    pub fn new(epoch: u64, message: Message) -> Self {
        Self { epoch, message }
    }
}
