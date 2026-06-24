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

/// Un evento (línea) de log. `message` ya viene recortado desde `effects`.
#[derive(Clone, Debug)]
pub struct LogEventDto {
    /// Epoch en milisegundos del evento, si lo hay.
    pub ts: Option<i64>,
    pub message: String,
    /// Stream de origen. `Some` solo en el *tail* del group (`filter_log_events`
    /// trae eventos de varios streams); `None` por-stream (ya conoces el stream).
    pub stream: Option<String>,
}

/// Ventana de tiempo de los logs del group. Dato plano; `effects` la traduce a
/// `start_time`/`end_time` (poniendo el reloj para `Last` y para `to: None`). La
/// vista nunca ve `now`: solo describe el rango.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogWindow {
    /// Últimos `n` milisegundos (start = now - n, end = now).
    Last(i64),
    /// Rango absoluto en epoch millis; `to: None` = ahora.
    Range { from: i64, to: Option<i64> },
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
    /// URLs de las colas que usan ESTA cola como su DLQ (`ListDeadLetterSourceQueues`).
    /// No vacío ⇒ la cola **es** un DLQ → se ofrece el redrive (`d`). Lo puebla `effects`.
    pub dlq_sources: Vec<String>,
}

impl QueueAttrsDto {
    /// La cola tiene configurada una DLQ (perspectiva de origen).
    pub fn has_dlq(&self) -> bool {
        self.dlq_target_arn.is_some()
    }

    /// La cola **es** un DLQ de otras (≥1 cola origen): habilita el redrive.
    pub fn is_dlq(&self) -> bool {
        !self.dlq_sources.is_empty()
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

// --- Lambda -------------------------------------------------------------------

/// Una función Lambda (datos para la lista).
#[derive(Clone, Debug)]
pub struct FunctionDto {
    pub name: String,
    pub arn: String,
    /// Runtime aplanado a String (p. ej. `"python3.12"`); `None` en funciones de imagen.
    pub runtime: Option<String>,
    pub last_modified: Option<String>,
    pub memory: Option<i32>,
}

/// Configuración de una función; se carga al hacer drill (`get_function`).
#[derive(Clone, Debug, Default)]
pub struct FunctionDetailDto {
    pub runtime: Option<String>,
    pub handler: Option<String>,
    pub memory: Option<i32>,
    pub timeout: Option<i32>,
    pub code_size: Option<i64>,
    pub last_modified: Option<String>,
    pub role: Option<String>,
    pub description: Option<String>,
    pub layers: Vec<String>,
    pub tracing: Option<String>,
    pub dlq_target: Option<String>,
    /// Variables de entorno `(clave, valor)`, ordenadas por clave.
    pub env: Vec<(String, String)>,
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
    /// Input del estado (`StateEntered`), ya pretty-printeado/truncado desde `effects`.
    /// `enter` sobre el estado lo expande en un panel scrolleable. `None` = no expuesto.
    pub input: Option<String>,
    /// Output del estado (`StateExited`), ya pretty-printeado/truncado. `None` = el estado
    /// no salió (falló o sigue corriendo) o no lo expone.
    pub output: Option<String>,
    /// ARN/identidad de la Lambda que invocó este estado, si es una invocación Lambda
    /// (integración directa o `arn:aws:states:::lambda:invoke`). Habilita el cross-link
    /// `sfn` → logs de la Lambda (`l`). `None` = el estado no invoca una Lambda (o no se
    /// pudo determinar). Lo puebla `effects::parse_history`.
    pub resource_arn: Option<String>,
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

/// Clase de un error, derivada del fallo del SDK en `effects`. **No nombra
/// servicios**: el core (`app.rs`) puede ramificar sobre ella sin dejar de ser
/// agnóstico (mostrar un hint de re-auth, marcar un transitorio como reintentable…).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Credenciales/sesión caducadas o ausentes (SSO/STS). Acción: re-autenticar.
    Auth,
    /// Permiso IAM faltante para la operación.
    AccessDenied,
    /// El servicio está limitando la tasa (throttling). Transitorio.
    Throttle,
    /// Fallo de red/conexión/timeout. Transitorio.
    Network,
    /// Cualquier otro fallo.
    Other,
}

impl ErrorKind {
    /// Clasifica por palabras clave a partir de la **cadena de causas** del error
    /// del SDK (que aplana code/message en `source()`). Insensible a mayúsculas.
    /// Heurística a propósito: el `SdkError` ya viene aplanado a `String` cuando
    /// llega aquí, así que no hay tipo sobre el cual hacer match exacto.
    pub fn classify(chain: &str) -> Self {
        let c = chain.to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| c.contains(n));
        if has(&[
            "expiredtoken",
            "expired",
            "the sso session",
            "session associated",
            "unrecognizedclient",
            "invalidclienttokenid",
            "no credentials",
            "credentials were not",
            "could not load credentials",
            "unable to load credentials",
        ]) {
            Self::Auth
        } else if has(&[
            "accessdenied",
            "not authorized",
            "is not authorized to perform",
        ]) {
            Self::AccessDenied
        } else if has(&["throttl", "toomanyrequests", "rate exceeded", "slowdown"]) {
            Self::Throttle
        } else if has(&[
            "dispatch failure",
            "timeout",
            "timed out",
            "connect",
            "dns",
            "io error",
        ]) {
            Self::Network
        } else {
            Self::Other
        }
    }

    /// Pista accionable para la status bar (lo no-obvio de cómo recuperarse).
    /// `None` = sin pista; se muestra el detalle crudo del SDK.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Self::Auth => Some(
                "sesión/credenciales caducadas — corre `aws sso login` o cambia de perfil con ctrl-e",
            ),
            Self::AccessDenied => Some("falta permiso IAM para esta operación"),
            Self::Throttle => Some("throttling del servicio — reintenta con r"),
            Self::Network => Some("problema de red — reintenta con r"),
            Self::Other => None,
        }
    }

    /// Transitorios: tiene sentido reintentar (`r`) sin cambiar nada. Reservado para
    /// el reintento automático de transitorios (P1); hoy el hint ya invita a `r`.
    #[allow(dead_code)]
    pub fn retryable(self) -> bool {
        matches!(self, Self::Throttle | Self::Network)
    }
}

/// Resultado de una operación async. Específico de servicio a propósito
/// (`message.rs` es frontera permitida para nombrar servicios).
#[derive(Clone, Debug)]
pub enum Message {
    /// Se cargó una página de log groups. `query` ecoa la búsqueda que la originó
    /// (para que la vista descarte respuestas de búsquedas viejas); `more` indica que
    /// el servidor tiene más resultados (`next_token`).
    LogGroupsLoaded {
        groups: Vec<LogGroupDto>,
        query: Option<String>,
        more: bool,
    },
    /// Se cargaron los streams de `group` (se incluye `group` para que la vista
    /// confirme que corresponden al drill actual). `more` = se topó el tope de
    /// paginación con más streams pendientes (la vista muestra `· parcial`).
    LogStreamsLoaded {
        group: String,
        streams: Vec<LogStreamDto>,
        more: bool,
    },
    /// Eventos de un stream (`get_log_events`). `group`+`stream` confirman el drill
    /// actual; `next_token` = hay líneas más viejas (para `o` traer más); `append` = es
    /// una página más vieja (la vista la **antepone**, las líneas viejas van arriba).
    LogEventsLoaded {
        group: String,
        stream: String,
        events: Vec<LogEventDto>,
        next_token: Option<String>,
        append: bool,
    },
    /// Logs del group por rango de tiempo (`filter_log_events` sobre todos sus
    /// streams). `group` confirma el drill actual; `generation` es la generación que la
    /// originó (la vista descarta respuestas con generation viejo: ventana/patrón/drill
    /// cambiaron); `next_token` = hay más en la ventana (para `o` cargar más);
    /// `append` = es continuación de una página previa (la vista la APPENDea).
    LogTailLoaded {
        group: String,
        events: Vec<LogEventDto>,
        next_token: Option<String>,
        append: bool,
        generation: u64,
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
    /// Se inició el redrive de un DLQ (`StartMessageMoveTask`): los mensajes vuelven a
    /// sus colas origen. `queue_url` = el DLQ (para refrescar su detalle).
    DlqRedriveStarted { queue_url: String },

    // --- Lambda ---
    /// Una página de funciones Lambda (se cargan en streaming: la primera llega tras un
    /// solo round-trip y la vista ya es usable; el resto se anexa de fondo). `append` =
    /// anexar (vs reemplazar, primera página); `more` = aún vienen más páginas (la vista
    /// mantiene "cargando…"); `partial` = se cortó por el tope de páginas (`· parcial`).
    FunctionsLoaded {
        functions: Vec<FunctionDto>,
        append: bool,
        more: bool,
        partial: bool,
    },
    /// Detalle de una función (`get_function`). `function_arn` permite a la vista
    /// confirmar que corresponde al drill actual.
    FunctionDetailLoaded {
        function_arn: String,
        detail: FunctionDetailDto,
    },

    // --- Step Functions (v2) ---
    /// Se cargaron las state machines del ambiente activo. `more` indica que se
    /// alcanzó el tope de paginación con más máquinas pendientes (caso patológico).
    StateMachinesLoaded {
        machines: Vec<StateMachineDto>,
        more: bool,
    },
    /// Ejecuciones de una máquina (`machine_arn` para confirmar el drill actual).
    /// `next_token` = el servidor tiene más (para `o` cargar más); `append` = es
    /// continuación de una página previa (la vista la APPENDea, como el tail de logs).
    ExecutionsLoaded {
        machine_arn: String,
        executions: Vec<ExecutionDto>,
        next_token: Option<String>,
        append: bool,
    },
    /// Detalle de una ejecución (`describe_execution` + history ya parseado).
    /// `execution_arn` permite a la vista confirmar el drill actual; `failed_state`
    /// es el estado que reventó (si lo hay), para saltar/resaltar. `history_more` = se
    /// topó el tope de paginación del history (>~10k eventos): la vista muestra `· parcial`.
    ExecutionDetailLoaded {
        execution_arn: String,
        detail: ExecutionDetailDto,
        history: Vec<StateSpanDto>,
        failed_state: Option<String>,
        history_more: bool,
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

    /// Identidad de la cuenta activa (STS `GetCallerIdentity`): confirma contra qué
    /// cuenta trabajamos. El `App` la muestra en el header (prod-safe: ver la cuenta
    /// real antes de una acción mutante).
    IdentityLoaded { account_id: String },

    /// Algo falló: se muestra en la status bar, nunca hace panic. `kind` clasifica
    /// el fallo (para que el core ofrezca recuperación —p. ej. una pista de re-auth—
    /// sin nombrar servicios) y `detail` es el texto ya listo para pintar (incluye el
    /// hint si aplica). Si es transitorio se deriva de `kind.retryable()`.
    Error { kind: ErrorKind, detail: String },
}

impl Message {
    /// Error genérico no-SDK (clipboard, navegador, …): `ErrorKind::Other`.
    pub fn err(detail: impl Into<String>) -> Self {
        Self::Error {
            kind: ErrorKind::Other,
            detail: detail.into(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_common_sdk_failures() {
        use ErrorKind::*;
        // Cadenas representativas de la causa real (lo que vive en source()).
        assert_eq!(
            ErrorKind::classify(
                "ExpiredTokenException: the security token included in the request is expired"
            ),
            Auth
        );
        assert_eq!(
            ErrorKind::classify("the SSO session associated with this profile has expired"),
            Auth
        );
        assert_eq!(
            ErrorKind::classify(
                "UnrecognizedClientException: The security token included in the request is invalid"
            ),
            Auth
        );
        assert_eq!(
            ErrorKind::classify("dispatch failure: could not load credentials"),
            Auth
        );
        assert_eq!(
            ErrorKind::classify(
                "AccessDeniedException: User is not authorized to perform: logs:DescribeLogGroups"
            ),
            AccessDenied
        );
        assert_eq!(
            ErrorKind::classify("ThrottlingException: Rate exceeded"),
            Throttle
        );
        assert_eq!(ErrorKind::classify("TooManyRequestsException"), Throttle);
        assert_eq!(
            ErrorKind::classify("dispatch failure: timeout while connecting"),
            Network
        );
        assert_eq!(
            ErrorKind::classify("ValidationException: invalid pattern"),
            Other
        );
    }

    #[test]
    fn hint_and_retryable_are_consistent() {
        assert!(ErrorKind::Auth.hint().is_some());
        assert!(ErrorKind::AccessDenied.hint().is_some());
        assert!(ErrorKind::Other.hint().is_none());
        // Solo los transitorios invitan a reintentar.
        assert!(ErrorKind::Throttle.retryable());
        assert!(ErrorKind::Network.retryable());
        assert!(!ErrorKind::Auth.retryable());
        assert!(!ErrorKind::Other.retryable());
    }

    #[test]
    fn err_constructor_is_other() {
        match Message::err("boom") {
            Message::Error { kind, detail } => {
                assert_eq!(kind, ErrorKind::Other);
                assert_eq!(detail, "boom");
                assert!(!kind.retryable());
            }
            _ => panic!("debe ser Error"),
        }
    }
}
