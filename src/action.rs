//! `Action` — intenciones que emiten las vistas / el usuario. El `App` maneja las
//! variantes "core" (agnósticas de servicio) y **reenvía el resto a `effects`**
//! sin inspeccionarlas, de modo que `app.rs` nunca nombra un servicio concreto.
//!
//! Las variantes específicas de servicio (p. ej. cargar log groups) viven aquí a
//! propósito: `action.rs` es una de las fronteras donde se permite nombrar un
//! servicio (junto con `message.rs`, `effects.rs`, `aws/` y `main.rs`).

use crate::aws::context::Env;

#[derive(Clone, Debug)]
pub enum Action {
    // --- Core: las maneja `App` directamente (agnóstico de servicio) ---
    /// Salir de la aplicación.
    Quit,
    /// Activar la vista con este `id` (p. ej. desde `:logs`).
    ActivateView(String),
    /// Cambiar de ambiente: sube el epoch, reconstruye el `AwsContext` y refresca.
    SwitchEnv(Env),

    // --- Efectos: `App` los reenvía a `effects::dispatch` (específicos de servicio) ---
    /// Pedir log groups: una página acotada (≤50). `query=None` = primeros 50;
    /// `query=Some(p)` = búsqueda server-side por substring (`logGroupNamePattern`).
    LoadLogGroups { query: Option<String> },
    /// Hacer drill: pedir los log streams de un log group.
    LoadLogStreams { group: String },
    /// Pedir la lista de colas SQS del ambiente activo.
    LoadQueues,
    /// Hacer drill a una cola: attributes + peek de mensajes.
    LoadQueueDetail { queue_url: String },

    // --- Mutantes: gated por el `App` (modo escritura + confirm) antes de effects ---
    /// Purgar una cola: borra TODOS sus mensajes. Irreversible.
    PurgeQueue { queue_url: String },
}
