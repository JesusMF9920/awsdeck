//! `effects` — el dispatcher y la ÚNICA frontera con el SDK. Recibe un `Action`,
//! hace `tokio::spawn` de una task contra el client correcto y manda un `Message`
//! de vuelta por el canal mpsc, etiquetado con el `epoch` del `Env` que lo lanzó.
//!
//! `Backend::Real` usa los `aws-sdk-*` (vía `AwsContext`); `Backend::Mock`
//! sirve para tests y desarrollo sin red. Como los `Message`/DTOs son los mismos,
//! ni las vistas ni `app.rs` distinguen entre uno y otro. Cualquier fallo del SDK
//! se reporta como `Message::Error` (lo pinta la status bar, nunca hace panic).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::action::Action;
use crate::aws::context::{AwsContext, Env};
use crate::message::{
    Envelope, ExecStatus, ExecutionDetailDto, ExecutionDto, LogGroupDto, LogStreamDto, MachineType,
    Message, QueueAttrsDto, QueueDto, QueueMessageDto, StateMachineDto, StateSpanDto,
};

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
            Action::LoadLogGroups { query } => self.load_log_groups(query, epoch),
            Action::LoadLogStreams { group } => self.load_log_streams(group, epoch),
            Action::LoadQueues => self.load_queues(epoch),
            Action::LoadQueueDetail { queue_url } => self.load_queue_detail(queue_url, epoch),
            Action::PurgeQueue { queue_url } => self.purge_queue(queue_url, epoch),
            Action::LoadStateMachines => self.load_state_machines(epoch),
            Action::LoadExecutions { machine_arn } => self.load_executions(machine_arn, epoch),
            Action::LoadExecutionDetail { execution_arn } => {
                self.load_execution_detail(execution_arn, epoch)
            }
            Action::RedriveExecution { execution_arn } => {
                self.redrive_execution(execution_arn, epoch)
            }
            Action::Quit | Action::ActivateView(_) | Action::Back | Action::SwitchEnv(_) => {}
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
                    let groups = mock_log_groups(&env, query.as_deref());
                    let msg = Message::LogGroupsLoaded {
                        groups,
                        query,
                        more: false,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_log_groups_page(&ctx, query.as_deref()).await {
                        Ok((groups, more)) => Message::LogGroupsLoaded {
                            groups,
                            query,
                            more,
                        },
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
                        Err(e) => Message::Error(format!("list_queues: {e}")),
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
                        Err(e) => Message::Error(format!("queue detail: {e}")),
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
                        Err(e) => Message::Error(format!("purge_queue: {e}")),
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
                    let msg = Message::StateMachinesLoaded(mock_state_machines(&env));
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_state_machines(&ctx).await {
                        Ok(machines) => Message::StateMachinesLoaded(machines),
                        Err(e) => Message::Error(format!("list_state_machines: {e}")),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }

    fn load_executions(&self, machine_arn: String, epoch: u64) {
        let tx = self.tx.clone();
        match &self.backend {
            Backend::Mock(_) => {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(450)).await;
                    let executions = mock_executions(&machine_arn);
                    let msg = Message::ExecutionsLoaded {
                        machine_arn,
                        executions,
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_executions(&ctx, &machine_arn).await {
                        Ok(executions) => Message::ExecutionsLoaded {
                            machine_arn,
                            executions,
                        },
                        Err(e) => Message::Error(format!("list_executions: {e}")),
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
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
            Backend::Real(ctx) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    let msg = match fetch_execution_detail(&ctx, &execution_arn).await {
                        Ok((detail, history, failed_state)) => Message::ExecutionDetailLoaded {
                            execution_arn,
                            detail,
                            history,
                            failed_state,
                        },
                        Err(e) => Message::Error(format!("execution detail: {e}")),
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
                        Err(e) => Message::Error(format!("redrive_execution: {e}")),
                    };
                    let _ = tx.send(Envelope::new(epoch, msg)).await;
                });
            }
        }
    }
}

// --- SDK real -----------------------------------------------------------------

/// Una página acotada (≤50) de log groups. Con `pattern`, busca server-side por
/// substring (`logGroupNamePattern`, case-sensitive — NO lowercasear). Devuelve
/// `(groups, hay_más)`. Nota: con pattern el SDK no devuelve `storedBytes`.
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
    let attrs = QueueAttrsDto {
        visible: int(&QueueAttributeName::ApproximateNumberOfMessages),
        in_flight: int(&QueueAttributeName::ApproximateNumberOfMessagesNotVisible),
        delayed: int(&QueueAttributeName::ApproximateNumberOfMessagesDelayed),
        arn: get(&QueueAttributeName::QueueArn).map(str::to_string),
        dlq_target_arn,
        max_receive_count,
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

async fn fetch_state_machines(ctx: &AwsContext) -> color_eyre::Result<Vec<StateMachineDto>> {
    let out = ctx
        .sfn()
        .await
        .list_state_machines()
        .max_results(50)
        .send()
        .await?;
    let machines = out
        .state_machines()
        .iter()
        .map(|m| StateMachineDto {
            arn: m.state_machine_arn().to_string(),
            name: m.name().to_string(),
            machine_type: machine_type(m.r#type()),
            created_ts: dt_millis(m.creation_date()),
        })
        .collect();
    Ok(machines)
}

async fn fetch_executions(
    ctx: &AwsContext,
    machine_arn: &str,
) -> color_eyre::Result<Vec<ExecutionDto>> {
    let out = ctx
        .sfn()
        .await
        .list_executions()
        .state_machine_arn(machine_arn)
        .max_results(50)
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
    Ok(executions)
}

/// Combina `describe_execution` (status/tiempos/input/output/error) +
/// `get_execution_history` (timeline) en una sola task, como `fetch_queue_detail`.
async fn fetch_execution_detail(
    ctx: &AwsContext,
    execution_arn: &str,
) -> color_eyre::Result<(ExecutionDetailDto, Vec<StateSpanDto>, Option<String>)> {
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

    let hist = client
        .get_execution_history()
        .execution_arn(execution_arn)
        .max_results(1000)
        .reverse_order(false)
        .send()
        .await?;
    let (history, failed_state) = parse_history(hist.events());

    Ok((detail, history, failed_state))
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

/// Empareja eventos `StateEntered`/`StateExited` (por nombre) en spans con duración
/// y marca el estado que reventó. Pura → testeable sin red. "Último entered gana"
/// si un estado se reentra (Map/Parallel/loops): aproximación aceptable.
fn parse_history(
    events: &[aws_sdk_sfn::types::HistoryEvent],
) -> (Vec<StateSpanDto>, Option<String>) {
    use std::collections::HashMap;

    let mut spans: Vec<StateSpanDto> = Vec::new();
    let mut open: HashMap<String, usize> = HashMap::new();
    let mut failed_state: Option<String> = None;

    for ev in events {
        let ts = dt_millis(ev.timestamp());
        if let Some(d) = ev.state_entered_event_details() {
            let name = d.name().to_string();
            open.insert(name.clone(), spans.len());
            spans.push(StateSpanDto {
                name,
                entered_ts: ts,
                exited_ts: None,
                failed: false,
            });
        } else if let Some(d) = ev.state_exited_event_details() {
            if let Some(&idx) = open.get(d.name()) {
                spans[idx].exited_ts = ts;
            }
        } else if is_failure_event(ev) {
            // El estado que reventó es el último span abierto (sin exited) sin marcar.
            if let Some(span) = spans
                .iter_mut()
                .rev()
                .find(|s| s.exited_ts.is_none() && !s.failed)
            {
                span.failed = true;
                failed_state = Some(span.name.clone());
            }
        }
    }

    (spans, failed_state)
}

fn is_failure_event(ev: &aws_sdk_sfn::types::HistoryEvent) -> bool {
    ev.execution_failed_event_details().is_some()
        || ev.task_failed_event_details().is_some()
        || ev.lambda_function_failed_event_details().is_some()
        || ev.activity_failed_event_details().is_some()
}

// --- Mock (tests / sin red) ---------------------------------------------------

/// Log groups falsos del ambiente. Un par de nombres llevan el `profile` activo
/// para que un cambio de ambiente sea visible en la lista.
fn mock_log_groups(env: &Env, query: Option<&str>) -> Vec<LogGroupDto> {
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
        // Mimetiza el filtro server-side por substring (logGroupNamePattern).
        .filter(|name| query.is_none_or(|q| name.contains(q)))
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
fn mock_executions(machine_arn: &str) -> Vec<ExecutionDto> {
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
    async fn mock_log_groups_search_filters_and_echoes_query() {
        let (tx, mut rx) = mpsc::channel(8);
        let fx = Effects::new_mock(tx, Env::new("dev", "us-east-1"));

        fx.dispatch(
            Action::LoadLogGroups {
                query: Some("orders".into()),
            },
            9,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 9);
        match envelope.message {
            Message::LogGroupsLoaded { groups, query, .. } => {
                assert_eq!(query.as_deref(), Some("orders"), "la query se ecoa");
                assert!(!groups.is_empty());
                assert!(
                    groups.iter().all(|g| g.name.contains("orders")),
                    "el mock filtra por substring como el server"
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
            Message::StateMachinesLoaded(machines) => {
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
            },
            6,
        );

        let envelope = rx.recv().await.expect("debe llegar un envelope");
        assert_eq!(envelope.epoch, 6);
        match envelope.message {
            Message::ExecutionsLoaded {
                machine_arn,
                executions,
            } => {
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

    #[test]
    fn parse_history_pairs_states_and_marks_failure() {
        use aws_sdk_sfn::primitives::DateTime;
        use aws_sdk_sfn::types::{
            ExecutionFailedEventDetails, HistoryEvent, HistoryEventType as T,
            StateEnteredEventDetails, StateExitedEventDetails,
        };

        let entered = |id, ts, name: &str| {
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
        };
        let exited = |id, ts, name: &str| {
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
        };
        let failed = |id, ts| {
            HistoryEvent::builder()
                .id(id)
                .timestamp(DateTime::from_millis(ts))
                .r#type(T::ExecutionFailed)
                .execution_failed_event_details(
                    ExecutionFailedEventDetails::builder()
                        .error("States.TaskFailed")
                        .cause("boom")
                        .build(),
                )
                .build()
                .unwrap()
        };

        let events = vec![
            entered(1, 1_000, "A"),
            exited(2, 1_500, "A"),
            entered(3, 1_500, "B"),
            failed(4, 1_800),
        ];
        let (spans, failed_state) = parse_history(&events);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "A");
        assert_eq!(spans[0].entered_ts, Some(1_000));
        assert_eq!(spans[0].exited_ts, Some(1_500));
        assert!(!spans[0].failed);
        assert_eq!(spans[1].name, "B");
        assert_eq!(spans[1].exited_ts, None);
        assert!(spans[1].failed, "el estado abierto al fallar se marca");
        assert_eq!(failed_state.as_deref(), Some("B"));
    }
}
