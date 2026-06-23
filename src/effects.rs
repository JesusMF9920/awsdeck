//! `effects` — el dispatcher y la ÚNICA frontera con el SDK. Recibe un `Action`,
//! hace `tokio::spawn` de una task contra el client correcto y manda un `Message`
//! de vuelta por el canal mpsc, etiquetado con el `epoch` del `Env` que lo lanzó.
//!
//! `Backend::Real` usa los `aws-sdk-*` (vía `AwsContext`); `Backend::Mock`
//! sirve para tests y desarrollo sin red. Como los `Message`/DTOs son los mismos,
//! ni las vistas ni `app.rs` distinguen entre uno y otro. Cualquier fallo del SDK
//! se reporta como `Message::Error` (lo pinta la status bar, nunca hace panic).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::action::{Action, ConsoleTarget};
use crate::aws::context::{AwsContext, Env};
use crate::message::{
    Envelope, ErrorKind, EventBusDto, ExecStatus, ExecutionDetailDto, ExecutionDto, LogEventDto,
    LogGroupDto, LogStreamDto, LogWindow, MachineType, Message, QueueAttrsDto, QueueDto,
    QueueMessageDto, RuleDetailDto, RuleDto, RuleState, StateMachineDto, StateSpanDto, TargetDto,
};
use crate::util::case_variants;

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

    /// Región del ambiente activo (mock o real), para construir URLs de la consola.
    fn region(&self) -> String {
        match &self.backend {
            Backend::Mock(env) => env.region.clone(),
            Backend::Real(ctx) => ctx.region().to_string(),
        }
    }

    /// Abre el recurso en la consola de AWS: construye la URL con la región activa y
    /// lanza el navegador en un task (sin tocar el SDK). Reporta error si no se pudo.
    fn open_console(&self, target: ConsoleTarget, epoch: u64) {
        let url = console_url(&self.region(), &target);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let opened = tokio::task::spawn_blocking(move || open::that(url)).await;
            if !matches!(opened, Ok(Ok(()))) {
                let msg = Message::err("no se pudo abrir el navegador");
                let _ = tx.send(Envelope::new(epoch, msg)).await;
            }
        });
    }

    /// Despacha una `Action` de efecto: lanza la task y retorna de inmediato (no
    /// bloquea el render). Las `Action` core las maneja el `App`.
    pub fn dispatch(&self, action: Action, epoch: u64) {
        match action {
            Action::LoadLogGroups { query } => self.load_log_groups(query, epoch),
            Action::LoadLogStreams { group } => self.load_log_streams(group, epoch),
            Action::LoadLogEvents {
                group,
                stream,
                token,
            } => self.load_log_events(group, stream, token, epoch),
            Action::LoadLogTail {
                group,
                pattern,
                window,
                token,
                generation,
            } => self.load_log_tail(group, pattern, window, token, generation, epoch),
            Action::LoadQueues => self.load_queues(epoch),
            Action::LoadQueueDetail { queue_url } => self.load_queue_detail(queue_url, epoch),
            Action::PurgeQueue { queue_url } => self.purge_queue(queue_url, epoch),
            Action::RedriveDlq { queue_url } => self.redrive_dlq(queue_url, epoch),
            Action::LoadStateMachines => self.load_state_machines(epoch),
            Action::LoadExecutions {
                machine_arn,
                status,
                token,
            } => self.load_executions(machine_arn, status, token, epoch),
            Action::LoadExecutionDetail { execution_arn } => {
                self.load_execution_detail(execution_arn, epoch)
            }
            Action::RedriveExecution { execution_arn } => {
                self.redrive_execution(execution_arn, epoch)
            }
            Action::LoadEventBuses => self.load_event_buses(epoch),
            Action::LoadRules { event_bus_name } => self.load_rules(event_bus_name, epoch),
            Action::LoadRuleDetail {
                event_bus_name,
                rule_name,
            } => self.load_rule_detail(event_bus_name, rule_name, epoch),
            Action::SendEvent {
                event_bus_name,
                source,
                detail_type,
                detail,
                time,
                resources,
            } => self.send_event(
                event_bus_name,
                source,
                detail_type,
                detail,
                time,
                resources,
                epoch,
            ),
            Action::OpenConsole { target } => self.open_console(target, epoch),
            Action::VerifyIdentity => self.verify_identity(epoch),
            Action::Quit
            | Action::ActivateView(_)
            | Action::ActivateViewWithContext { .. }
            | Action::Back
            | Action::ClearFilter
            | Action::CopyToClipboard { .. }
            | Action::SwitchEnv(_) => {}
        }
    }

    fn load_log_groups(&self, query: Option<String>, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(env) => {
                let env = env.clone();
                tokio::spawn(async move {
                    // Delay artificial: ejercita el path async y hace observable el
                    // epoch guard (cambiar de ambiente con un request en vuelo).
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let msg = Message::LogGroupsLoaded {
                        groups: mock_log_groups(&env, query.as_deref()),
                        query,
                        more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_groups(&ctx, query.as_deref()).await {
                        Ok((groups, more)) => Message::LogGroupsLoaded {
                            groups,
                            query,
                            more,
                        },
                        Err(e) => sdk_error("describe_log_groups", &e),
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
                    let msg = Message::LogStreamsLoaded {
                        group,
                        streams,
                        more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_streams(&ctx, &group).await {
                        Ok((streams, more)) => Message::LogStreamsLoaded {
                            group,
                            streams,
                            more,
                        },
                        Err(e) => sdk_error("describe_log_streams", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_log_events(&self, group: String, stream: String, token: Option<String>, epoch: u64) {
        let tx = self.tx.clone();
        let append = token.is_some();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let events = mock_log_events(&group, &stream);
                    let msg = Message::LogEventsLoaded {
                        group,
                        stream,
                        events,
                        next_token: None,
                        append,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_events(&ctx, &group, &stream, token).await {
                        Ok((events, next_token)) => Message::LogEventsLoaded {
                            group,
                            stream,
                            events,
                            next_token,
                            append,
                        },
                        Err(e) => sdk_error("get_log_events", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_log_tail(
        &self,
        group: String,
        pattern: Option<String>,
        window: LogWindow,
        token: Option<String>,
        generation: u64,
        epoch: u64,
    ) {
        let tx = self.tx.clone();
        // Con token, esta respuesta continúa una página previa → la vista la appendea.
        let append = token.is_some();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let events = mock_log_tail(&group, pattern.as_deref());
                    let msg = Message::LogTailLoaded {
                        group,
                        events,
                        next_token: None,
                        append,
                        generation,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_tail(&ctx, &group, pattern.as_deref(), window, token)
                        .await
                    {
                        Ok((events, next_token)) => Message::LogTailLoaded {
                            group,
                            events,
                            next_token,
                            append,
                            generation,
                        },
                        Err(e) => sdk_error("filter_log_events", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_queues(&self, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(env) => {
                let env = env.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let msg = Message::QueuesLoaded(mock_queues(&env));
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_queues(&ctx).await {
                        Ok(queues) => Message::QueuesLoaded(queues),
                        Err(e) => sdk_error("list_queues", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_queue_detail(&self, queue_url: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let (attrs, messages) = mock_queue_detail(&queue_url);
                    let msg = Message::QueueDetailLoaded {
                        queue_url,
                        attrs,
                        messages,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_queue_detail(&ctx, &queue_url).await {
                        Ok((attrs, messages)) => Message::QueueDetailLoaded {
                            queue_url,
                            attrs,
                            messages,
                        },
                        Err(e) => sdk_error("queue detail", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn purge_queue(&self, queue_url: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let msg = Message::QueuePurged { queue_url };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match purge_queue_real(&ctx, &queue_url).await {
                        Ok(()) => Message::QueuePurged { queue_url },
                        Err(e) => sdk_error("purge_queue", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn redrive_dlq(&self, queue_url: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let msg = Message::DlqRedriveStarted { queue_url };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match redrive_dlq_real(&ctx, &queue_url).await {
                        Ok(()) => Message::DlqRedriveStarted { queue_url },
                        Err(e) => sdk_error("start_message_move_task", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    // --- Step Functions (v2) --------------------------------------------------

    fn load_state_machines(&self, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(env) => {
                let env = env.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let msg = Message::StateMachinesLoaded {
                        machines: mock_state_machines(&env),
                        more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_state_machines(&ctx).await {
                        Ok((machines, more)) => Message::StateMachinesLoaded { machines, more },
                        Err(e) => sdk_error("list_state_machines", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_executions(
        &self,
        machine_arn: String,
        status: Option<ExecStatus>,
        token: Option<String>,
        epoch: u64,
    ) {
        let tx = self.tx.clone();
        let append = token.is_some();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let executions = mock_executions(&machine_arn, status);
                    let msg = Message::ExecutionsLoaded {
                        machine_arn,
                        executions,
                        next_token: None,
                        append,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_executions(&ctx, &machine_arn, status, token).await {
                        Ok((executions, next_token)) => Message::ExecutionsLoaded {
                            machine_arn,
                            executions,
                            next_token,
                            append,
                        },
                        Err(e) => sdk_error("list_executions", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_execution_detail(&self, execution_arn: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let (detail, history, failed_state) = mock_execution_detail(&execution_arn);
                    let msg = Message::ExecutionDetailLoaded {
                        execution_arn,
                        detail,
                        history,
                        failed_state,
                        history_more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_execution_detail(&ctx, &execution_arn, MAX_HISTORY_PAGES)
                        .await
                    {
                        Ok((detail, history, failed_state, history_more)) => {
                            Message::ExecutionDetailLoaded {
                                execution_arn,
                                detail,
                                history,
                                failed_state,
                                history_more,
                            }
                        }
                        Err(e) => sdk_error("execution detail", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn redrive_execution(&self, execution_arn: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let msg = Message::ExecutionRedriven { execution_arn };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match redrive_execution_real(&ctx, &execution_arn).await {
                        Ok(()) => Message::ExecutionRedriven { execution_arn },
                        Err(e) => sdk_error("redrive_execution", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    // --- EventBridge (v3) -----------------------------------------------------

    fn load_event_buses(&self, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(env) => {
                let env = env.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let msg = Message::EventBusesLoaded {
                        buses: mock_event_buses(&env),
                        more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_event_buses(&ctx).await {
                        Ok((buses, more)) => Message::EventBusesLoaded { buses, more },
                        Err(e) => sdk_error("list_event_buses", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_rules(&self, event_bus_name: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let rules = mock_rules(&event_bus_name);
                    let msg = Message::RulesLoaded {
                        event_bus_name,
                        rules,
                        more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_rules(&ctx, &event_bus_name).await {
                        Ok((rules, more)) => Message::RulesLoaded {
                            event_bus_name,
                            rules,
                            more,
                        },
                        Err(e) => sdk_error("list_rules", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_rule_detail(&self, event_bus_name: String, rule_name: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let (detail, targets) = mock_rule_detail(&event_bus_name, &rule_name);
                    let msg = Message::RuleDetailLoaded {
                        event_bus_name,
                        rule_name,
                        detail,
                        targets,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_rule_detail(&ctx, &event_bus_name, &rule_name).await {
                        Ok((detail, targets)) => Message::RuleDetailLoaded {
                            event_bus_name,
                            rule_name,
                            detail,
                            targets,
                        },
                        Err(e) => sdk_error("rule detail", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn send_event(
        &self,
        event_bus_name: String,
        source: String,
        detail_type: String,
        detail: String,
        time: Option<i64>,
        resources: Vec<String>,
        epoch: u64,
    ) {
        let tx = self.tx.clone();
        match &self.backend {
            // El mock ignora el payload (incluidos time/resources): solo ecoa EventSent.
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let msg = Message::EventSent { event_bus_name };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match send_event_real(
                        &ctx,
                        &event_bus_name,
                        &source,
                        &detail_type,
                        &detail,
                        time,
                        &resources,
                    )
                    .await
                    {
                        Ok(()) => Message::EventSent { event_bus_name },
                        Err(e) => sdk_error("put_events", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn verify_identity(&self, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    let msg = Message::IdentityLoaded {
                        account_id: "123456789012".to_string(),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match get_caller_identity(&ctx).await {
                        Ok(account_id) => Message::IdentityLoaded { account_id },
                        // Un fallo aquí ES la señal de SSO/credenciales caducadas: se
                        // clasifica como `Auth` y enciende el `[re-auth]` del header.
                        Err(e) => sdk_error("get_caller_identity", &e),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }
}

/// STS `GetCallerIdentity`: devuelve el id de cuenta de 12 dígitos contra el que
/// estamos trabajando. Confirma la cuenta real (un `~/.aws/config` mal puesto puede
/// no corresponder al nombre del profile).
async fn get_caller_identity(ctx: &AwsContext) -> color_eyre::Result<String> {
    let out = ctx.sts().await.get_caller_identity().send().await?;
    Ok(out.account().unwrap_or_default().to_string())
}

/// Traduce un error del SDK (ya aplanado a `Report` por el `?`) en un
/// `Message::Error` **clasificado y accionable**. Recorre toda la cadena de causas
/// —el `SdkError` esconde el code/message reales (ExpiredToken, AccessDenied,
/// Throttling…) en `source()`, no en su `Display` de tope— y la pasa por
/// `ErrorKind::classify`. Con hint (Auth/AccessDenied/Throttle/Network) se muestra
/// la pista accionable; sin hint, el detalle crudo. Única frontera con el SDK: el
/// resto de la app solo ve el `Message` plano.
fn sdk_error(op: &str, e: &color_eyre::eyre::Report) -> Message {
    let chain = e
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let kind = ErrorKind::classify(&chain);
    let detail = match kind.hint() {
        Some(hint) => format!("{op}: {hint}"),
        None => format!("{op}: {chain}"),
    };
    Message::Error { kind, detail }
}

// --- Consola AWS (URLs) -------------------------------------------------------

/// URL (best-effort) de la consola web de AWS para `target`, en `region`. Los
/// deep-links pueden cambiar entre versiones de la consola; el objetivo es aterrizar
/// en el recurso/región correctos.
fn console_url(region: &str, target: &ConsoleTarget) -> String {
    let base = format!("https://{region}.console.aws.amazon.com");
    match target {
        ConsoleTarget::LogGroup { name } => format!(
            "{base}/cloudwatch/home?region={region}#logsV2:log-groups/log-group/{}",
            cw_encode(name)
        ),
        ConsoleTarget::SqsQueue { url } => format!(
            "{base}/sqs/v3/home?region={region}#/queues/{}",
            pct_encode(url)
        ),
        ConsoleTarget::StateMachine { arn } => format!(
            "{base}/states/home?region={region}#/statemachines/view/{}",
            pct_encode(arn)
        ),
        ConsoleTarget::Execution { arn } => format!(
            "{base}/states/home?region={region}#/executions/details/{}",
            pct_encode(arn)
        ),
        ConsoleTarget::EventBus { name } => format!(
            "{base}/events/home?region={region}#/eventbus/{}",
            pct_encode(name)
        ),
        ConsoleTarget::Rule { event_bus, name } => format!(
            "{base}/events/home?region={region}#/eventbus/{}/rules/{}",
            pct_encode(event_bus),
            pct_encode(name)
        ),
    }
}

/// Percent-encoding RFC 3986 (deja sin tocar los *unreserved*: ALPHA/DIGIT/`-._~`).
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Codificación del fragment de la consola de CloudWatch Logs: como `pct_encode` pero
/// con `%` → `$25` (así `/` queda `$252F`, que la consola decodifica).
fn cw_encode(s: &str) -> String {
    pct_encode(s).replace('%', "$25")
}

// --- SDK real -----------------------------------------------------------------

/// Log groups del ambiente. Sin búsqueda (`query=None`) trae UNA página acotada (≤50,
/// 1 round-trip → rápido a cualquier escala). Con búsqueda hace **fan-out**: como
/// `logGroupNamePattern` es substring **case-sensitive**, dispara una variante de casing
/// por request en paralelo (`case_variants`), mergea dedup por nombre y OR-ea el `more`.
/// La vista rankea con fuzzy local lo devuelto. Cubre diferencias de casing comunes
/// (primera letra, ALL-CAPS, kebab); NO CamelCase interno desde minúsculas.
async fn fetch_log_groups(
    ctx: &AwsContext,
    query: Option<&str>,
) -> color_eyre::Result<(Vec<LogGroupDto>, bool)> {
    let Some(q) = query else {
        return fetch_log_groups_page(ctx, None).await;
    };
    let variants = case_variants(q);
    let results =
        futures::future::join_all(variants.iter().map(|v| fetch_log_groups_page(ctx, Some(v))))
            .await;

    let mut seen = std::collections::HashSet::new();
    let mut groups: Vec<LogGroupDto> = Vec::new();
    let mut more = false;
    let mut last_err = None;
    let mut any_ok = false;
    for r in results {
        match r {
            Ok((page, page_more)) => {
                any_ok = true;
                more |= page_more;
                for g in page {
                    if seen.insert(g.name.clone()) {
                        groups.push(g);
                    }
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    // Best-effort: si alguna variante respondió, usamos lo que llegó; solo si TODAS
    // fallaron propagamos el error (no dejar la búsqueda en blanco por un fallo parcial).
    match last_err {
        Some(e) if !any_ok => Err(e),
        _ => Ok((groups, more)),
    }
}

/// Una página acotada (≤50) de log groups. Con `pattern`, busca server-side por
/// subcadena (`logGroupNamePattern`, infix, case-sensitive — NO lowercasear). Un solo
/// round-trip; `more=true` si el servidor tiene más (`next_token`). Nota: con pattern el
/// SDK no devuelve `storedBytes`.
async fn fetch_log_groups_page(
    ctx: &AwsContext,
    pattern: Option<&str>,
) -> color_eyre::Result<(Vec<LogGroupDto>, bool)> {
    let client = ctx.logs().await;
    let mut req = client.describe_log_groups().limit(50);
    if let Some(p) = pattern {
        req = req.log_group_name_pattern(p);
    }
    let out = req.send().await?;
    let groups = out
        .log_groups()
        .iter()
        .map(|g| LogGroupDto {
            name: g.log_group_name().unwrap_or_default().to_string(),
            stored_bytes: g.stored_bytes(),
            arn: g.arn().map(str::to_string),
        })
        .collect();
    Ok((groups, out.next_token().is_some()))
}

/// Tope de páginas de streams. Un group puede tener decenas de miles de streams;
/// `into_paginator().items()` los drenaba TODOS (segundos de bloqueo). Acotamos a
/// `MAX_STREAM_PAGES × 50` (los más recientes primero) y señalamos `· parcial`.
const MAX_STREAM_PAGES: usize = 10;

async fn fetch_log_streams(
    ctx: &AwsContext,
    group: &str,
) -> color_eyre::Result<(Vec<LogStreamDto>, bool)> {
    use aws_sdk_cloudwatchlogs::types::OrderBy;

    let client = ctx.logs().await;
    let mut streams = Vec::new();
    let mut next: Option<String> = None;
    let mut more = false;

    for page in 0..MAX_STREAM_PAGES {
        let out = client
            .describe_log_streams()
            .log_group_name(group)
            .order_by(OrderBy::LastEventTime)
            .descending(true)
            .limit(50)
            .set_next_token(next)
            .send()
            .await?;
        for s in out.log_streams() {
            streams.push(LogStreamDto {
                name: s.log_stream_name().unwrap_or_default().to_string(),
                last_event_ts: s.last_event_timestamp(),
            });
        }
        next = out.next_token().map(str::to_string);
        match &next {
            Some(_) if page == MAX_STREAM_PAGES - 1 => more = true,
            Some(_) => {}
            None => break,
        }
    }
    Ok((streams, more))
}

/// Tope de líneas por carga. `get_log_events` con `start_from_head(false)` trae las
/// más recientes.
const EVENTS_LIMIT: i32 = 200;
/// Tope de páginas a seguir hacia atrás en `get_log_events`. La API puede devolver una
/// **página vacía aunque el stream tenga eventos** (hay que seguir `nextBackwardToken`);
/// también la usamos para juntar hasta `EVENTS_LIMIT` líneas si la primera página es corta.
const MAX_EVENT_PAGES: usize = 5;
/// Tope de eventos por página de `filter_log_events`.
const TAIL_LIMIT: i32 = 1000;
/// Tope de páginas que `fetch_log_tail` junta por request (auto-paginación). Lo que
/// quede más allá se reporta vía `next_token` y el usuario lo trae con `o` (load-more).
const MAX_TAIL_PAGES: usize = 10;
/// Tope por mensaje: las líneas de log pueden ser enormes (stack traces, JSON); no
/// pasamos cientos de KB a la vista, pero sí lo suficiente para expandir y leer el
/// evento completo (`enter` abre el detalle). Se conservan los saltos de línea (el
/// detalle los muestra; la lista los colapsa al render).
const MAX_LINE: usize = 16 * 1024;

/// Epoch en millis (UTC) ahora. La vista nunca ve relojes: effects acota la ventana.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Recorta un mensaje de log a `MAX_LINE` (preservando saltos de línea para el panel
/// de detalle). La fila de la lista los colapsa a espacio al render.
fn clip_message(raw: Option<&str>) -> String {
    let s = raw.unwrap_or_default();
    if s.len() <= MAX_LINE {
        return s.to_string();
    }
    let mut end = MAX_LINE;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Eventos recientes de un stream (`get_log_events`, hasta `EVENTS_LIMIT`). Orden
/// cronológico ascendente (newest abajo); devuelve `(events, hay_más_viejas)`.
///
/// Con `start_from_head(false)` la primera página son los eventos más recientes, y
/// `nextBackwardToken` retrocede en el tiempo. Pero la API **puede devolver una página
/// vacía aunque el stream tenga eventos** (documentado): por eso seguimos el token
/// hacia atrás hasta juntar algo / llenar `EVENTS_LIMIT` / agotar (el token deja de
/// cambiar) / topar `MAX_EVENT_PAGES`. Cada página es más vieja → se antepone.
async fn fetch_log_events(
    ctx: &AwsContext,
    group: &str,
    stream: &str,
    start_token: Option<String>,
) -> color_eyre::Result<(Vec<LogEventDto>, Option<String>)> {
    let client = ctx.logs().await;
    // `start_token=None` arranca en los más recientes; con token (load-more) continúa
    // hacia atrás desde ahí (líneas más viejas).
    let mut token: Option<String> = start_token;
    let mut collected: Vec<LogEventDto> = Vec::new();

    for _ in 0..MAX_EVENT_PAGES {
        let out = client
            .get_log_events()
            .log_group_name(group)
            .log_stream_name(stream)
            .limit(EVENTS_LIMIT)
            .start_from_head(false)
            .set_next_token(token.clone())
            .send()
            .await?;
        let page: Vec<LogEventDto> = out
            .events()
            .iter()
            .map(|e| LogEventDto {
                ts: e.timestamp(),
                message: clip_message(e.message()),
                stream: None,
            })
            .collect();
        let next = out.next_backward_token().map(str::to_string);
        // Token que no cambia (o ausente) = no hay más eventos hacia atrás.
        let exhausted = next.is_none() || next == token;

        // La página recién traída es más vieja que lo ya juntado → va delante.
        if !page.is_empty() {
            let mut newer = std::mem::replace(&mut collected, page);
            collected.append(&mut newer);
        }
        token = next;

        if collected.len() as i32 >= EVENTS_LIMIT || exhausted {
            // `token` apunta justo más atrás de lo recién traído: continuación para `o`.
            let next = (!exhausted).then(|| token.clone()).flatten();
            collected.truncate(EVENTS_LIMIT as usize);
            return Ok((collected, next));
        }
    }

    // Topamos el cap de páginas con algo juntado: probablemente hay más hacia atrás.
    let next = (!collected.is_empty()).then(|| token.clone()).flatten();
    collected.truncate(EVENTS_LIMIT as usize);
    Ok((collected, next))
}

/// Logs de un group por rango de tiempo (`filter_log_events` sobre todos sus streams).
/// Traduce `window` → `start_time`/`end_time` (con `now_millis`), filtra server-side con
/// `pattern` y auto-pagina (`set_next_token`) hasta `MAX_TAIL_PAGES`. Los eventos vienen
/// en orden cronológico ascendente (oldest→newest); `next_token` = continuación (la usa
/// `o` para cargar más). Con `token`, continúa desde ahí. Devuelve `(events, next_token)`.
async fn fetch_log_tail(
    ctx: &AwsContext,
    group: &str,
    pattern: Option<&str>,
    window: LogWindow,
    token: Option<String>,
) -> color_eyre::Result<(Vec<LogEventDto>, Option<String>)> {
    let client = ctx.logs().await;
    let now = now_millis();
    let (start, end) = match window {
        LogWindow::Last(n) => (now - n, now),
        LogWindow::Range { from, to } => (from, to.unwrap_or(now)),
    };

    let mut next = token;
    let mut events: Vec<LogEventDto> = Vec::new();
    for _ in 0..MAX_TAIL_PAGES {
        let out = client
            .filter_log_events()
            .log_group_name(group)
            .set_filter_pattern(pattern.map(str::to_string))
            .start_time(start)
            .end_time(end)
            .limit(TAIL_LIMIT)
            .set_next_token(next.clone())
            .send()
            .await?;
        events.extend(out.events().iter().map(|e| LogEventDto {
            ts: e.timestamp(),
            message: clip_message(e.message()),
            stream: e.log_stream_name().map(str::to_string),
        }));
        next = out.next_token().map(str::to_string);
        if next.is_none() {
            break;
        }
    }
    Ok((events, next))
}

async fn fetch_queues(ctx: &AwsContext) -> color_eyre::Result<Vec<QueueDto>> {
    let client = ctx.sqs().await;
    let mut pages = client.list_queues().into_paginator().items().send();

    let mut queues = Vec::new();
    while let Some(item) = pages.next().await {
        let url = item?;
        let name = url.rsplit('/').next().unwrap_or(&url).to_string();
        let is_fifo = name.ends_with(".fifo");
        queues.push(QueueDto { name, url, is_fifo });
    }
    Ok(queues)
}

async fn fetch_queue_detail(
    ctx: &AwsContext,
    queue_url: &str,
) -> color_eyre::Result<(QueueAttrsDto, Vec<QueueMessageDto>)> {
    use aws_sdk_sqs::types::{MessageSystemAttributeName, QueueAttributeName};

    let client = ctx.sqs().await;

    // Attributes.
    let out = client
        .get_queue_attributes()
        .queue_url(queue_url)
        .attribute_names(QueueAttributeName::All)
        .send()
        .await?;
    let map = out.attributes();
    let get = |k: &QueueAttributeName| map.and_then(|m| m.get(k)).map(String::as_str);
    let int = |k: &QueueAttributeName| get(k).and_then(|s| s.parse::<i64>().ok());

    let (dlq_target_arn, max_receive_count) =
        parse_redrive(get(&QueueAttributeName::RedrivePolicy));

    // ¿Esta cola ES un DLQ? `ListDeadLetterSourceQueues` lista las colas que la usan
    // como su DLQ; ≥1 ⇒ el redrive (`d`) tiene sentido. Best-effort: un gap de permiso
    // en esta llamada NO debe romper el detalle (se trata como "no es DLQ").
    let dlq_sources = client
        .list_dead_letter_source_queues()
        .queue_url(queue_url)
        .send()
        .await
        .map(|o| o.queue_urls().to_vec())
        .unwrap_or_default();

    let attrs = QueueAttrsDto {
        visible: int(&QueueAttributeName::ApproximateNumberOfMessages),
        in_flight: int(&QueueAttributeName::ApproximateNumberOfMessagesNotVisible),
        delayed: int(&QueueAttributeName::ApproximateNumberOfMessagesDelayed),
        arn: get(&QueueAttributeName::QueueArn).map(str::to_string),
        dlq_target_arn,
        max_receive_count,
        dlq_sources,
    };

    // Peek: receive con visibility_timeout(0) (best-effort; incrementa receive_count).
    let recv = client
        .receive_message()
        .queue_url(queue_url)
        .max_number_of_messages(10)
        .visibility_timeout(0)
        .message_system_attribute_names(MessageSystemAttributeName::SentTimestamp)
        .message_system_attribute_names(MessageSystemAttributeName::ApproximateReceiveCount)
        .send()
        .await?;

    let messages = recv
        .messages()
        .iter()
        .map(|m| {
            let sys = |k: &MessageSystemAttributeName| {
                m.attributes()
                    .and_then(|a| a.get(k))
                    .and_then(|s| s.parse::<i64>().ok())
            };
            QueueMessageDto {
                id: m.message_id().unwrap_or("—").to_string(),
                body: m.body().unwrap_or_default().to_string(),
                sent_ts: sys(&MessageSystemAttributeName::SentTimestamp),
                receive_count: sys(&MessageSystemAttributeName::ApproximateReceiveCount),
            }
        })
        .collect();

    Ok((attrs, messages))
}

async fn purge_queue_real(ctx: &AwsContext, queue_url: &str) -> color_eyre::Result<()> {
    ctx.sqs()
        .await
        .purge_queue()
        .queue_url(queue_url)
        .send()
        .await?;
    Ok(())
}

/// Inicia el redrive de un DLQ: `StartMessageMoveTask` requiere el ARN de la cola
/// (`source_arn`), no su URL; lo resolvemos con `GetQueueAttributes(QueueArn)`. Sin
/// `destination_arn`, AWS devuelve los mensajes a sus colas origen.
async fn redrive_dlq_real(ctx: &AwsContext, queue_url: &str) -> color_eyre::Result<()> {
    use aws_sdk_sqs::types::QueueAttributeName;
    let client = ctx.sqs().await;
    let out = client
        .get_queue_attributes()
        .queue_url(queue_url)
        .attribute_names(QueueAttributeName::QueueArn)
        .send()
        .await?;
    let source_arn = out
        .attributes()
        .and_then(|m| m.get(&QueueAttributeName::QueueArn))
        .ok_or_else(|| color_eyre::eyre::eyre!("no se pudo resolver el ARN de la cola"))?;
    client
        .start_message_move_task()
        .source_arn(source_arn)
        .send()
        .await?;
    Ok(())
}

/// Extrae `(deadLetterTargetArn, maxReceiveCount)` del JSON de `RedrivePolicy`.
/// `maxReceiveCount` puede venir como número o como string según la cuenta.
fn parse_redrive(policy: Option<&str>) -> (Option<String>, Option<i64>) {
    let Some(p) = policy else {
        return (None, None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(p) else {
        return (None, None);
    };
    let arn = v
        .get("deadLetterTargetArn")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let max = v.get("maxReceiveCount").and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok()))
    });
    (arn, max)
}

// --- Step Functions: SDK real -------------------------------------------------

/// `aws_smithy_types::DateTime` → epoch millis. La vista nunca ve `DateTime`:
/// effects siempre aplana a `Option<i64>`.
fn dt_millis(dt: &aws_sdk_sfn::primitives::DateTime) -> Option<i64> {
    (*dt).to_millis().ok()
}

fn opt_dt_millis(dt: Option<&aws_sdk_sfn::primitives::DateTime>) -> Option<i64> {
    dt.and_then(|d| (*d).to_millis().ok())
}

/// Status del SDK (`#[non_exhaustive]`) → enum propio plano. Desconocido → Running
/// (no-redrivable, no "rojo terminal"): nunca panickea.
fn exec_status(s: &aws_sdk_sfn::types::ExecutionStatus) -> ExecStatus {
    use aws_sdk_sfn::types::ExecutionStatus as S;
    match s {
        S::Running => ExecStatus::Running,
        S::Succeeded => ExecStatus::Succeeded,
        S::Failed => ExecStatus::Failed,
        S::TimedOut => ExecStatus::TimedOut,
        S::Aborted => ExecStatus::Aborted,
        S::PendingRedrive => ExecStatus::PendingRedrive,
        _ => ExecStatus::Running,
    }
}

fn machine_type(t: &aws_sdk_sfn::types::StateMachineType) -> MachineType {
    use aws_sdk_sfn::types::StateMachineType as T;
    match t {
        T::Express => MachineType::Express,
        _ => MachineType::Standard,
    }
}

/// Pretty-print de un payload JSON (input/output), con fallback al raw si no
/// parsea, truncado para no pasar cientos de KB a la vista.
fn pretty_truncate(raw: Option<&str>) -> Option<String> {
    const MAX_PAYLOAD: usize = 4096;
    let raw = raw?;
    let pretty = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| raw.to_string());
    if pretty.len() <= MAX_PAYLOAD {
        return Some(pretty);
    }
    let mut end = MAX_PAYLOAD;
    while !pretty.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}\n… (truncado)", &pretty[..end]))
}

/// Tope de páginas al listar state machines: sin filtro server-side, las máquinas
/// que queden fuera serían inalcanzables, así que las traemos todas (son pocas por
/// cuenta). El tope evita colgarse en una cuenta patológica; si se alcanza con más
/// pendientes, se reporta `more = true` (la vista muestra `· parcial`).
const MAX_MACHINE_PAGES: usize = 20;

async fn fetch_state_machines(
    ctx: &AwsContext,
) -> color_eyre::Result<(Vec<StateMachineDto>, bool)> {
    let client = ctx.sfn().await;
    let mut machines: Vec<StateMachineDto> = Vec::new();
    let mut next: Option<String> = None;
    let mut more = false;
    for page in 0..MAX_MACHINE_PAGES {
        let out = client
            .list_state_machines()
            .max_results(50)
            .set_next_token(next)
            .send()
            .await?;
        machines.extend(out.state_machines().iter().map(|m| StateMachineDto {
            arn: m.state_machine_arn().to_string(),
            name: m.name().to_string(),
            machine_type: machine_type(m.r#type()),
            created_ts: dt_millis(m.creation_date()),
        }));
        next = out.next_token().map(str::to_string);
        if next.is_none() {
            break;
        }
        // Última iteración con token aún presente: hay más de las que trajimos.
        if page == MAX_MACHINE_PAGES - 1 {
            more = true;
        }
    }
    Ok((machines, more))
}

async fn fetch_executions(
    ctx: &AwsContext,
    machine_arn: &str,
    status: Option<ExecStatus>,
    token: Option<String>,
) -> color_eyre::Result<(Vec<ExecutionDto>, Option<String>)> {
    let out = ctx
        .sfn()
        .await
        .list_executions()
        .state_machine_arn(machine_arn)
        .max_results(50)
        .set_status_filter(status.and_then(exec_status_filter))
        .set_next_token(token)
        .send()
        .await?;
    let executions = out
        .executions()
        .iter()
        .map(|e| ExecutionDto {
            arn: e.execution_arn().to_string(),
            name: e.name().to_string(),
            status: exec_status(e.status()),
            start_ts: dt_millis(e.start_date()),
            stop_ts: opt_dt_millis(e.stop_date()),
        })
        .collect();
    Ok((executions, out.next_token().map(str::to_string)))
}

/// `ExecStatus` propio → el `ExecutionStatus` del SDK para `status_filter`. `None` para
/// `PendingRedrive` (no es un valor de filtro del API).
fn exec_status_filter(status: ExecStatus) -> Option<aws_sdk_sfn::types::ExecutionStatus> {
    use aws_sdk_sfn::types::ExecutionStatus as S;
    Some(match status {
        ExecStatus::Running => S::Running,
        ExecStatus::Succeeded => S::Succeeded,
        ExecStatus::Failed => S::Failed,
        ExecStatus::TimedOut => S::TimedOut,
        ExecStatus::Aborted => S::Aborted,
        ExecStatus::PendingRedrive => return None,
    })
}

/// Combina `describe_execution` (status/tiempos/input/output/error) +
/// `get_execution_history` (timeline) en una sola task, como `fetch_queue_detail`.
/// Tope de páginas del history por defecto (≈10k eventos). Antes era una sola llamada
/// de `max_results(1000)`: un history más largo (Map/Parallel grandes) perdía eventos
/// en silencio. Acotamos y señalamos `· parcial` si queda `next_token`; el load-more
/// (`o` en Detail) re-pide con un `page_budget` mayor (ver `load_more_execution_history`).
const MAX_HISTORY_PAGES: usize = 10;

/// `page_budget` = cuántas páginas de `get_execution_history` traer (cada una ≤1000
/// eventos). El load-more re-fetchea con un presupuesto creciente y re-parsea TODO el
/// prefijo (siempre contiguo desde el inicio cronológico): `parse_history` empareja
/// StateEntered/StateExited con una pila por nombre, así que acumular solo páginas
/// nuevas rompería el emparejamiento en el borde. Re-parsear el set completo es la
/// única forma correcta sin guardar eventos crudos del SDK (effects es stateless).
async fn fetch_execution_detail(
    ctx: &AwsContext,
    execution_arn: &str,
    page_budget: usize,
) -> color_eyre::Result<(ExecutionDetailDto, Vec<StateSpanDto>, Option<String>, bool)> {
    let client = ctx.sfn().await;

    let d = client
        .describe_execution()
        .execution_arn(execution_arn)
        .send()
        .await?;
    let detail = ExecutionDetailDto {
        status: exec_status(d.status()),
        start_ts: dt_millis(d.start_date()),
        stop_ts: opt_dt_millis(d.stop_date()),
        input: pretty_truncate(d.input()),
        output: pretty_truncate(d.output()),
        error: d.error().map(str::to_string),
        cause: d.cause().map(str::to_string),
        redrive_count: d.redrive_count().map(i64::from),
    };

    // `reverse_order(true)` trae los eventos más recientes primero: garantiza que
    // el evento de fallo (que ocurre al FINAL del timeline) entre en la primera
    // página aunque el history supere los 1000 eventos. Acumulamos hasta
    // `page_budget` y luego revertimos a orden cronológico para `parse_history`.
    let mut events = Vec::new();
    let mut next: Option<String> = None;
    let mut more = false;
    for page in 0..page_budget {
        let hist = client
            .get_execution_history()
            .execution_arn(execution_arn)
            .max_results(1000)
            .reverse_order(true)
            .set_next_token(next)
            .send()
            .await?;
        events.extend(hist.events().iter().cloned());
        next = hist.next_token().map(str::to_string);
        match &next {
            Some(_) if page == page_budget - 1 => more = true,
            Some(_) => {}
            None => break,
        }
    }
    events.reverse();
    let (history, failed_state) = parse_history(&events);

    Ok((detail, history, failed_state, more))
}

async fn redrive_execution_real(ctx: &AwsContext, execution_arn: &str) -> color_eyre::Result<()> {
    ctx.sfn()
        .await
        .redrive_execution()
        .execution_arn(execution_arn)
        .send()
        .await?;
    Ok(())
}

// --- EventBridge: SDK real ----------------------------------------------------

/// Tope de páginas al listar buses/rules: sin filtro server-side, lo que quede
/// fuera sería inalcanzable por el fuzzy, así que paginamos todo (son pocos por
/// cuenta). Si se topa con más pendientes, `more = true` (la vista muestra `· parcial`).
const MAX_EVENTS_PAGES: usize = 20;

/// `RuleState` del SDK (`#[non_exhaustive]`) → enum propio plano. Cualquier estado
/// no `Disabled` cuenta como habilitado (incluye `EnabledWithAllCloudTrail…`).
fn rule_state(s: Option<&aws_sdk_eventbridge::types::RuleState>) -> RuleState {
    use aws_sdk_eventbridge::types::RuleState as S;
    match s {
        Some(S::Disabled) => RuleState::Disabled,
        _ => RuleState::Enabled,
    }
}

async fn fetch_event_buses(ctx: &AwsContext) -> color_eyre::Result<(Vec<EventBusDto>, bool)> {
    let client = ctx.eventbridge().await;
    let mut buses: Vec<EventBusDto> = Vec::new();
    let mut next: Option<String> = None;
    let mut more = false;
    for page in 0..MAX_EVENTS_PAGES {
        let out = client
            .list_event_buses()
            .limit(100)
            .set_next_token(next)
            .send()
            .await?;
        buses.extend(out.event_buses().iter().map(|b| EventBusDto {
            arn: b.arn().unwrap_or_default().to_string(),
            name: b.name().unwrap_or_default().to_string(),
        }));
        next = out.next_token().map(str::to_string);
        if next.is_none() {
            break;
        }
        if page == MAX_EVENTS_PAGES - 1 {
            more = true;
        }
    }
    Ok((buses, more))
}

async fn fetch_rules(
    ctx: &AwsContext,
    event_bus_name: &str,
) -> color_eyre::Result<(Vec<RuleDto>, bool)> {
    let client = ctx.eventbridge().await;
    let mut rules: Vec<RuleDto> = Vec::new();
    let mut next: Option<String> = None;
    let mut more = false;
    for page in 0..MAX_EVENTS_PAGES {
        let out = client
            .list_rules()
            .event_bus_name(event_bus_name)
            .limit(100)
            .set_next_token(next)
            .send()
            .await?;
        rules.extend(out.rules().iter().map(|r| RuleDto {
            name: r.name().unwrap_or_default().to_string(),
            event_bus_name: event_bus_name.to_string(),
            state: rule_state(r.state()),
            description: r.description().map(str::to_string),
        }));
        next = out.next_token().map(str::to_string);
        if next.is_none() {
            break;
        }
        if page == MAX_EVENTS_PAGES - 1 {
            more = true;
        }
    }
    Ok((rules, more))
}

/// Combina `describe_rule` (patrón/estado/descr/schedule) + `list_targets_by_rule`
/// en una sola task (como `fetch_execution_detail`). Targets ≤5 por regla → una llamada.
async fn fetch_rule_detail(
    ctx: &AwsContext,
    event_bus_name: &str,
    rule_name: &str,
) -> color_eyre::Result<(RuleDetailDto, Vec<TargetDto>)> {
    let client = ctx.eventbridge().await;

    let r = client
        .describe_rule()
        .name(rule_name)
        .event_bus_name(event_bus_name)
        .send()
        .await?;
    let detail = RuleDetailDto {
        state: rule_state(r.state()),
        description: r.description().map(str::to_string),
        event_pattern: pretty_truncate(r.event_pattern()),
        schedule_expression: r.schedule_expression().map(str::to_string),
    };

    let t = client
        .list_targets_by_rule()
        .rule(rule_name)
        .event_bus_name(event_bus_name)
        .send()
        .await?;
    let targets = t
        .targets()
        .iter()
        .map(|tg| TargetDto {
            id: tg.id().to_string(),
            arn: tg.arn().to_string(),
            input: pretty_truncate(tg.input()),
        })
        .collect();

    Ok((detail, targets))
}

/// Publica un evento con el payload que el usuario editó. PutEvents puede fallar
/// parcialmente (HTTP 200 con `failed_entry_count > 0`): se traduce a error.
async fn send_event_real(
    ctx: &AwsContext,
    event_bus_name: &str,
    source: &str,
    detail_type: &str,
    detail: &str,
    time: Option<i64>,
    resources: &[String],
) -> color_eyre::Result<()> {
    use aws_sdk_eventbridge::primitives::DateTime;
    use aws_sdk_eventbridge::types::PutEventsRequestEntry;
    let mut builder = PutEventsRequestEntry::builder()
        .source(source)
        .detail_type(detail_type)
        .detail(detail)
        .event_bus_name(event_bus_name);
    // Campos opcionales de PutEvents: timestamp del evento y ARNs de recursos.
    if let Some(ms) = time {
        builder = builder.time(DateTime::from_millis(ms));
    }
    if !resources.is_empty() {
        builder = builder.set_resources(Some(resources.to_vec()));
    }
    let entry = builder.build();
    let out = ctx
        .eventbridge()
        .await
        .put_events()
        .entries(entry)
        .send()
        .await?;
    if out.failed_entry_count() > 0 {
        let why = out
            .entries()
            .iter()
            .find_map(|e| e.error_code().or(e.error_message()))
            .unwrap_or("evento rechazado");
        return Err(color_eyre::eyre::eyre!("{why}"));
    }
    Ok(())
}

/// Empareja eventos `StateEntered`/`StateExited` (en orden cronológico) en spans con
/// duración y marca el estado que reventó. Pura → testeable sin red.
///
/// - **Pila por nombre** (`open[name]` es una pila LIFO de índices): tolera estados
///   homónimos abiertos a la vez (ramas Parallel / Map con concurrencia) sin huérfanos.
/// - **Salir limpia el fallo**: un estado que falló pero luego emitió `StateExited`
///   se recuperó (Retry/Catch) → se desmarca; solo queda `failed` el que no salió.
/// - **Fallo por tipo de evento** (`*Failed`/`*TimedOut`/`*Aborted`): cubre todos los
///   modos terminales (incluye TIMED_OUT y ABORTED, no solo `*Failed`).
fn parse_history(
    events: &[aws_sdk_sfn::types::HistoryEvent],
) -> (Vec<StateSpanDto>, Option<String>) {
    use std::collections::HashMap;

    let mut spans: Vec<StateSpanDto> = Vec::new();
    let mut open: HashMap<String, Vec<usize>> = HashMap::new();

    for ev in events {
        let ts = dt_millis(ev.timestamp());
        if let Some(d) = ev.state_entered_event_details() {
            let name = d.name().to_string();
            open.entry(name.clone()).or_default().push(spans.len());
            spans.push(StateSpanDto {
                name,
                entered_ts: ts,
                exited_ts: None,
                failed: false,
                input: pretty_truncate(d.input()),
                output: None,
                resource_arn: None,
            });
        } else if let Some(d) = ev.state_exited_event_details() {
            // Cierra el span homónimo más reciente (LIFO); salir = se recuperó.
            if let Some(idx) = open.get_mut(d.name()).and_then(Vec::pop) {
                spans[idx].exited_ts = ts;
                spans[idx].failed = false;
                spans[idx].output = pretty_truncate(d.output());
            }
        } else if is_failure_event(ev) {
            // El estado que reventó es el span abierto más reciente (innermost).
            if let Some(&idx) = open.values().flatten().max() {
                spans[idx].failed = true;
            }
        } else if let Some(arn) = lambda_resource(ev) {
            // Evento de scheduling de Lambda: lo cuelga del span abierto más reciente
            // (el estado que la invocó) para el cross-link `l`. Primero gana.
            if let Some(&idx) = open.values().flatten().max()
                && spans[idx].resource_arn.is_none()
            {
                spans[idx].resource_arn = Some(arn);
            }
        }
    }

    let failed_state = spans
        .iter()
        .rev()
        .find(|s| s.failed)
        .map(|s| s.name.clone());
    (spans, failed_state)
}

/// `true` para eventos terminales de fallo/timeout/abort. Detecta por el nombre del
/// tipo (`*Failed`/`*TimedOut`/`*Aborted`) para cubrir TODOS los modos del SDK
/// —incluidos los nuevos— sin enumerar cada `*_event_details`.
fn is_failure_event(ev: &aws_sdk_sfn::types::HistoryEvent) -> bool {
    let t = ev.r#type().as_str();
    t.contains("Failed") || t.contains("TimedOut") || t.contains("Aborted")
}

/// ARN/identidad de la Lambda de un evento de scheduling, si lo es. Cubre las dos
/// integraciones: **directa** (`LambdaFunctionScheduled` → `resource` = ARN de la
/// función) y **optimizada** `arn:aws:states:::lambda:invoke` (`TaskScheduled` con
/// `resourceType=lambda` → `FunctionName` en los `parameters` JSON). `None` para
/// cualquier otro evento (o si los parameters no traen `FunctionName`).
fn lambda_resource(ev: &aws_sdk_sfn::types::HistoryEvent) -> Option<String> {
    if let Some(d) = ev.lambda_function_scheduled_event_details() {
        return Some(d.resource().to_string());
    }
    if let Some(d) = ev.task_scheduled_event_details()
        && d.resource_type() == "lambda"
    {
        let params: serde_json::Value = serde_json::from_str(d.parameters()).ok()?;
        return params
            .get("FunctionName")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    None
}

// --- Mock (tests / sin red) ---------------------------------------------------

/// Log groups falsos del ambiente. Un par de nombres llevan el `profile` activo
/// para que un cambio de ambiente sea visible en la lista.
fn mock_log_groups(env: &Env, query: Option<&str>) -> Vec<LogGroupDto> {
    let names = [
        format!("/aws/lambda/{}-orders-api", env.profile),
        format!("/aws/lambda/{}-payments-worker", env.profile),
        // Nombre largo con prefijo: la búsqueda server-side (substring) debe encontrarlo
        // tecleando solo `CreateOrder` (sin el prefijo `orders-service-staging-`).
        "/aws/lambda/orders-service-staging-CreateOrderV3".to_string(),
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
        // Mimetiza el fan-out server-side: substring case-sensitive contra cada variante
        // de casing de la query (mock y real coinciden en cobertura).
        .filter(|name| query.is_none_or(|q| case_variants(q).iter().any(|v| name.contains(v))))
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

/// Eventos falsos de un stream, deterministas por `group`+`stream`. Orden ascendente
/// (newest al final). Incluye una línea `ERROR` y una `WARN` para ejercitar el color.
fn mock_log_events(group: &str, stream: &str) -> Vec<LogEventDto> {
    let seed: i64 = group.bytes().chain(stream.bytes()).map(i64::from).sum();
    let base_ts: i64 = 1_750_000_000_000; // ~2025, epoch millis
    let lines = [
        "START RequestId: 3f9a-1d2b Version: $LATEST",
        "INFO cold start: init 412ms",
        "INFO handler invoked: { \"orderId\": \"A-1001\" }",
        "INFO fetching order from dynamodb",
        "WARN retry 1/3: throttled by downstream",
        "INFO charge authorized: 4200 cents",
        "INFO publishing OrderProcessed event",
        "ERROR unhandled: NullPointer at process.js:42",
        "INFO emitting metrics: { \"latencyMs\": 318 }",
        "END RequestId: 3f9a-1d2b",
    ];
    lines
        .into_iter()
        .enumerate()
        .map(|(i, msg)| LogEventDto {
            // Eventos cada ~ (seed-derivado) segundos; ascendente.
            ts: Some(base_ts + (i as i64) * (1_000 + seed % 700)),
            message: msg.to_string(),
            stream: None,
        })
        .collect()
}

/// Tail falso de un group: líneas intercaladas de 3 streams (`filter_log_events` trae
/// el stream de cada evento). Con `pattern`, filtra por substring case-insensitive
/// (mimetiza el `filter_pattern` del server, que en real es más rico).
fn mock_log_tail(group: &str, pattern: Option<&str>) -> Vec<LogEventDto> {
    let base_ts: i64 = 1_750_000_000_000;
    let seed: i64 = group.bytes().map(i64::from).sum();
    let streams = [
        format!("2026/06/21/[$LATEST]{:08x}", seed as u32),
        format!("2026/06/21/[$LATEST]{:08x}", (seed as u32).wrapping_mul(7)),
        format!("2026/06/21/[$LATEST]{:08x}", (seed as u32).wrapping_mul(13)),
    ];
    let raw = [
        "INFO request accepted",
        "INFO validating payload",
        "WARN deprecated field 'legacyId' present",
        "ERROR downstream 502: payments-api unavailable",
        "INFO falling back to cache",
        "INFO response sent 200",
    ];
    let pat = pattern.map(str::to_lowercase);
    raw.into_iter()
        .enumerate()
        .filter(|(_, msg)| {
            pat.as_deref()
                .is_none_or(|p| msg.to_lowercase().contains(p))
        })
        .map(|(i, msg)| LogEventDto {
            ts: Some(base_ts + (i as i64) * 1_700),
            message: msg.to_string(),
            stream: Some(streams[i % streams.len()].clone()),
        })
        .collect()
}

/// Colas SQS falsas del ambiente. Incluye una `.fifo` y una `*-dlq`; un par llevan
/// el `profile` activo para que un cambio de ambiente sea visible.
fn mock_queues(env: &Env) -> Vec<QueueDto> {
    let names = [
        format!("{}-orders", env.profile),
        format!("{}-payments.fifo", env.profile),
        "notifications".to_string(),
        "checkout-events".to_string(),
        "image-processing".to_string(),
        "orders-dlq".to_string(),
    ];
    names
        .into_iter()
        .map(|name| {
            let is_fifo = name.ends_with(".fifo");
            let url = format!(
                "https://sqs.{}.amazonaws.com/000000000000/{}",
                env.region, name
            );
            QueueDto { name, url, is_fifo }
        })
        .collect()
}

/// Detalle falso (attributes + peek) de una cola, determinista por el URL. Las
/// colas que no son `*-dlq` traen una DLQ configurada (RedrivePolicy).
fn mock_queue_detail(url: &str) -> (QueueAttrsDto, Vec<QueueMessageDto>) {
    let name = url.rsplit('/').next().unwrap_or(url);
    let seed: i64 = url.bytes().map(i64::from).sum::<i64>();
    let visible = seed % 137;
    let is_dlq = name.contains("dlq");

    let attrs = QueueAttrsDto {
        visible: Some(visible),
        in_flight: Some(seed % 19),
        delayed: Some(0),
        arn: Some(format!("arn:aws:sqs:us-east-1:000000000000:{name}")),
        dlq_target_arn: (!is_dlq).then(|| format!("arn:aws:sqs:us-east-1:000000000000:{name}-dlq")),
        max_receive_count: (!is_dlq).then_some(5),
        // Una cola `*-dlq` ES un DLQ: tiene colas origen → redrive demoable offline.
        dlq_sources: if is_dlq {
            let src = name.strip_suffix("-dlq").unwrap_or(name);
            vec![format!(
                "https://sqs.us-east-1.amazonaws.com/000000000000/{src}"
            )]
        } else {
            Vec::new()
        },
    };

    let base_ts: i64 = 1_750_000_000_000;
    let messages = (0..visible.min(8))
        .map(|i| QueueMessageDto {
            id: format!("{name}-msg-{i}"),
            body: format!("{{\"event\":\"sample\",\"n\":{i},\"queue\":\"{name}\"}}"),
            sent_ts: Some(base_ts - i * 53_000),
            receive_count: Some((i % 3) + 1),
        })
        .collect();

    (attrs, messages)
}

/// State machines falsas del ambiente. Incluye al menos una STANDARD y una EXPRESS;
/// un par llevan el `profile` activo para que un cambio de ambiente sea visible.
fn mock_state_machines(env: &Env) -> Vec<StateMachineDto> {
    let specs: [(&str, MachineType); 4] = [
        ("order-saga", MachineType::Standard),
        ("payment-flow", MachineType::Standard),
        ("ingest-fast", MachineType::Express),
        ("nightly-report", MachineType::Standard),
    ];
    let base_ts: i64 = 1_740_000_000_000;
    specs
        .into_iter()
        .enumerate()
        .map(|(i, (name, machine_type))| {
            let full = format!("{}-{name}", env.profile);
            StateMachineDto {
                arn: format!(
                    "arn:aws:states:{}:000000000000:stateMachine:{full}",
                    env.region
                ),
                name: full,
                machine_type,
                created_ts: Some(base_ts - (i as i64) * 86_400_000),
            }
        })
        .collect()
}

/// Ejecuciones falsas de una máquina, deterministas por el arn. Incluye al menos
/// una FAILED y una RUNNING (sin `stop_ts`). Los `tag`s (`fail`/`run`/…) los lee
/// `mock_execution_detail` para producir un detalle coherente.
fn mock_executions(machine_arn: &str, status: Option<ExecStatus>) -> Vec<ExecutionDto> {
    let specs: [(&str, ExecStatus); 6] = [
        ("fail", ExecStatus::Failed),
        ("ok-1", ExecStatus::Succeeded),
        ("ok-2", ExecStatus::Succeeded),
        ("run", ExecStatus::Running),
        ("abort", ExecStatus::Aborted),
        ("timeout", ExecStatus::TimedOut),
    ];
    let base_ts: i64 = 1_750_000_000_000;
    specs
        .into_iter()
        .enumerate()
        // Espeja el `status_filter` server-side del SDK real.
        .filter(|(_, (_, s))| status.is_none_or(|want| *s == want))
        .map(|(i, (tag, status))| {
            let name = format!("exec-{tag}-{i}");
            let start = base_ts - (i as i64) * 3_600_000;
            let stop =
                (status != ExecStatus::Running).then_some(start + 45_000 + (i as i64) * 1_000);
            ExecutionDto {
                arn: format!("{machine_arn}:exec:{name}"),
                name,
                status,
                start_ts: Some(start),
                stop_ts: stop,
            }
        })
        .collect()
}

/// Detalle falso de una ejecución, determinista por el arn: si contiene un tag de
/// fallo (`fail`/`abort`/`timeout`) → FAILED con error/cause y un timeline donde
/// revienta `ProcessOrder`; `run` → RUNNING (último estado abierto); si no → SUCCEEDED.
fn mock_execution_detail(
    execution_arn: &str,
) -> (ExecutionDetailDto, Vec<StateSpanDto>, Option<String>) {
    let base: i64 = 1_750_000_000_000;
    let input = Some("{\n  \"orderId\": \"A-1001\",\n  \"amount\": 4200\n}".to_string());
    let span = |name: &str, from: i64, to: Option<i64>, failed: bool| StateSpanDto {
        name: name.to_string(),
        entered_ts: Some(base + from),
        exited_ts: to.map(|t| base + t),
        failed,
        // Input por estado (siempre); output solo si el estado salió (`enter` lo expande).
        input: Some(format!(
            "{{\n  \"state\": \"{name}\",\n  \"orderId\": \"A-1001\"\n}}"
        )),
        output: to.map(|_| format!("{{\n  \"state\": \"{name}\",\n  \"ok\": true\n}}")),
        // Los estados de tipo Lambda task llevan su ARN → habilita el cross-link `l`
        // en mock (p. ej. `ProcessOrder` → `/aws/lambda/ProcessOrder`).
        resource_arn: matches!(name, "Validate" | "ChargeCard" | "ProcessOrder")
            .then(|| format!("arn:aws:lambda:us-east-1:123456789012:function:{name}")),
    };

    if execution_arn.contains("fail")
        || execution_arn.contains("abort")
        || execution_arn.contains("timeout")
    {
        let detail = ExecutionDetailDto {
            status: ExecStatus::Failed,
            start_ts: Some(base),
            stop_ts: Some(base + 12_000),
            input,
            output: None,
            error: Some("States.TaskFailed".to_string()),
            cause: Some("ProcessOrder lambda lanzó: insufficient funds".to_string()),
            redrive_count: Some(0),
        };
        let history = vec![
            span("Validate", 0, Some(2_000), false),
            span("ChargeCard", 2_000, Some(5_000), false),
            span("ProcessOrder", 5_000, None, true),
        ];
        (detail, history, Some("ProcessOrder".to_string()))
    } else if execution_arn.contains("run") {
        let detail = ExecutionDetailDto {
            status: ExecStatus::Running,
            start_ts: Some(base),
            stop_ts: None,
            input,
            output: None,
            error: None,
            cause: None,
            redrive_count: Some(0),
        };
        let history = vec![
            span("Validate", 0, Some(2_000), false),
            span("ChargeCard", 2_000, None, false),
        ];
        (detail, history, None)
    } else {
        let detail = ExecutionDetailDto {
            status: ExecStatus::Succeeded,
            start_ts: Some(base),
            stop_ts: Some(base + 9_000),
            input,
            output: Some("{\n  \"status\": \"ok\"\n}".to_string()),
            error: None,
            cause: None,
            redrive_count: Some(0),
        };
        let history = vec![
            span("Validate", 0, Some(2_000), false),
            span("ChargeCard", 2_000, Some(5_000), false),
            span("ProcessOrder", 5_000, Some(9_000), false),
        ];
        (detail, history, None)
    }
}

/// Buses falsos del ambiente: `default` + dos que reflejan el `profile` activo
/// (para que un switch de ambiente sea visible).
fn mock_event_buses(env: &Env) -> Vec<EventBusDto> {
    ["default", "app-bus", "ingest-bus"]
        .into_iter()
        .map(|name| {
            let full = if name == "default" {
                "default".to_string()
            } else {
                format!("{}-{name}", env.profile)
            };
            EventBusDto {
                arn: format!(
                    "arn:aws:events:{}:000000000000:event-bus/{full}",
                    env.region
                ),
                name: full,
            }
        })
        .collect()
}

/// Rules falsas de un bus, deterministas. Incluye al menos una `Disabled` (su
/// nombre lleva `disabled`, que `mock_rule_detail` lee para el detalle coherente).
fn mock_rules(event_bus_name: &str) -> Vec<RuleDto> {
    let specs: [(&str, RuleState, &str); 4] = [
        (
            "orders-created",
            RuleState::Enabled,
            "Route OrderCreated to fulfillment",
        ),
        (
            "payments-failed",
            RuleState::Enabled,
            "Alert on PaymentFailed",
        ),
        (
            "nightly-disabled",
            RuleState::Disabled,
            "Nightly batch (apagada)",
        ),
        ("audit-all", RuleState::Enabled, "Archive every event"),
    ];
    specs
        .into_iter()
        .map(|(name, state, desc)| RuleDto {
            name: name.to_string(),
            event_bus_name: event_bus_name.to_string(),
            state,
            description: Some(desc.to_string()),
        })
        .collect()
}

/// Detalle falso de una rule: patrón JSON (pretty) + 1–2 targets. Si el nombre
/// lleva `disabled` → `Disabled`.
fn mock_rule_detail(event_bus_name: &str, rule_name: &str) -> (RuleDetailDto, Vec<TargetDto>) {
    let state = if rule_name.contains("disabled") {
        RuleState::Disabled
    } else {
        RuleState::Enabled
    };
    let pattern = pretty_truncate(Some(
        r#"{"source":["my.app"],"detail-type":["OrderCreated"]}"#,
    ));
    let detail = RuleDetailDto {
        state,
        description: Some(format!("Rule {rule_name} en {event_bus_name}")),
        event_pattern: pattern,
        schedule_expression: None,
    };
    let targets = vec![
        TargetDto {
            id: "target-lambda".to_string(),
            arn: "arn:aws:lambda:us-east-1:000000000000:function:fulfillment".to_string(),
            input: None,
        },
        TargetDto {
            id: "target-sqs".to_string(),
            arn: "arn:aws:sqs:us-east-1:000000000000:orders-dlq".to_string(),
            input: pretty_truncate(Some(r#"{"forwarded":true}"#)),
        },
    ];
    (detail, targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_log_groups_tag_epoch_and_reflect_profile() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(Action::LoadLogGroups { query: None }, 7);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 7, "el epoch se propaga al resultado");
        match envelope.message {
            Message::LogGroupsLoaded {
                groups,
                query,
                more,
            } => {
                assert!(query.is_none());
                assert!(!more);
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
    async fn mock_verify_identity_returns_canned_account() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(Action::VerifyIdentity, 3);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 3, "el epoch se propaga");
        match envelope.message {
            Message::IdentityLoaded { account_id } => assert_eq!(account_id, "123456789012"),
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[test]
    fn console_urls_encode_region_and_resource() {
        // CloudWatch: el `/` del log group queda como $252F en el fragment.
        let u = console_url(
            "us-east-1",
            &ConsoleTarget::LogGroup {
                name: "/aws/lambda/foo".into(),
            },
        );
        assert!(u.contains("us-east-1.console.aws.amazon.com/cloudwatch"));
        assert!(
            u.contains("$252Faws$252Flambda$252Ffoo"),
            "log group codificado: {u}"
        );

        // SFN: el ARN va percent-encoded (`:` → %3A).
        let u = console_url(
            "eu-west-1",
            &ConsoleTarget::Execution {
                arn: "arn:aws:states:eu-west-1:0:execution:m:e1".into(),
            },
        );
        assert!(u.contains("/states/home?region=eu-west-1"));
        assert!(u.contains("arn%3Aaws%3Astates"), "arn percent-encoded: {u}");

        // EventBridge rule.
        let u = console_url(
            "us-east-1",
            &ConsoleTarget::Rule {
                event_bus: "default".into(),
                name: "my-rule".into(),
            },
        );
        assert!(u.contains("/events/home"));
        assert!(u.contains("/eventbus/default/rules/my-rule"));
    }

    #[tokio::test]
    async fn mock_log_groups_search_filters_by_substring() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        // Búsqueda server-side por subcadena: `CreateOrder` (sin el prefijo) trae el
        // group de nombre largo, y el `query` se ecoa para el guard "latest wins".
        fx.dispatch(
            Action::LoadLogGroups {
                query: Some("CreateOrder".into()),
            },
            9,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 9);
        match envelope.message {
            Message::LogGroupsLoaded {
                groups,
                query,
                more,
            } => {
                assert_eq!(query.as_deref(), Some("CreateOrder"), "el query se ecoa");
                assert!(!more);
                assert!(
                    groups.iter().all(|g| g.name.contains("CreateOrder")),
                    "el mock filtra por subcadena como el server"
                );
                assert!(groups.iter().any(|g| g.name.contains("CreateOrderV3")));
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[test]
    fn mock_log_groups_search_tolerates_casing() {
        let env = Env::new("dev", "us-east-1");
        // Fan-out de variantes: `createOrder` (primera letra minúscula) ahora encuentra
        // `…CreateOrderV3` vía la variante `CreateOrder`.
        let hits = mock_log_groups(&env, Some("createOrder"));
        assert!(
            hits.iter().any(|g| g.name.contains("CreateOrderV3")),
            "createOrder debe encontrar CreateOrderV3: {hits:?}"
        );
        // Limitación documentada: todo en minúsculas NO reconstruye el CamelCase interno.
        let miss = mock_log_groups(&env, Some("createorder"));
        assert!(
            !miss.iter().any(|g| g.name.contains("CreateOrderV3")),
            "createorder (minúsculas) no reconstruye CreateOrder"
        );
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
            Message::LogStreamsLoaded {
                group,
                streams,
                more,
            } => {
                assert_eq!(group, "/ecs/checkout-service");
                assert!(!streams.is_empty());
                assert!(!more, "el mock no es parcial");
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_log_events_have_lines_and_epoch() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadLogEvents {
                group: "/ecs/checkout-service".into(),
                stream: "2026/06/21/[$LATEST]abc".into(),
                token: None,
            },
            4,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 4);
        match envelope.message {
            Message::LogEventsLoaded {
                group,
                stream,
                events,
                next_token,
                append,
            } => {
                assert_eq!(group, "/ecs/checkout-service");
                assert_eq!(stream, "2026/06/21/[$LATEST]abc");
                assert!(next_token.is_none());
                assert!(!append);
                assert!(!events.is_empty());
                assert!(
                    events.iter().all(|e| e.stream.is_none()),
                    "por-stream: sin stream"
                );
                assert!(
                    events.iter().any(|e| e.message.contains("ERROR")),
                    "incluye una línea de error"
                );
                // Orden ascendente (newest al final).
                assert!(
                    events.windows(2).all(|w| w[0].ts <= w[1].ts),
                    "los eventos vienen en orden cronológico"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    /// Helper: `LoadLogTail` con ventana de 1h y sin token, para los tests del mock.
    fn load_tail(
        group: &str,
        pattern: Option<&str>,
        token: Option<&str>,
        generation: u64,
    ) -> Action {
        Action::LoadLogTail {
            group: group.into(),
            pattern: pattern.map(str::to_string),
            window: LogWindow::Last(3_600_000),
            token: token.map(str::to_string),
            generation,
        }
    }

    #[tokio::test]
    async fn mock_log_tail_reflects_multiple_streams() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(load_tail("/ecs/checkout-service", None, None, 1), 6);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 6);
        match envelope.message {
            Message::LogTailLoaded {
                group,
                events,
                append,
                generation,
                ..
            } => {
                assert_eq!(group, "/ecs/checkout-service");
                assert!(!append, "carga fresca (sin token)");
                assert_eq!(generation, 1, "el generation se propaga");
                assert!(
                    events.iter().all(|e| e.stream.is_some()),
                    "tail trae el stream"
                );
                let distinct: std::collections::HashSet<_> =
                    events.iter().filter_map(|e| e.stream.clone()).collect();
                assert!(distinct.len() > 1, "el tail mezcla varios streams");
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_log_tail_filters_by_pattern() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            load_tail("/ecs/checkout-service", Some("error"), None, 2),
            8,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 8);
        match envelope.message {
            Message::LogTailLoaded { events, .. } => {
                assert!(!events.is_empty());
                assert!(
                    events
                        .iter()
                        .all(|e| e.message.to_lowercase().contains("error")),
                    "el mock filtra por substring como el server"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_log_tail_with_token_marks_append() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        // Token presente = continuación de una página previa (load-more).
        fx.dispatch(load_tail("/ecs/checkout-service", None, Some("tok"), 3), 1);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::LogTailLoaded {
                append, generation, ..
            } => {
                assert!(append, "con token, la respuesta es append (load-more)");
                assert_eq!(generation, 3);
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_queues_loaded_with_epoch_and_fifo() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(Action::LoadQueues, 5);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 5);
        match envelope.message {
            Message::QueuesLoaded(queues) => {
                assert!(!queues.is_empty());
                assert!(queues.iter().any(|q| q.is_fifo), "hay una cola .fifo");
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_queue_detail_has_dlq_for_normal_queue() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));
        let url = "https://sqs.us-east-1.amazonaws.com/000000000000/orders".to_string();

        fx.dispatch(
            Action::LoadQueueDetail {
                queue_url: url.clone(),
            },
            2,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 2);
        match envelope.message {
            Message::QueueDetailLoaded {
                queue_url, attrs, ..
            } => {
                assert_eq!(queue_url, url);
                assert!(attrs.has_dlq(), "una cola normal tiene DLQ configurada");
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_purge_replies_purged() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));
        let url = "https://sqs.us-east-1.amazonaws.com/000000000000/orders".to_string();

        fx.dispatch(
            Action::PurgeQueue {
                queue_url: url.clone(),
            },
            1,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 1);
        assert!(matches!(envelope.message, Message::QueuePurged { queue_url } if queue_url == url));
    }

    #[tokio::test]
    async fn mock_queue_detail_dlq_queue_has_sources() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));
        let url = "https://sqs.us-east-1.amazonaws.com/000000000000/orders-dlq".to_string();

        fx.dispatch(
            Action::LoadQueueDetail {
                queue_url: url.clone(),
            },
            2,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::QueueDetailLoaded { attrs, .. } => {
                assert!(attrs.is_dlq(), "un *-dlq ES un DLQ (tiene colas origen)");
                assert!(!attrs.has_dlq(), "un DLQ no apunta a otro DLQ");
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_redrive_dlq_replies_started() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));
        let url = "https://sqs.us-east-1.amazonaws.com/000000000000/orders-dlq".to_string();

        fx.dispatch(
            Action::RedriveDlq {
                queue_url: url.clone(),
            },
            7,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 7);
        assert!(
            matches!(envelope.message, Message::DlqRedriveStarted { queue_url } if queue_url == url)
        );
    }

    #[test]
    fn parse_redrive_extracts_dlq_and_count() {
        let policy = r#"{"deadLetterTargetArn":"arn:aws:sqs:us-east-1:000:orders-dlq","maxReceiveCount":"5"}"#;
        let (arn, max) = parse_redrive(Some(policy));
        assert_eq!(arn.as_deref(), Some("arn:aws:sqs:us-east-1:000:orders-dlq"));
        assert_eq!(max, Some(5));

        assert_eq!(parse_redrive(None), (None, None));
        // maxReceiveCount como número (no string).
        let (_, max_num) = parse_redrive(Some(r#"{"maxReceiveCount":3}"#));
        assert_eq!(max_num, Some(3));
    }

    // --- Step Functions -------------------------------------------------------

    #[tokio::test]
    async fn mock_state_machines_tag_epoch_and_include_express() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(Action::LoadStateMachines, 4);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 4);
        match envelope.message {
            Message::StateMachinesLoaded { machines, more } => {
                assert!(!more, "el mock cabe en una página");
                assert!(
                    machines
                        .iter()
                        .any(|m| m.machine_type == MachineType::Express)
                );
                assert!(
                    machines
                        .iter()
                        .any(|m| m.machine_type == MachineType::Standard)
                );
                assert!(
                    machines.iter().any(|m| m.name.contains("dev")),
                    "la data mock refleja el profile activo"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_executions_include_failed_and_running() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadExecutions {
                machine_arn: "arn:aws:states:us-east-1:000:stateMachine:dev-order-saga".into(),
                status: None,
                token: None,
            },
            6,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 6);
        match envelope.message {
            Message::ExecutionsLoaded {
                machine_arn,
                executions,
                next_token,
                append,
            } => {
                assert!(next_token.is_none(), "el mock cabe en una página");
                assert!(!append, "primera página no es append");
                assert!(machine_arn.contains("order-saga"));
                assert!(executions.iter().any(|e| e.status == ExecStatus::Failed));
                assert!(
                    executions
                        .iter()
                        .any(|e| e.status == ExecStatus::Running && e.stop_ts.is_none()),
                    "una RUNNING no tiene stop_ts"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_executions_status_filter_narrows() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));
        fx.dispatch(
            Action::LoadExecutions {
                machine_arn: "arn:aws:states:us-east-1:000:stateMachine:dev-order-saga".into(),
                status: Some(ExecStatus::Failed),
                token: None,
            },
            1,
        );
        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::ExecutionsLoaded { executions, .. } => {
                assert!(!executions.is_empty());
                assert!(
                    executions.iter().all(|e| e.status == ExecStatus::Failed),
                    "el filtro de estado solo trae FAILED"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_execution_detail_failed_has_error_and_failed_state() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadExecutionDetail {
                execution_arn: "arn:…:exec:exec-fail-0".into(),
            },
            2,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::ExecutionDetailLoaded {
                detail,
                history,
                failed_state,
                ..
            } => {
                assert_eq!(detail.status, ExecStatus::Failed);
                assert!(detail.error.is_some() && detail.cause.is_some());
                assert_eq!(failed_state.as_deref(), Some("ProcessOrder"));
                assert!(history.iter().any(|s| s.failed && s.name == "ProcessOrder"));
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_execution_detail_ok_is_clean() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadExecutionDetail {
                execution_arn: "arn:…:exec:exec-ok-1".into(),
            },
            2,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::ExecutionDetailLoaded {
                detail,
                failed_state,
                ..
            } => {
                assert_eq!(detail.status, ExecStatus::Succeeded);
                assert!(failed_state.is_none());
                assert!(detail.error.is_none());
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_redrive_replies_redriven() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));
        let arn = "arn:…:exec:exec-fail-0".to_string();

        fx.dispatch(
            Action::RedriveExecution {
                execution_arn: arn.clone(),
            },
            1,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 1);
        assert!(
            matches!(envelope.message, Message::ExecutionRedriven { execution_arn } if execution_arn == arn)
        );
    }

    // --- EventBridge ----------------------------------------------------------

    #[tokio::test]
    async fn mock_event_buses_tag_epoch_and_reflect_profile() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(Action::LoadEventBuses, 7);

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 7);
        match envelope.message {
            Message::EventBusesLoaded { buses, more } => {
                assert!(!more, "el mock cabe en una página");
                assert!(buses.iter().any(|b| b.name == "default"));
                assert!(
                    buses.iter().any(|b| b.name.contains("dev")),
                    "la data mock refleja el profile activo"
                );
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_rules_include_enabled_and_disabled() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadRules {
                event_bus_name: "default".into(),
            },
            3,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::RulesLoaded {
                event_bus_name,
                rules,
                more,
            } => {
                assert!(!more);
                assert_eq!(event_bus_name, "default");
                assert!(rules.iter().any(|r| r.state == RuleState::Enabled));
                assert!(rules.iter().any(|r| r.state == RuleState::Disabled));
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_rule_detail_has_pattern_and_targets() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadRuleDetail {
                event_bus_name: "default".into(),
                rule_name: "orders-created".into(),
            },
            4,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        match envelope.message {
            Message::RuleDetailLoaded {
                detail, targets, ..
            } => {
                assert!(detail.event_pattern.is_some());
                assert!(!targets.is_empty());
            }
            other => panic!("mensaje inesperado: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_send_event_replies_event_sent() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::SendEvent {
                event_bus_name: "default".into(),
                source: "awsdeck.manual".into(),
                detail_type: "test".into(),
                detail: "{}".into(),
                time: None,
                resources: vec![],
            },
            1,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 1);
        assert!(
            matches!(envelope.message, Message::EventSent { event_bus_name } if event_bus_name == "default")
        );
    }

    // Constructores de `HistoryEvent` para los tests de `parse_history`.
    fn ev_entered(id: i64, ts: i64, name: &str) -> aws_sdk_sfn::types::HistoryEvent {
        use aws_sdk_sfn::primitives::DateTime;
        use aws_sdk_sfn::types::{HistoryEvent, HistoryEventType as T, StateEnteredEventDetails};
        HistoryEvent::builder()
            .id(id)
            .timestamp(DateTime::from_millis(ts))
            .r#type(T::TaskStateEntered)
            .state_entered_event_details(
                StateEnteredEventDetails::builder()
                    .name(name)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }
    fn ev_exited(id: i64, ts: i64, name: &str) -> aws_sdk_sfn::types::HistoryEvent {
        use aws_sdk_sfn::primitives::DateTime;
        use aws_sdk_sfn::types::{HistoryEvent, HistoryEventType as T, StateExitedEventDetails};
        HistoryEvent::builder()
            .id(id)
            .timestamp(DateTime::from_millis(ts))
            .r#type(T::TaskStateExited)
            .state_exited_event_details(
                StateExitedEventDetails::builder()
                    .name(name)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }
    /// Evento de fallo/timeout/abort por tipo (sin detalles de estado): `is_failure_event`
    /// lo detecta por el nombre del tipo.
    fn ev_typed(
        id: i64,
        ts: i64,
        ty: aws_sdk_sfn::types::HistoryEventType,
    ) -> aws_sdk_sfn::types::HistoryEvent {
        use aws_sdk_sfn::primitives::DateTime;
        aws_sdk_sfn::types::HistoryEvent::builder()
            .id(id)
            .timestamp(DateTime::from_millis(ts))
            .r#type(ty)
            .build()
            .unwrap()
    }

    /// `LambdaFunctionScheduled` con `resource` = ARN de la función (integración directa).
    fn ev_lambda_scheduled(id: i64, ts: i64, resource: &str) -> aws_sdk_sfn::types::HistoryEvent {
        use aws_sdk_sfn::primitives::DateTime;
        use aws_sdk_sfn::types::{
            HistoryEvent, HistoryEventType as T, LambdaFunctionScheduledEventDetails,
        };
        HistoryEvent::builder()
            .id(id)
            .timestamp(DateTime::from_millis(ts))
            .r#type(T::LambdaFunctionScheduled)
            .lambda_function_scheduled_event_details(
                LambdaFunctionScheduledEventDetails::builder()
                    .resource(resource)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    /// `TaskScheduled` (integración optimizada): `resource_type` + `parameters` JSON.
    fn ev_task_scheduled(
        id: i64,
        ts: i64,
        resource_type: &str,
        parameters: &str,
    ) -> aws_sdk_sfn::types::HistoryEvent {
        use aws_sdk_sfn::primitives::DateTime;
        use aws_sdk_sfn::types::{HistoryEvent, HistoryEventType as T, TaskScheduledEventDetails};
        HistoryEvent::builder()
            .id(id)
            .timestamp(DateTime::from_millis(ts))
            .r#type(T::TaskScheduled)
            .task_scheduled_event_details(
                TaskScheduledEventDetails::builder()
                    .resource_type(resource_type)
                    .resource("invoke")
                    .region("us-east-1")
                    .parameters(parameters)
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()
    }

    #[test]
    fn parse_history_attaches_lambda_resource_per_state() {
        // Integración directa: el `LambdaFunctionScheduled` dentro del estado cuelga su
        // ARN del span abierto (Charge). La optimizada (`TaskScheduled` resourceType
        // lambda) saca `FunctionName` de los parameters (Process).
        let events = vec![
            ev_entered(1, 0, "Charge"),
            ev_lambda_scheduled(
                2,
                50,
                "arn:aws:lambda:us-east-1:111122223333:function:Charge",
            ),
            ev_exited(3, 100, "Charge"),
            ev_entered(4, 100, "Process"),
            ev_task_scheduled(5, 150, "lambda", "{\"FunctionName\":\"ProcessFn\"}"),
            ev_exited(6, 200, "Process"),
            // Estado sin Lambda: no debe ganar resource.
            ev_entered(7, 200, "Wait"),
            ev_exited(8, 250, "Wait"),
        ];
        let (spans, _) = parse_history(&events);
        let by = |n: &str| spans.iter().find(|s| s.name == n).unwrap();
        assert_eq!(
            by("Charge").resource_arn.as_deref(),
            Some("arn:aws:lambda:us-east-1:111122223333:function:Charge")
        );
        assert_eq!(by("Process").resource_arn.as_deref(), Some("ProcessFn"));
        assert_eq!(by("Wait").resource_arn, None);
    }

    #[test]
    fn parse_history_pairs_states_and_marks_failure() {
        use aws_sdk_sfn::types::HistoryEventType as T;
        let events = vec![
            ev_entered(1, 1_000, "A"),
            ev_exited(2, 1_500, "A"),
            ev_entered(3, 1_500, "B"),
            ev_typed(4, 1_800, T::ExecutionFailed),
        ];
        let (spans, failed_state) = parse_history(&events);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "A");
        assert_eq!(spans[0].exited_ts, Some(1_500));
        assert!(!spans[0].failed);
        assert_eq!(spans[1].name, "B");
        assert_eq!(spans[1].exited_ts, None);
        assert!(spans[1].failed, "el estado abierto al fallar se marca");
        assert_eq!(failed_state.as_deref(), Some("B"));
    }

    #[test]
    fn parse_history_detects_timeout_and_abort() {
        use aws_sdk_sfn::types::HistoryEventType as T;
        // TaskTimedOut (no es *Failed) debe marcar el estado abierto igual.
        let events = vec![ev_entered(1, 0, "Wait"), ev_typed(2, 500, T::TaskTimedOut)];
        let (spans, failed_state) = parse_history(&events);
        assert!(spans[0].failed, "TIMED_OUT marca el estado abierto");
        assert_eq!(failed_state.as_deref(), Some("Wait"));

        // ExecutionAborted también.
        let events = vec![
            ev_entered(1, 0, "Work"),
            ev_typed(2, 500, T::ExecutionAborted),
        ];
        let (spans, _) = parse_history(&events);
        assert!(spans[0].failed, "ABORTED marca el estado abierto");
    }

    #[test]
    fn parse_history_clears_failure_on_retry_recovery() {
        use aws_sdk_sfn::types::HistoryEventType as T;
        // Charge falla (Task) pero reintenta y SALE bien → no debe quedar marcado.
        let events = vec![
            ev_entered(1, 0, "Charge"),
            ev_typed(2, 100, T::TaskFailed),
            ev_exited(3, 300, "Charge"),
            ev_entered(4, 300, "Done"),
            ev_exited(5, 400, "Done"),
        ];
        let (spans, failed_state) = parse_history(&events);
        assert!(
            !spans.iter().any(|s| s.failed),
            "un estado que falló-reintentó-y-salió no queda marcado"
        );
        assert_eq!(failed_state, None);
        assert_eq!(spans[0].exited_ts, Some(300));
    }

    #[test]
    fn parse_history_handles_concurrent_homonyms() {
        // Dos "Worker" abiertos a la vez (Parallel): ambos deben cerrarse (sin huérfanos).
        let events = vec![
            ev_entered(1, 0, "Worker"),
            ev_entered(2, 0, "Worker"),
            ev_exited(3, 100, "Worker"),
            ev_exited(4, 200, "Worker"),
        ];
        let (spans, _) = parse_history(&events);
        assert_eq!(spans.len(), 2);
        assert!(
            spans.iter().all(|s| s.exited_ts.is_some()),
            "ningún span homónimo queda huérfano (pila LIFO)"
        );
    }
}
