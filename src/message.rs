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
    /// ARN del log group. Reservado para `y` (copiar ARN/URL) en v1; el SDK ya lo
    /// puebla aunque v0 todavía no lo muestre.
    #[allow(dead_code)]
    pub arn: Option<String>,
}

/// Un log stream dentro de un log group.
#[derive(Clone, Debug)]
pub struct LogStreamDto {
    pub name: String,
    /// Epoch en milisegundos del último evento, si lo hay.
    pub last_event_ts: Option<i64>,
}

/// Una cola de SQS (datos para la lista).
#[derive(Clone, Debug)]
pub struct QueueDto {
    pub name: String,
    pub url: String,
    pub is_fifo: bool,
}

/// Attributes de una cola; se cargan al hacer drill (un request por cola).
#[derive(Clone, Debug, Default)]
pub struct QueueAttrsDto {
    pub visible: Option<i64>,
    pub in_flight: Option<i64>,
    pub delayed: Option<i64>,
    pub arn: Option<String>,
    /// ARN de la DLQ (de `RedrivePolicy`), si la cola tiene una configurada.
    pub dlq_target_arn: Option<String>,
    pub max_receive_count: Option<i64>,
}

impl QueueAttrsDto {
    pub fn has_dlq(&self) -> bool {
        self.dlq_target_arn.is_some()
    }
}

/// Un mensaje peekeado (receive sin borrar) de una cola.
#[derive(Clone, Debug)]
pub struct QueueMessageDto {
    pub id: String,
    pub body: String,
    pub sent_ts: Option<i64>,
    pub receive_count: Option<i64>,
}

// --- Step Functions (v2) ------------------------------------------------------

/// Tipo de state machine. Enum propio plano (no el del SDK, que es
/// `#[non_exhaustive]`): la vista nunca importa `aws-sdk-*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineType {
    Standard,
    Express,
}

/// Estado de una ejecución. Enum propio plano (ver `MachineType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecStatus {
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Aborted,
    /// Encolada para redrive; ni "rojo terminal" ni redrivable.
    PendingRedrive,
}

impl ExecStatus {
    /// Solo las ejecuciones terminadas en fallo se pueden redrivar.
    pub fn is_redrivable(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut | Self::Aborted)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::TimedOut => "TIMED_OUT",
            Self::Aborted => "ABORTED",
            Self::PendingRedrive => "PENDING_REDRIVE",
        }
    }
}

/// Una state machine (datos para la lista).
#[derive(Clone, Debug)]
pub struct StateMachineDto {
    pub arn: String,
    pub name: String,
    pub machine_type: MachineType,
    pub created_ts: Option<i64>,
}

/// Una ejecución de una state machine (datos para la lista).
#[derive(Clone, Debug)]
pub struct ExecutionDto {
    pub arn: String,
    pub name: String,
    pub status: ExecStatus,
    pub start_ts: Option<i64>,
    /// `None` = aún corriendo (duración "en curso").
    pub stop_ts: Option<i64>,
}

/// Detalle de una ejecución (de `describe_execution`). `input`/`output` ya vienen
/// pretty-printeados y truncados desde `effects`.
#[derive(Clone, Debug)]
pub struct ExecutionDetailDto {
    pub status: ExecStatus,
    pub start_ts: Option<i64>,
    pub stop_ts: Option<i64>,
    pub input: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub cause: Option<String>,
    pub redrive_count: Option<i64>,
}

/// Un estado del timeline de ejecución, ya emparejado (entered/exited) por
/// `effects::parse_history`. La duración la calcula la vista al render.
#[derive(Clone, Debug)]
pub struct StateSpanDto {
    pub name: String,
    pub entered_ts: Option<i64>,
    /// `None` = abierto sin salir (terminó en fallo o aún corriendo).
    pub exited_ts: Option<i64>,
    /// `true` si este estado fue el que reventó.
    pub failed: bool,
}

// --- EventBridge (v3) ---------------------------------------------------------

/// Estado de una rule. Enum propio plano (el del SDK es `#[non_exhaustive]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleState {
    Enabled,
    Disabled,
}

impl RuleState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Enabled => "ENABLED",
            Self::Disabled => "DISABLED",
        }
    }
}

/// Un event bus (datos para la lista).
#[derive(Clone, Debug)]
pub struct EventBusDto {
    pub arn: String,
    pub name: String,
}

/// Una rule de un bus (datos para la lista).
#[derive(Clone, Debug)]
pub struct RuleDto {
    pub name: String,
    pub event_bus_name: String,
    pub state: RuleState,
    pub description: Option<String>,
}

/// Detalle de una rule (de `describe_rule`). El nombre/bus ya los conoce la vista
/// por el nivel de drill. `event_pattern` ya viene pretty-printeado/truncado.
#[derive(Clone, Debug)]
pub struct RuleDetailDto {
    pub state: RuleState,
    pub description: Option<String>,
    pub event_pattern: Option<String>,
    pub schedule_expression: Option<String>,
}

/// Un target de una rule (de `list_targets_by_rule`). `input` ya viene
/// pretty-printeado/truncado desde `effects`.
#[derive(Clone, Debug)]
pub struct TargetDto {
    pub id: String,
    pub arn: String,
    pub input: Option<String>,
}

/// Resultado de una operación async. Específico de servicio a propósito
/// (`message.rs` es frontera permitida para nombrar servicios).
#[derive(Clone, Debug)]
pub enum Message {
    /// Se cargó una página de log groups. `query` ecoa la búsqueda que la originó
    /// (para que la vista descarte respuestas de búsquedas viejas); `more` indica
    /// que el servidor tiene más resultados (`next_token`).
    LogGroupsLoaded {
        groups: Vec<LogGroupDto>,
        query: Option<String>,
        more: bool,
    },
    /// Se cargaron los streams de `group` (se incluye `group` para que la vista
    /// confirme que corresponden al drill actual).
    LogStreamsLoaded {
        group: String,
        streams: Vec<LogStreamDto>,
    },
    /// Se cargaron las colas del ambiente activo.
    QueuesLoaded(Vec<QueueDto>),
    /// Detalle de una cola (attributes + peek). `queue_url` permite a la vista
    /// confirmar que corresponde al drill actual.
    QueueDetailLoaded {
        queue_url: String,
        attrs: QueueAttrsDto,
        messages: Vec<QueueMessageDto>,
    },
    /// Se purgó una cola (acción mutante confirmada).
    QueuePurged { queue_url: String },

    // --- Step Functions (v2) ---
    /// Se cargaron las state machines del ambiente activo. `more` indica que se
    /// alcanzó el tope de paginación con más máquinas pendientes (caso patológico).
    StateMachinesLoaded {
        machines: Vec<StateMachineDto>,
        more: bool,
    },
    /// Ejecuciones de una máquina (`machine_arn` para confirmar el drill actual).
    /// `more` indica que el servidor tiene más (`next_token`): se muestran las 50
    /// más recientes.
    ExecutionsLoaded {
        machine_arn: String,
        executions: Vec<ExecutionDto>,
        more: bool,
    },
    /// Detalle de una ejecución (`describe_execution` + history ya parseado).
    /// `execution_arn` permite a la vista confirmar el drill actual; `failed_state`
    /// es el estado que reventó (si lo hay), para saltar/resaltar.
    ExecutionDetailLoaded {
        execution_arn: String,
        detail: ExecutionDetailDto,
        history: Vec<StateSpanDto>,
        failed_state: Option<String>,
    },
    /// Se relanzó una ejecución vía redrive (acción mutante confirmada).
    ExecutionRedriven { execution_arn: String },

    // --- EventBridge (v3) ---
    /// Se cargaron los event buses del ambiente activo. `more` = se topó la
    /// paginación con más buses pendientes (caso patológico).
    EventBusesLoaded { buses: Vec<EventBusDto>, more: bool },
    /// Rules de un bus (`event_bus_name` para confirmar el drill actual).
    RulesLoaded {
        event_bus_name: String,
        rules: Vec<RuleDto>,
        more: bool,
    },
    /// Detalle de una rule (`describe_rule` + `list_targets_by_rule`).
    /// `event_bus_name`+`rule_name` confirman el drill actual.
    RuleDetailLoaded {
        event_bus_name: String,
        rule_name: String,
        detail: RuleDetailDto,
        targets: Vec<TargetDto>,
    },
    /// Se publicó un evento de prueba en un bus (acción mutante confirmada).
    EventSent { event_bus_name: String },

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
