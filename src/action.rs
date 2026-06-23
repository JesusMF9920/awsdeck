//! `Action` — intenciones que emiten las vistas / el usuario. El `App` maneja las
//! variantes "core" (agnósticas de servicio) y **reenvía el resto a `effects`**
//! sin inspeccionarlas, de modo que `app.rs` nunca nombra un servicio concreto.
//!
//! Las variantes específicas de servicio (p. ej. cargar log groups) viven aquí a
//! propósito: `action.rs` es una de las fronteras donde se permite nombrar un
//! servicio (junto con `message.rs`, `effects.rs`, `aws/` y `main.rs`).

use crate::aws::context::Env;
use crate::message::LogWindow;

#[derive(Clone, Debug)]
pub enum Action {
    // --- Core: las maneja `App` directamente (agnóstico de servicio) ---
    /// Salir de la aplicación.
    Quit,
    /// Activar la vista con este `id` (p. ej. desde `:logs`).
    ActivateView(String),
    /// Volver un nivel. La vista la emite cuando `esc` ya no tiene drill que
    /// despojar (está en su raíz); el `App` la interpreta como "volver al menú".
    /// La vista nunca nombra al menú: solo dice "atrás".
    Back,
    /// Limpiar el filtro activo. La vista la emite al cambiar de nivel de drill
    /// (entrar a un hijo o subir a un padre), para que el filtro de un nivel no se
    /// arrastre al siguiente (estilo k9s). El `App` —dueño del filtro— la ejecuta.
    ClearFilter,
    /// Cambiar de ambiente: sube el epoch, reconstruye el `AwsContext` y refresca.
    SwitchEnv(Env),
    /// Copiar texto (ARN/URL/línea) al portapapeles. Agnóstico: la vista decide qué
    /// identificador del item seleccionado copiar; el `App` lo copia y avisa en la
    /// status bar. No es un efecto del SDK (no va a `effects`).
    CopyToClipboard { text: String },
    /// Abrir el recurso seleccionado en la consola web de AWS. La vista arma el
    /// `ConsoleTarget`; `effects` construye la URL con la región activa y abre el
    /// navegador (el `App` la reenvía sin inspeccionarla).
    OpenConsole { target: ConsoleTarget },

    // --- Efectos: `App` los reenvía a `effects::dispatch` (específicos de servicio) ---
    /// Pedir una página acotada (≤50) de log groups. `query=None` = primera página;
    /// `query=Some(p)` = búsqueda server-side por subcadena (`logGroupNamePattern`,
    /// infix). La vista rankea localmente con fuzzy lo que vuelve.
    LoadLogGroups { query: Option<String> },
    /// Hacer drill: pedir los log streams de un log group.
    LoadLogStreams { group: String },
    /// Hacer drill a un stream: sus eventos más recientes (`get_log_events`).
    LoadLogEvents { group: String, stream: String },
    /// Logs de un group por rango de tiempo (`filter_log_events` sobre todos sus
    /// streams). `pattern` = filtro server-side; `window` = rango; `token=Some` =
    /// continuar paginando (load-more, append); `generation` = generación (staleness).
    LoadLogTail {
        group: String,
        pattern: Option<String>,
        window: LogWindow,
        token: Option<String>,
        generation: u64,
    },
    /// Pedir la lista de colas SQS del ambiente activo.
    LoadQueues,
    /// Hacer drill a una cola: attributes + peek de mensajes.
    LoadQueueDetail { queue_url: String },
    /// Pedir las state machines de Step Functions del ambiente activo.
    LoadStateMachines,
    /// Hacer drill a una máquina: sus ejecuciones (más recientes primero).
    LoadExecutions { machine_arn: String },
    /// Hacer drill a una ejecución: describe + history (timeline de estados).
    LoadExecutionDetail { execution_arn: String },
    /// Pedir los event buses de EventBridge del ambiente activo.
    LoadEventBuses,
    /// Hacer drill a un bus: sus rules.
    LoadRules { event_bus_name: String },
    /// Hacer drill a una rule: describe + targets (patrón + destinos).
    LoadRuleDetail {
        event_bus_name: String,
        rule_name: String,
    },

    // --- Mutantes: gated por el `App` (modo escritura + confirm) antes de effects ---
    /// Purgar una cola: borra TODOS sus mensajes. Irreversible.
    PurgeQueue { queue_url: String },
    /// Redrive de una ejecución fallida: la relanza desde el último estado fallido.
    RedriveExecution { execution_arn: String },
    /// Publicar un evento de prueba (canned) contra un bus de EventBridge.
    SendEvent { event_bus_name: String },
}

/// Recurso a abrir en la consola de AWS. Service-shaped (vive en `action.rs`, frontera
/// permitida); `effects` lo traduce a una URL con la región activa. El `App` no lo
/// inspecciona.
#[derive(Clone, Debug)]
pub enum ConsoleTarget {
    LogGroup { name: String },
    SqsQueue { url: String },
    StateMachine { arn: String },
    Execution { arn: String },
    EventBus { name: String },
    Rule { event_bus: String, name: String },
}
