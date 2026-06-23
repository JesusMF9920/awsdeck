//! Vista `sfn`: Step Functions. Primer drill de 3 niveles: state machines →
//! ejecuciones (status coloreado) → detalle (input/output, error/cause y timeline
//! de estados con duración, resaltando el que reventó). Pura y síncrona; NUNCA
//! importa `aws-sdk-*` (recibe DTOs planos vía `on_message`).
//!
//! `R` en el detalle emite la intención `RedriveExecution`; la vista NO sabe de
//! modo escritura ni confirm: ese gate vive en el `App` (reusa el de `PurgeQueue`).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::View;
use crate::action::{Action, ConsoleTarget, ViewContext};
use crate::message::{
    ExecStatus, ExecutionDetailDto, ExecutionDto, LogWindow, MachineType, Message, StateMachineDto,
    StateSpanDto,
};
use crate::ui::detail::DetailPanel;
use crate::util::{fmt_epoch_millis, fuzzy_score, lambda_log_group_from_arn, ranked};

/// Presupuesto inicial de páginas del history y el paso de cada load-more (`o`).
/// Coincide con `effects::MAX_HISTORY_PAGES` (la carga inicial usa ese tope).
const HISTORY_PAGE_STEP: usize = 10;

/// Nivel de drill actual. Cada nivel carga los identificadores que `back()`
/// necesita para reconstruir el padre y que `on_message` usa como guard.
enum Level {
    Machines,
    Executions {
        machine_arn: String,
        machine_name: String,
        machine_type: MachineType,
    },
    Detail {
        machine_arn: String,
        machine_name: String,
        machine_type: MachineType,
        execution_arn: String,
    },
}

pub struct SfnView {
    level: Level,
    machines: Vec<StateMachineDto>,
    executions: Vec<ExecutionDto>,
    detail: Option<ExecutionDetailDto>,
    history: Vec<StateSpanDto>,
    failed_state: Option<String>,
    /// Panel con el input/output completo del estado seleccionado del timeline
    /// (`enter` en Detail): la fila los colapsa; el panel los muestra enteros,
    /// scrolleable/copiable. `None` = vista normal.
    detail_panel: Option<DetailPanel>,
    filter: String,
    loading: bool,
    /// Se alcanzó el tope de paginación de máquinas (hay más sin traer).
    machines_partial: bool,
    /// Se topó el tope de paginación del history de la ejecución (>~10k eventos).
    history_partial: bool,
    /// Páginas del history pedidas para la ejecución actual (`o` lo sube por
    /// `HISTORY_PAGE_STEP`). Se reinicia al drillear a una ejecución.
    history_pages: usize,
    /// El servidor tiene más ejecuciones (`next_token`): se muestran las recientes.
    executions_partial: bool,
    /// `next_token` de las ejecuciones para `o` (load-more, append). `None` = no hay más.
    exec_token: Option<String>,
    /// Filtro server-side por estado (`:status failed`); `None` = todas.
    exec_status: Option<ExecStatus>,
    state: ListState,
}

impl SfnView {
    pub fn new() -> Self {
        Self {
            level: Level::Machines,
            machines: Vec::new(),
            executions: Vec::new(),
            detail: None,
            history: Vec::new(),
            failed_state: None,
            detail_panel: None,
            filter: String::new(),
            loading: false,
            machines_partial: false,
            history_partial: false,
            history_pages: HISTORY_PAGE_STEP,
            executions_partial: false,
            exec_token: None,
            exec_status: None,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    // --- Filtrado / selección -------------------------------------------------

    fn filtered_machine_indices(&self) -> Vec<usize> {
        ranked(self.machines.len(), &self.filter, |i| {
            fuzzy_score(&self.machines[i].name, &self.filter)
        })
    }

    fn filtered_execution_indices(&self) -> Vec<usize> {
        ranked(self.executions.len(), &self.filter, |i| {
            fuzzy_score(&self.executions[i].name, &self.filter)
        })
    }

    /// El timeline también se filtra por nombre de estado (`/` consistente en los
    /// 3 niveles). Útil en histories largos (Map/Parallel con cientos de estados).
    fn filtered_history_indices(&self) -> Vec<usize> {
        ranked(self.history.len(), &self.filter, |i| {
            fuzzy_score(&self.history[i].name, &self.filter)
        })
    }

    /// Tamaño de la lista navegable del nivel activo.
    fn visible_len(&self) -> usize {
        match self.level {
            Level::Machines => self.filtered_machine_indices().len(),
            Level::Executions { .. } => self.filtered_execution_indices().len(),
            Level::Detail { .. } => self.filtered_history_indices().len(),
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.visible_len();
        if len == 0 {
            self.state.select(None);
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, len as i32 - 1) as usize;
        self.state.select(Some(next));
    }

    fn select_edge(&mut self, last: bool) {
        let len = self.visible_len();
        if len == 0 {
            self.state.select(None);
        } else {
            self.state.select(Some(if last { len - 1 } else { 0 }));
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_len();
        match self.state.selected() {
            _ if len == 0 => self.state.select(None),
            Some(i) if i >= len => self.state.select(Some(len - 1)),
            None => self.state.select(Some(0)),
            Some(_) => {}
        }
    }

    fn selected_machine(&self) -> Option<StateMachineDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_machine_indices().get(sel)?;
        Some(self.machines[idx].clone())
    }

    /// Texto a copiar con `y`: el ARN del item del nivel actual (máquina / ejecución /
    /// ejecución en detalle).
    fn copy_text(&self) -> Option<String> {
        match &self.level {
            Level::Machines => self.selected_machine().map(|m| m.arn),
            Level::Executions { .. } => self.selected_execution().map(|e| e.arn),
            Level::Detail { execution_arn, .. } => Some(execution_arn.clone()),
        }
    }

    /// Recurso del nivel actual a abrir en la consola de Step Functions.
    fn console_target(&self) -> Option<ConsoleTarget> {
        match &self.level {
            Level::Machines => self
                .selected_machine()
                .map(|m| ConsoleTarget::StateMachine { arn: m.arn }),
            Level::Executions { .. } => self
                .selected_execution()
                .map(|e| ConsoleTarget::Execution { arn: e.arn }),
            Level::Detail { execution_arn, .. } => Some(ConsoleTarget::Execution {
                arn: execution_arn.clone(),
            }),
        }
    }

    fn selected_execution(&self) -> Option<ExecutionDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_execution_indices().get(sel)?;
        Some(self.executions[idx].clone())
    }

    /// Estado seleccionado del timeline (en `Detail`). `None` fuera de `Detail` o si la
    /// lista visible está vacía.
    fn selected_state_span(&self) -> Option<StateSpanDto> {
        if !matches!(self.level, Level::Detail { .. }) {
            return None;
        }
        let sel = self.state.selected()?;
        let idx = *self.filtered_history_indices().get(sel)?;
        Some(self.history[idx].clone())
    }

    /// Abre el panel con el input/output completos del estado seleccionado (la fila los
    /// colapsa). `enter` sobre un estado del timeline en `Detail`.
    fn open_state_io(&mut self) {
        let Some(span) = self.selected_state_span() else {
            return;
        };
        let body = match (&span.input, &span.output) {
            (None, None) => "(este estado no expone input/output)".to_string(),
            (i, o) => {
                let part = |label: &str, v: &Option<String>| {
                    format!("=== {label} ===\n{}", v.as_deref().unwrap_or("(ninguno)"))
                };
                format!("{}\n\n{}", part("input", i), part("output", o))
            }
        };
        self.detail_panel = Some(DetailPanel::new(span.name, body));
    }

    // --- Navegación -----------------------------------------------------------

    fn drill(&mut self) -> Vec<Action> {
        if matches!(self.level, Level::Machines) {
            return match self.selected_machine() {
                Some(m) => {
                    let express = m.machine_type == MachineType::Express;
                    self.level = Level::Executions {
                        machine_arn: m.arn.clone(),
                        machine_name: m.name.clone(),
                        machine_type: m.machine_type,
                    };
                    self.executions.clear();
                    self.executions_partial = false;
                    self.exec_token = None;
                    self.exec_status = None; // cada máquina arranca sin filtro de estado
                    self.state.select(Some(0));
                    if express {
                        // EXPRESS no soporta list_executions (van a CloudWatch Logs):
                        // no se pide nada; el render muestra una nota.
                        self.loading = false;
                        vec![Action::ClearFilter]
                    } else {
                        self.loading = true;
                        vec![
                            Action::ClearFilter,
                            Action::LoadExecutions {
                                machine_arn: m.arn,
                                status: None,
                                token: None,
                            },
                        ]
                    }
                }
                None => vec![],
            };
        }

        // Executions → Detail (clona el contexto del nivel antes de mutarlo).
        let ctx = if let Level::Executions {
            machine_arn,
            machine_name,
            machine_type,
        } = &self.level
        {
            Some((machine_arn.clone(), machine_name.clone(), *machine_type))
        } else {
            None
        };
        if let Some((machine_arn, machine_name, machine_type)) = ctx {
            return match self.selected_execution() {
                Some(e) => {
                    self.level = Level::Detail {
                        machine_arn,
                        machine_name,
                        machine_type,
                        execution_arn: e.arn.clone(),
                    };
                    self.detail = None;
                    self.history.clear();
                    self.failed_state = None;
                    self.history_partial = false;
                    self.history_pages = HISTORY_PAGE_STEP; // presupuesto fresco por ejecución
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![
                        Action::ClearFilter,
                        Action::LoadExecutionDetail {
                            execution_arn: e.arn,
                        },
                    ]
                }
                None => vec![],
            };
        }

        vec![] // Detail: enter no hace nada (v2 es solo-lectura del timeline)
    }

    /// `esc`: despoja un nivel; en la raíz (machines) emite `Back` (→ menú).
    fn back(&mut self) -> Vec<Action> {
        self.detail_panel = None; // cerrar cualquier panel al subir de nivel
        // Detail → Executions (reconstruido con lo que Detail carga; la lista de
        // ejecuciones sigue cacheada, así que no se recarga).
        let ctx = if let Level::Detail {
            machine_arn,
            machine_name,
            machine_type,
            ..
        } = &self.level
        {
            Some((machine_arn.clone(), machine_name.clone(), *machine_type))
        } else {
            None
        };
        if let Some((machine_arn, machine_name, machine_type)) = ctx {
            self.level = Level::Executions {
                machine_arn,
                machine_name,
                machine_type,
            };
            self.detail = None;
            self.history.clear();
            self.failed_state = None;
            self.history_partial = false;
            self.loading = false;
            self.state.select(Some(0));
            self.clamp_selection();
            return vec![Action::ClearFilter];
        }
        if matches!(self.level, Level::Executions { .. }) {
            self.level = Level::Machines;
            self.loading = false;
            self.state.select(Some(0));
            self.clamp_selection();
            return vec![Action::ClearFilter];
        }
        vec![Action::Back]
    }

    fn refresh(&mut self) -> Vec<Action> {
        let actions = match &self.level {
            Level::Machines => vec![Action::LoadStateMachines],
            Level::Executions {
                machine_arn,
                machine_type,
                ..
            } => {
                if *machine_type == MachineType::Express {
                    vec![]
                } else {
                    self.exec_token = None;
                    vec![Action::LoadExecutions {
                        machine_arn: machine_arn.clone(),
                        status: self.exec_status,
                        token: None,
                    }]
                }
            }
            Level::Detail { execution_arn, .. } => vec![Action::LoadExecutionDetail {
                execution_arn: execution_arn.clone(),
            }],
        };
        self.loading = !actions.is_empty();
        actions
    }

    /// `o`: trae la siguiente página de ejecuciones (append) si hay `next_token`. Así
    /// ninguna ejecución (p. ej. un fallo de ayer fuera del top-50) queda inalcanzable.
    fn load_more_executions(&mut self) -> Vec<Action> {
        let Some(token) = self.exec_token.clone() else {
            return vec![];
        };
        if let Level::Executions {
            machine_arn,
            machine_type,
            ..
        } = &self.level
            && *machine_type != MachineType::Express
        {
            let machine_arn = machine_arn.clone();
            self.loading = true;
            return vec![Action::LoadExecutions {
                machine_arn,
                status: self.exec_status,
                token: Some(token),
            }];
        }
        vec![]
    }

    /// `o` en Detail: carga más del history si está parcial. Sube el presupuesto de
    /// páginas y re-pide (effects re-fetchea y re-parsea TODO → emparejamiento correcto).
    fn load_more_history(&mut self) -> Vec<Action> {
        if let Level::Detail { execution_arn, .. } = &self.level
            && self.history_partial
        {
            self.history_pages += HISTORY_PAGE_STEP;
            self.loading = true;
            return vec![Action::LoadMoreExecutionHistory {
                execution_arn: execution_arn.clone(),
                page_budget: self.history_pages,
            }];
        }
        vec![]
    }

    /// `:status <estado|all>` (solo en Executions): fija el filtro server-side por estado
    /// y recarga desde la primera página. Permite "ver solo las fallidas" más allá del top-50.
    fn set_exec_status(&mut self, arg: &str) -> Vec<Action> {
        let machine_arn = match &self.level {
            Level::Executions {
                machine_arn,
                machine_type,
                ..
            } if *machine_type != MachineType::Express => machine_arn.clone(),
            _ => return vec![],
        };
        let status = match arg.to_ascii_lowercase().as_str() {
            "all" | "todas" | "todos" | "*" => None,
            "failed" | "fail" => Some(ExecStatus::Failed),
            "running" | "run" => Some(ExecStatus::Running),
            "succeeded" | "ok" | "success" => Some(ExecStatus::Succeeded),
            "aborted" | "abort" => Some(ExecStatus::Aborted),
            "timedout" | "timed_out" | "timeout" => Some(ExecStatus::TimedOut),
            _ => return vec![], // arg inválido → no-op (el App avisa "comando desconocido")
        };
        self.exec_status = status;
        self.exec_token = None;
        self.executions.clear();
        self.executions_partial = false;
        self.state.select(Some(0));
        self.loading = true;
        vec![Action::LoadExecutions {
            machine_arn,
            status,
            token: None,
        }]
    }

    /// `R` en el detalle: emite la intención de redrive SOLO si la ejecución es
    /// redrivable (FAILED/TIMED_OUT/ABORTED). El App la gatea (modo escritura +
    /// confirm). En ejecuciones sanas/en curso no ofrece nada.
    fn redrive_intent(&self) -> Vec<Action> {
        if let Level::Detail { execution_arn, .. } = &self.level
            && self
                .detail
                .as_ref()
                .is_some_and(|d| d.status.is_redrivable())
        {
            return vec![Action::RedriveExecution {
                execution_arn: execution_arn.clone(),
            }];
        }
        vec![]
    }

    /// `l` en el detalle: salta a los logs de la Lambda del estado seleccionado.
    /// Resuelve el log group desde el `resource_arn` del span y emite el handoff
    /// `ActivateViewWithContext` (lectura → sin gate). No ofrece nada si el estado no
    /// invoca una Lambda. La ventana se acota a la duración del estado.
    fn open_lambda_logs(&self) -> Vec<Action> {
        let Some(span) = self.selected_state_span() else {
            return vec![];
        };
        let Some(group) = span
            .resource_arn
            .as_deref()
            .and_then(lambda_log_group_from_arn)
        else {
            return vec![];
        };
        vec![Action::ActivateViewWithContext {
            id: "logs".to_string(),
            context: ViewContext::LogGroupTail {
                group,
                window: self.lambda_window(&span),
            },
        }]
    }

    /// Ventana de tiempo para los logs de la Lambda de un estado: el rango del span
    /// (con colchón de 1 min para no perder líneas de borde), con fallback a la ventana
    /// de la ejecución y, en último caso, la última hora.
    fn lambda_window(&self, span: &StateSpanDto) -> LogWindow {
        const PAD: i64 = 60_000; // 1 min de colchón a cada lado
        let range = |from: Option<i64>, to: Option<i64>| {
            from.map(|f| LogWindow::Range {
                from: f - PAD,
                to: to.map(|t| t + PAD),
            })
        };
        range(span.entered_ts, span.exited_ts)
            .or_else(|| {
                self.detail
                    .as_ref()
                    .and_then(|d| range(d.start_ts, d.stop_ts))
            })
            .unwrap_or(LogWindow::Last(3_600_000))
    }

    // --- Render ---------------------------------------------------------------

    fn machines_title(&self) -> String {
        let total = self.machines.len();
        let shown = self.filtered_machine_indices().len();
        let partial = if self.machines_partial {
            " · parcial"
        } else {
            ""
        };
        if self.filter.is_empty() {
            format!(" {total} state machines{partial} ")
        } else {
            format!(
                " {shown}/{total} state machines{partial} · filtro: {} ",
                self.filter
            )
        }
    }

    fn executions_title(&self) -> String {
        let name = match &self.level {
            Level::Executions { machine_name, .. } | Level::Detail { machine_name, .. } => {
                machine_name.as_str()
            }
            Level::Machines => "",
        };
        let total = self.executions.len();
        let shown = self.filtered_execution_indices().len();
        let partial = if self.executions_partial {
            " · parcial · o: más"
        } else {
            ""
        };
        let status = match self.exec_status {
            Some(s) => format!(" · solo {}", s.label()),
            None => String::new(),
        };
        if self.filter.is_empty() {
            format!(" {name} · {total} ejecuciones{status}{partial} ")
        } else {
            format!(
                " {name} · {shown}/{total}{status}{partial} · filtro: {} ",
                self.filter
            )
        }
    }

    fn timeline_title(&self) -> String {
        let total = self.history.len();
        let redrive = if self
            .detail
            .as_ref()
            .is_some_and(|d| d.status.is_redrivable())
        {
            " · [R] redrive"
        } else {
            ""
        };
        let partial = if self.history_partial {
            " · parcial · o: más"
        } else {
            ""
        };
        if self.filter.is_empty() {
            format!(" timeline · {total} estados{partial}{redrive} ")
        } else {
            let shown = self.filtered_history_indices().len();
            format!(
                " timeline · {shown}/{total} estados · filtro: {}{partial}{redrive} ",
                self.filter
            )
        }
    }

    fn detail_header(&self) -> Paragraph<'static> {
        let block = Block::bordered().title(" ejecución ");
        let Some(d) = &self.detail else {
            let msg = if self.loading { "cargando…" } else { "—" };
            return Paragraph::new(msg).block(block);
        };
        let started = d
            .start_ts
            .map(fmt_epoch_millis)
            .unwrap_or_else(|| "—".to_string());
        let dur = duration_label(d.start_ts, d.stop_ts);
        let redrive = d
            .redrive_count
            .filter(|&n| n > 0)
            .map(|n| format!("   redrive {n}"))
            .unwrap_or_default();

        let mut lines = vec![Line::from(vec![
            Span::styled(format!("{:<9}", "status"), Style::new().dark_gray()),
            Span::styled(d.status.label(), Style::new().fg(status_color(d.status))),
            Span::raw(format!("   inicio {started}   duración {dur}{redrive}")),
        ])];
        if let Some(e) = &d.error {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<9}", "error"), Style::new().dark_gray()),
                Span::styled(oneline(e, 80), Style::new().fg(Color::Red)),
            ]));
        }
        if let Some(c) = &d.cause {
            lines.push(row("cause", oneline(c, 80)));
        }
        if let Some(i) = &d.input {
            lines.push(row("input", oneline(i, 80)));
        }
        if let Some(o) = &d.output {
            lines.push(row("output", oneline(o, 80)));
        }
        Paragraph::new(lines).block(block)
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect, block: Block, items: Vec<ListItem>) {
        if items.is_empty() {
            let msg = if self.loading {
                "cargando…"
            } else if self.filter.is_empty() {
                "(sin resultados)"
            } else {
                "(sin coincidencias para el filtro)"
            };
            frame.render_widget(Paragraph::new(msg).block(block), area);
            return;
        }
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed())
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

impl Default for SfnView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for SfnView {
    fn id(&self) -> &'static str {
        "sfn"
    }

    fn description(&self) -> &'static str {
        "Step Functions: ejecuciones, timeline, redrive"
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        // Con el panel de input/output abierto, sus teclas (scroll/copiar/cerrar) mandan.
        if let Some(p) = &self.detail_panel {
            return p.hints();
        }
        let mut hints = vec![("y", "copiar ARN"), ("O", "consola")];
        // En ejecuciones: filtrar por estado y traer más allá del top-50.
        if matches!(self.level, Level::Executions { .. }) {
            hints.push((":status", "filtrar (failed/all)"));
            if self.executions_partial {
                hints.push(("o", "más"));
            }
        }
        // En el detalle, `enter` expande el input/output del estado seleccionado.
        if self
            .selected_state_span()
            .is_some_and(|s| s.input.is_some() || s.output.is_some())
        {
            hints.push(("enter", "in/out"));
        }
        // En el detalle con history parcial, `o` trae más estados del timeline.
        if matches!(self.level, Level::Detail { .. }) && self.history_partial {
            hints.push(("o", "más history"));
        }
        // `l` solo si el estado seleccionado del timeline invoca una Lambda (cross-link
        // a sus logs). Lectura → sin gate.
        if self
            .selected_state_span()
            .is_some_and(|s| s.resource_arn.is_some())
        {
            hints.push(("l", "logs Lambda"));
        }
        // `R` solo se ofrece sobre una ejecución redrivable (FAILED/TIMED_OUT/ABORTED),
        // igual que `redrive_intent`. Gated por modo escritura + confirm en el App.
        if matches!(self.level, Level::Detail { .. })
            && self
                .detail
                .as_ref()
                .is_some_and(|d| d.status.is_redrivable())
        {
            hints.push(("R", "redrive"));
        }
        hints
    }

    fn title(&self) -> String {
        match &self.level {
            Level::Machines => "sfn".to_string(),
            Level::Executions { machine_name, .. } => format!("sfn / {machine_name}"),
            Level::Detail {
                machine_name,
                execution_arn,
                ..
            } => format!("sfn / {machine_name} / {}", short_exec(execution_arn)),
        }
    }

    fn on_activate(&mut self) -> Vec<Action> {
        self.level = Level::Machines;
        self.executions.clear();
        self.exec_token = None;
        self.exec_status = None;
        self.detail = None;
        self.history.clear();
        self.failed_state = None;
        self.detail_panel = None;
        self.loading = true;
        self.machines_partial = false;
        self.history_partial = false;
        self.history_pages = HISTORY_PAGE_STEP;
        self.executions_partial = false;
        self.state.select(Some(0));
        vec![Action::LoadStateMachines]
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // Panel de input/output abierto: `y` copia su contenido, el resto scrollea/cierra.
        if self.detail_panel.is_some() {
            if key.code == KeyCode::Char('y') {
                return self
                    .detail_panel
                    .as_ref()
                    .map(|p| Action::CopyToClipboard {
                        text: p.content().to_string(),
                    })
                    .into_iter()
                    .collect();
            }
            if let Some(p) = self.detail_panel.as_mut()
                && p.on_key(key)
            {
                self.detail_panel = None;
            }
            return vec![];
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                vec![]
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                vec![]
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.select_edge(false);
                vec![]
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.select_edge(true);
                vec![]
            }
            // En el detalle, `enter` expande el input/output del estado seleccionado del
            // timeline (la lista navegable son los estados); en los demás niveles, drillea.
            KeyCode::Enter if matches!(self.level, Level::Detail { .. }) => {
                self.open_state_io();
                vec![]
            }
            KeyCode::Enter => self.drill(),
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') => self.refresh(),
            // `o` carga más según el nivel: ejecuciones (append) o history (re-fetch).
            KeyCode::Char('o') => match self.level {
                Level::Executions { .. } => self.load_more_executions(),
                Level::Detail { .. } => self.load_more_history(),
                Level::Machines => vec![],
            },
            KeyCode::Char('R') => self.redrive_intent(),
            KeyCode::Char('l') => self.open_lambda_logs(),
            KeyCode::Char('y') => self
                .copy_text()
                .map(|text| Action::CopyToClipboard { text })
                .into_iter()
                .collect(),
            KeyCode::Char('O') => self
                .console_target()
                .map(|target| Action::OpenConsole { target })
                .into_iter()
                .collect(),
            _ => vec![],
        }
    }

    fn on_command(&mut self, cmd: &str) -> Vec<Action> {
        // `:status <estado|all>` filtra las ejecuciones server-side por estado.
        match cmd.strip_prefix("status ") {
            Some(arg) => self.set_exec_status(arg.trim()),
            None => vec![],
        }
    }

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::StateMachinesLoaded { machines, more } => {
                self.machines = machines.clone();
                self.machines_partial = *more;
                if matches!(self.level, Level::Machines) {
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::ExecutionsLoaded {
                machine_arn,
                executions,
                next_token,
                append,
            } => {
                if let Level::Executions {
                    machine_arn: current,
                    ..
                } = &self.level
                    && current == machine_arn
                {
                    if *append {
                        self.executions.extend(executions.clone());
                    } else {
                        self.executions = executions.clone();
                    }
                    self.exec_token = next_token.clone();
                    self.executions_partial = next_token.is_some();
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::ExecutionDetailLoaded {
                execution_arn,
                detail,
                history,
                failed_state,
                history_more,
            } => {
                if let Level::Detail {
                    execution_arn: current,
                    ..
                } = &self.level
                    && current == execution_arn
                {
                    // Recordar qué estado estaba seleccionado ANTES de reemplazar el
                    // history (para preservarlo en el load-more, donde la lista crece).
                    let prev_name = self.selected_state_span().map(|s| s.name);
                    self.detail = Some(detail.clone());
                    self.history = history.clone();
                    self.failed_state = failed_state.clone();
                    self.history_partial = *history_more;
                    self.loading = false;
                    // Preservar la selección por NOMBRE sobre la lista visible (robusto a
                    // que la lista crezca con `o`). Si no había selección previa (1ª carga),
                    // saltar al estado que reventó; si tampoco, al tope.
                    let visible = self.filtered_history_indices();
                    let pos = |hist: &[StateSpanDto], vis: &[usize], name: &str| {
                        vis.iter().position(|&i| hist[i].name == name)
                    };
                    let idx = prev_name
                        .as_deref()
                        .and_then(|n| pos(&self.history, &visible, n))
                        .or_else(|| {
                            self.failed_state
                                .as_deref()
                                .and_then(|fs| pos(&self.history, &visible, fs))
                        })
                        .unwrap_or(0);
                    self.state.select(Some(idx));
                    self.clamp_selection();
                }
            }
            Message::ExecutionRedriven { execution_arn } => {
                // El App muestra info y re-dispara LoadExecutionDetail; aquí
                // marcamos recarga.
                if let Level::Detail {
                    execution_arn: current,
                    ..
                } = &self.level
                    && current == execution_arn
                {
                    self.loading = true;
                }
            }
            Message::Error { .. } => self.loading = false,
            // Mensajes de otras vistas: se ignoran.
            _ => {}
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.state.select(Some(0)); // top = mejor match (estilo fzf)
        self.clamp_selection();
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Panel de input/output del estado: ocupa el cuerpo entero.
        if let Some(p) = self.detail_panel.as_mut() {
            p.render(frame, area);
            return;
        }
        match &self.level {
            Level::Machines => {
                let block = Block::bordered().title(self.machines_title());
                let items: Vec<ListItem> = self
                    .filtered_machine_indices()
                    .into_iter()
                    .map(|i| machine_item(&self.machines[i]))
                    .collect();
                self.render_list(frame, area, block, items);
            }
            Level::Executions {
                machine_type: MachineType::Express,
                ..
            } => {
                let block = Block::bordered().title(self.executions_title());
                let note = "EXPRESS: las ejecuciones no se listan vía API \
                    (van a CloudWatch Logs). Úsalas desde la vista logs.";
                frame.render_widget(Paragraph::new(note).block(block), area);
            }
            Level::Executions { .. } => {
                let block = Block::bordered().title(self.executions_title());
                let items: Vec<ListItem> = self
                    .filtered_execution_indices()
                    .into_iter()
                    .map(|i| execution_item(&self.executions[i]))
                    .collect();
                self.render_list(frame, area, block, items);
            }
            Level::Detail { .. } => {
                let [head, timeline] =
                    Layout::vertical([Constraint::Length(8), Constraint::Min(0)]).areas(area);
                frame.render_widget(self.detail_header(), head);

                let block = Block::bordered().title(self.timeline_title());
                let items: Vec<ListItem> = self
                    .filtered_history_indices()
                    .into_iter()
                    .map(|i| span_item(&self.history[i]))
                    .collect();
                if items.is_empty() {
                    let msg = if self.loading {
                        "cargando…"
                    } else if !self.filter.is_empty() {
                        "(sin coincidencias para el filtro)"
                    } else {
                        "(sin history)"
                    };
                    frame.render_widget(Paragraph::new(msg).block(block), timeline);
                    return;
                }
                let list = List::new(items)
                    .block(block)
                    .highlight_style(Style::new().reversed())
                    .highlight_symbol("› ");
                frame.render_stateful_widget(list, timeline, &mut self.state);
            }
        }
    }
}

// --- Construcción de filas y helpers ------------------------------------------

fn status_color(s: ExecStatus) -> Color {
    match s {
        ExecStatus::Succeeded => Color::Green,
        ExecStatus::Failed | ExecStatus::TimedOut | ExecStatus::Aborted => Color::Red,
        ExecStatus::Running => Color::Yellow,
        ExecStatus::PendingRedrive => Color::Cyan,
    }
}

/// Duración legible de milisegundos: `"3.4s"`, `"2m 5s"`, `"1h 12m"`.
fn fmt_dur(ms: i64) -> String {
    if ms < 0 {
        return "—".to_string();
    }
    let secs = ms / 1000;
    if secs < 60 {
        format!("{}.{}s", secs, (ms % 1000) / 100)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn duration_label(start: Option<i64>, stop: Option<i64>) -> String {
    match (start, stop) {
        (Some(a), Some(b)) => fmt_dur(b - a),
        (Some(_), None) => "en curso".to_string(),
        _ => "—".to_string(),
    }
}

fn short_exec(arn: &str) -> &str {
    arn.rsplit(':').next().unwrap_or(arn)
}

/// Colapsa un payload multilínea a una sola línea truncada (para previews).
fn oneline(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

fn row(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::new().dark_gray()),
        Span::raw(value),
    ])
}

fn machine_item(m: &StateMachineDto) -> ListItem<'static> {
    let badge = match m.machine_type {
        MachineType::Express => "[express]",
        MachineType::Standard => "[standard]",
    };
    let created = m
        .created_ts
        .map(fmt_epoch_millis)
        .unwrap_or_else(|| "—".to_string());
    ListItem::new(Line::from(vec![
        Span::raw(m.name.clone()),
        Span::raw("  "),
        Span::styled(badge, Style::new().dark_gray()),
        Span::raw("  "),
        Span::styled(created, Style::new().dark_gray()),
    ]))
}

fn execution_item(e: &ExecutionDto) -> ListItem<'static> {
    let when = e
        .start_ts
        .map(fmt_epoch_millis)
        .unwrap_or_else(|| "—".to_string());
    let dur = duration_label(e.start_ts, e.stop_ts);
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:<15}", e.status.label()),
            Style::new().fg(status_color(e.status)),
        ),
        Span::raw(e.name.clone()),
        Span::raw("  "),
        Span::styled(format!("{when}  {dur}"), Style::new().dark_gray()),
    ]))
}

fn span_item(s: &StateSpanDto) -> ListItem<'static> {
    let dur = match (s.entered_ts, s.exited_ts) {
        (Some(a), Some(b)) => fmt_dur(b - a),
        _ => "—".to_string(),
    };
    let (marker, name_style) = if s.failed {
        ("✗ ", Style::new().fg(Color::Red).bold())
    } else {
        ("  ", Style::new())
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{marker}{}", s.name), name_style),
        Span::raw("  "),
        Span::styled(dur, Style::new().dark_gray()),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, machine_type: MachineType) -> StateMachineDto {
        StateMachineDto {
            arn: format!("arn:aws:states:us-east-1:000:stateMachine:{name}"),
            name: name.to_string(),
            machine_type,
            created_ts: Some(1_700_000_000_000),
        }
    }

    fn exec(name: &str, status: ExecStatus) -> ExecutionDto {
        ExecutionDto {
            arn: format!("arn:aws:states:us-east-1:000:execution:m1:{name}"),
            name: name.to_string(),
            status,
            start_ts: Some(1_700_000_000_000),
            stop_ts: Some(1_700_000_045_000),
        }
    }

    /// Construye un `StateMachinesLoaded` no-parcial (el caso típico en tests).
    fn machines_msg(machines: Vec<StateMachineDto>) -> Message {
        Message::StateMachinesLoaded {
            machines,
            more: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        use ratatui::crossterm::event::KeyModifiers;
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Lleva la vista a `Detail` con un detalle del status dado.
    fn view_in_detail(status: ExecStatus) -> SfnView {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // → Executions
        let machine_arn = "arn:aws:states:us-east-1:000:stateMachine:m1".to_string();
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn,
            executions: vec![exec("e1", status)],
            next_token: None,
            append: false,
        });
        v.on_key(key(KeyCode::Enter)); // → Detail
        let execution_arn = exec("e1", status).arn;
        v.on_message(&Message::ExecutionDetailLoaded {
            execution_arn,
            detail: ExecutionDetailDto {
                status,
                start_ts: Some(1_700_000_000_000),
                stop_ts: Some(1_700_000_012_000),
                input: Some("{\"a\":1}".into()),
                output: None,
                error: status.is_redrivable().then(|| "States.TaskFailed".into()),
                cause: status.is_redrivable().then(|| "boom".into()),
                redrive_count: Some(0),
            },
            history: vec![
                StateSpanDto {
                    name: "Validate".into(),
                    entered_ts: Some(0),
                    exited_ts: Some(2_000),
                    failed: false,
                    input: Some("{\"step\":\"validate\"}".into()),
                    output: Some("{\"valid\":true}".into()),
                    resource_arn: None, // estado sin Lambda → `l` no aplica
                },
                StateSpanDto {
                    name: "ProcessOrder".into(),
                    entered_ts: Some(2_000),
                    exited_ts: None,
                    failed: status.is_redrivable(),
                    input: Some("{\"step\":\"process\"}".into()),
                    output: None, // no salió (falló o en curso)
                    resource_arn: Some(
                        "arn:aws:lambda:us-east-1:000000000000:function:ProcessOrder".into(),
                    ),
                },
            ],
            failed_state: status.is_redrivable().then(|| "ProcessOrder".into()),
            history_more: false,
        });
        v
    }

    #[test]
    fn activate_requests_state_machines() {
        let mut v = SfnView::new();
        assert!(matches!(
            v.on_activate().as_slice(),
            [Action::LoadStateMachines]
        ));
    }

    #[test]
    fn ingests_machines_via_message() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![
            machine("a", MachineType::Standard),
            machine("b", MachineType::Express),
        ]));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn filter_narrows_machine_list() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![
            machine("order-saga", MachineType::Standard),
            machine("payment-flow", MachineType::Standard),
            machine("ingest-fast", MachineType::Express),
        ]));
        v.set_filter("ORDER"); // case-insensitive
        assert_eq!(v.visible_len(), 1);
        // fuzzy: subsecuencia no contigua
        v.set_filter("pyflow");
        assert_eq!(v.visible_len(), 1);
    }

    #[test]
    fn partial_flag_shows_in_titles() {
        let mut v = SfnView::new();
        // Sin parcial: el título no menciona "parcial".
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        assert!(!v.machines_title().contains("parcial"));
        // Con parcial (tope de paginación): aparece la señal.
        v.on_message(&Message::StateMachinesLoaded {
            machines: vec![machine("m1", MachineType::Standard)],
            more: true,
        });
        assert!(v.machines_title().contains("parcial"));

        // Ejecuciones: drill a la máquina y cargar con more=true.
        v.on_key(key(KeyCode::Enter)); // → Executions
        let machine_arn = "arn:aws:states:us-east-1:000:stateMachine:m1".to_string();
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn,
            executions: vec![exec("e1", ExecStatus::Succeeded)],
            next_token: Some("more".into()),
            append: false,
        });
        assert!(v.executions_title().contains("parcial"));
    }

    #[test]
    fn filter_narrows_timeline_in_detail() {
        // El timeline (Validate, ProcessOrder) se filtra por nombre de estado.
        let mut v = view_in_detail(ExecStatus::Succeeded);
        assert_eq!(v.visible_len(), 2, "timeline completo sin filtro");
        v.set_filter("process"); // fuzzy → solo ProcessOrder
        assert_eq!(v.visible_len(), 1);
        assert!(v.timeline_title().contains("filtro"));
        v.set_filter("zzz");
        assert_eq!(v.visible_len(), 0);
        v.set_filter(""); // se restaura completo
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn detail_preselects_failed_state_in_visible_list() {
        // ProcessOrder reventó → preseleccionado (índice 1 del timeline visible).
        let v = view_in_detail(ExecStatus::Failed);
        assert_eq!(v.state.selected(), Some(1));
    }

    #[test]
    fn enter_drills_into_standard_machine_executions() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine(
            "order-saga",
            MachineType::Standard,
        )]));
        let actions = v.on_key(key(KeyCode::Enter));
        match actions.as_slice() {
            [
                Action::ClearFilter,
                Action::LoadExecutions {
                    machine_arn,
                    status: None,
                    token: None,
                },
            ] => {
                assert!(machine_arn.ends_with("order-saga"))
            }
            other => panic!("se esperaba ClearFilter+LoadExecutions, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Executions { .. }));
    }

    #[test]
    fn o_loads_more_executions_and_appends() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // → Executions
        let arn = "arn:aws:states:us-east-1:000:stateMachine:m1".to_string();
        // Primera página con next_token → guarda el token y marca parcial.
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: arn.clone(),
            executions: vec![exec("e1", ExecStatus::Succeeded)],
            next_token: Some("tok".into()),
            append: false,
        });
        assert_eq!(v.visible_len(), 1);
        assert!(v.executions_partial);

        // `o` → load-more con el token (append).
        match v.on_key(key(KeyCode::Char('o'))).as_slice() {
            [
                Action::LoadExecutions {
                    machine_arn,
                    status: None,
                    token: Some(t),
                },
            ] => {
                assert!(machine_arn.ends_with("m1"));
                assert_eq!(t, "tok");
            }
            other => panic!("se esperaba LoadExecutions con token, llegó {other:?}"),
        }

        // Llega la página 2 (append) → se extiende, no reemplaza.
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: arn,
            executions: vec![exec("e2", ExecStatus::Failed)],
            next_token: None,
            append: true,
        });
        assert_eq!(v.visible_len(), 2, "append extiende la lista");
        assert!(!v.executions_partial, "sin next_token ya no es parcial");
    }

    #[test]
    fn status_command_filters_executions_server_side() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // → Executions
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: "arn:aws:states:us-east-1:000:stateMachine:m1".into(),
            executions: vec![exec("e1", ExecStatus::Succeeded)],
            next_token: None,
            append: false,
        });

        // `:status failed` → recarga server-side filtrando por FAILED.
        match v.on_command("status failed").as_slice() {
            [
                Action::LoadExecutions {
                    status: Some(ExecStatus::Failed),
                    token: None,
                    ..
                },
            ] => {}
            other => panic!("se esperaba LoadExecutions FAILED, llegó {other:?}"),
        }
        assert_eq!(v.exec_status, Some(ExecStatus::Failed));
        assert!(v.executions_title().contains("solo FAILED"));

        // `:status all` limpia el filtro.
        match v.on_command("status all").as_slice() {
            [Action::LoadExecutions { status: None, .. }] => {}
            other => panic!("se esperaba LoadExecutions sin filtro, llegó {other:?}"),
        }
        assert_eq!(v.exec_status, None);
    }

    #[test]
    fn express_machine_does_not_request_executions() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine(
            "ingest-fast",
            MachineType::Express,
        )]));
        let actions = v.on_key(key(KeyCode::Enter));
        assert!(
            matches!(actions.as_slice(), [Action::ClearFilter]),
            "EXPRESS no dispara list_executions (evita el error del SDK); solo limpia el filtro"
        );
        assert!(matches!(
            v.level,
            Level::Executions {
                machine_type: MachineType::Express,
                ..
            }
        ));
        assert!(!v.loading, "no queda en loading: no hay request en vuelo");
    }

    #[test]
    fn enter_drills_into_execution_detail_and_back() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // → Executions
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: "arn:aws:states:us-east-1:000:stateMachine:m1".into(),
            executions: vec![exec("e1", ExecStatus::Succeeded)],
            next_token: None,
            append: false,
        });
        let actions = v.on_key(key(KeyCode::Enter)); // → Detail
        match actions.as_slice() {
            [
                Action::ClearFilter,
                Action::LoadExecutionDetail { execution_arn },
            ] => {
                assert!(execution_arn.ends_with("e1"))
            }
            other => panic!("se esperaba ClearFilter+LoadExecutionDetail, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Detail { .. }));

        // esc: Detail → Executions (con la lista cacheada); limpia el filtro al subir.
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::ClearFilter]));
        assert!(matches!(v.level, Level::Executions { .. }));
        assert_eq!(v.visible_len(), 1, "las ejecuciones siguen cacheadas");
    }

    #[test]
    fn esc_at_root_emits_back() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
        assert!(matches!(v.level, Level::Machines));
    }

    #[test]
    fn esc_at_root_empty_list_emits_back() {
        let mut v = SfnView::new();
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
    }

    #[test]
    fn esc_in_executions_pops_without_back() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // → Executions
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(
            matches!(actions.as_slice(), [Action::ClearFilter]),
            "esc en executions se consume en la vista (limpia el filtro al subir de nivel)"
        );
        assert!(matches!(v.level, Level::Machines));
    }

    #[test]
    fn executions_from_wrong_machine_are_ignored() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // drill m1 → Executions
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: "arn:aws:states:us-east-1:000:stateMachine:OTRA".into(),
            executions: vec![exec("x", ExecStatus::Succeeded)],
            next_token: None,
            append: false,
        });
        assert_eq!(
            v.visible_len(),
            0,
            "no se aceptan ejecuciones de otra máquina"
        );
    }

    #[test]
    fn detail_from_wrong_execution_is_ignored() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter));
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: "arn:aws:states:us-east-1:000:stateMachine:m1".into(),
            executions: vec![exec("e1", ExecStatus::Succeeded)],
            next_token: None,
            append: false,
        });
        v.on_key(key(KeyCode::Enter)); // → Detail de e1
        v.on_message(&Message::ExecutionDetailLoaded {
            execution_arn: "arn:…:OTRA".into(),
            detail: ExecutionDetailDto {
                status: ExecStatus::Succeeded,
                start_ts: None,
                stop_ts: None,
                input: None,
                output: None,
                error: None,
                cause: None,
                redrive_count: None,
            },
            history: vec![],
            failed_state: None,
            history_more: false,
        });
        assert!(
            v.detail.is_none(),
            "no se acepta el detalle de otra ejecución"
        );
    }

    #[test]
    fn y_copies_machine_arn() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert!(text.contains(":stateMachine:m1")),
            other => panic!("se esperaba CopyToClipboard, llegó {other:?}"),
        }
    }

    #[test]
    fn o_opens_machine_in_console() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        match v.on_key(key(KeyCode::Char('O'))).as_slice() {
            [
                Action::OpenConsole {
                    target: ConsoleTarget::StateMachine { arn },
                },
            ] => assert!(arn.contains(":stateMachine:m1")),
            other => panic!("se esperaba OpenConsole StateMachine, llegó {other:?}"),
        }
    }

    #[test]
    fn hints_offer_redrive_only_when_redrivable() {
        let failed = view_in_detail(ExecStatus::Failed);
        assert!(
            failed.hints().iter().any(|(k, _)| *k == "R"),
            "una ejecución fallida anuncia redrive"
        );
        let ok = view_in_detail(ExecStatus::Succeeded);
        assert!(
            !ok.hints().iter().any(|(k, _)| *k == "R"),
            "una ejecución sana no anuncia redrive"
        );
    }

    #[test]
    fn redrive_emits_intent_only_when_redrivable() {
        // FAILED → R emite RedriveExecution.
        let mut v = view_in_detail(ExecStatus::Failed);
        match v.on_key(key(KeyCode::Char('R'))).as_slice() {
            [Action::RedriveExecution { execution_arn }] => assert!(execution_arn.ends_with("e1")),
            other => panic!("se esperaba RedriveExecution, llegó {other:?}"),
        }
        // El estado fallido quedó preseleccionado ("saltar al que reventó").
        assert_eq!(v.state.selected(), Some(1));

        // SUCCEEDED → R no ofrece nada.
        let mut ok = view_in_detail(ExecStatus::Succeeded);
        assert!(ok.on_key(key(KeyCode::Char('R'))).is_empty());
        // RUNNING tampoco es redrivable.
        let mut run = view_in_detail(ExecStatus::Running);
        assert!(run.on_key(key(KeyCode::Char('R'))).is_empty());
    }

    #[test]
    fn lambda_logs_crosslink_only_for_lambda_states() {
        // FAILED → ProcessOrder (con resource_arn) queda preseleccionado: `l` salta a
        // los logs de su Lambda con el handoff agnóstico.
        let mut v = view_in_detail(ExecStatus::Failed);
        assert_eq!(v.state.selected(), Some(1), "ProcessOrder preseleccionado");
        match v.on_key(key(KeyCode::Char('l'))).as_slice() {
            [Action::ActivateViewWithContext { id, context }] => {
                assert_eq!(id, "logs");
                let ViewContext::LogGroupTail { group, window } = context else {
                    panic!("se esperaba LogGroupTail, llegó {context:?}");
                };
                assert_eq!(group, "/aws/lambda/ProcessOrder");
                // Ventana = rango del span (sin salir → `to: None`).
                assert!(matches!(window, LogWindow::Range { to: None, .. }));
            }
            other => panic!("se esperaba ActivateViewWithContext, llegó {other:?}"),
        }

        // Estado sin Lambda (Validate, seleccionado en SUCCEEDED) → `l` no ofrece nada.
        let mut ok = view_in_detail(ExecStatus::Succeeded);
        assert_eq!(ok.state.selected(), Some(0), "Validate seleccionado");
        assert!(ok.on_key(key(KeyCode::Char('l'))).is_empty());
    }

    #[test]
    fn hints_offer_lambda_logs_only_on_lambda_state() {
        let failed = view_in_detail(ExecStatus::Failed); // ProcessOrder (lambda)
        assert!(failed.hints().iter().any(|(k, _)| *k == "l"));
        let ok = view_in_detail(ExecStatus::Succeeded); // Validate (no lambda)
        assert!(!ok.hints().iter().any(|(k, _)| *k == "l"));
    }

    #[test]
    fn enter_in_detail_opens_state_io_panel_and_esc_closes() {
        // FAILED → ProcessOrder (con input) queda preseleccionado (idx 1 del timeline).
        let mut v = view_in_detail(ExecStatus::Failed);
        assert_eq!(v.state.selected(), Some(1));

        // `enter` abre el panel con el input/output del estado (no drillea, no redrive).
        let actions = v.on_key(key(KeyCode::Enter));
        assert!(
            actions.is_empty(),
            "enter en Detail abre el panel, no drillea"
        );
        let p = v.detail_panel.as_ref().expect("panel abierto");
        assert!(p.content().contains("input"), "muestra el input del estado");
        assert!(p.content().contains("process"));

        // `y` copia el contenido del panel.
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert!(text.contains("process")),
            other => panic!("se esperaba copiar el io del estado, llegó {other:?}"),
        }

        // `esc` cierra el panel pero NO sube de nivel.
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(actions.is_empty());
        assert!(v.detail_panel.is_none());
        assert!(
            matches!(v.level, Level::Detail { .. }),
            "sigue en el detalle"
        );
    }

    #[test]
    fn hints_offer_state_io_in_detail() {
        let v = view_in_detail(ExecStatus::Failed);
        assert!(
            v.hints().iter().any(|(k, _)| *k == "enter"),
            "el detalle anuncia enter: in/out cuando el estado expone io"
        );
    }

    #[test]
    fn history_partial_shows_in_timeline_title() {
        let mut v = view_in_detail(ExecStatus::Succeeded);
        assert!(!v.timeline_title().contains("parcial"));
        // Reenvía el detalle de la MISMA ejecución marcando el history como parcial.
        let arn = match &v.level {
            Level::Detail { execution_arn, .. } => execution_arn.clone(),
            _ => unreachable!(),
        };
        v.on_message(&Message::ExecutionDetailLoaded {
            execution_arn: arn,
            detail: ExecutionDetailDto {
                status: ExecStatus::Succeeded,
                start_ts: None,
                stop_ts: None,
                input: None,
                output: None,
                error: None,
                cause: None,
                redrive_count: None,
            },
            history: vec![],
            failed_state: None,
            history_more: true,
        });
        assert!(
            v.timeline_title().contains("parcial"),
            "history_more=true → timeline parcial"
        );
    }

    fn span_named(name: &str, from: i64) -> StateSpanDto {
        StateSpanDto {
            name: name.to_string(),
            entered_ts: Some(from),
            exited_ts: Some(from + 1_000),
            failed: false,
            input: None,
            output: None,
            resource_arn: None,
        }
    }

    fn ok_detail_dto() -> ExecutionDetailDto {
        ExecutionDetailDto {
            status: ExecStatus::Succeeded,
            start_ts: Some(0),
            stop_ts: Some(1),
            input: None,
            output: None,
            error: None,
            cause: None,
            redrive_count: None,
        }
    }

    /// Lleva la vista a `Detail` con un `history` dado y la señal `more`.
    fn view_in_detail_with_history(
        history: Vec<StateSpanDto>,
        failed: Option<&str>,
        more: bool,
    ) -> SfnView {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.on_key(key(KeyCode::Enter)); // → Executions
        v.on_message(&Message::ExecutionsLoaded {
            machine_arn: "arn:aws:states:us-east-1:000:stateMachine:m1".into(),
            executions: vec![exec("e1", ExecStatus::Succeeded)],
            next_token: None,
            append: false,
        });
        v.on_key(key(KeyCode::Enter)); // → Detail
        v.on_message(&Message::ExecutionDetailLoaded {
            execution_arn: exec("e1", ExecStatus::Succeeded).arn,
            detail: ok_detail_dto(),
            history,
            failed_state: failed.map(String::from),
            history_more: more,
        });
        v
    }

    #[test]
    fn o_in_detail_loads_more_history_only_when_partial() {
        let history = vec![span_named("Step06", 6_000), span_named("Step07", 7_000)];
        let mut v = view_in_detail_with_history(history, None, true);
        assert!(v.history_partial);

        // 1ª `o`: sube el presupuesto a 20 (10 inicial + paso 10).
        match v.on_key(key(KeyCode::Char('o'))).as_slice() {
            [
                Action::LoadMoreExecutionHistory {
                    execution_arn,
                    page_budget,
                },
            ] => {
                assert!(execution_arn.ends_with("e1"));
                assert_eq!(*page_budget, 20);
            }
            other => panic!("se esperaba LoadMoreExecutionHistory budget 20, llegó {other:?}"),
        }
        // 2ª `o` (sigue parcial): presupuesto 30.
        match v.on_key(key(KeyCode::Char('o'))).as_slice() {
            [Action::LoadMoreExecutionHistory { page_budget, .. }] => assert_eq!(*page_budget, 30),
            other => panic!("se esperaba budget 30, llegó {other:?}"),
        }

        // Sin parcial → `o` en Detail es no-op.
        let mut done = view_in_detail_with_history(vec![span_named("Step01", 1_000)], None, false);
        assert!(done.on_key(key(KeyCode::Char('o'))).is_empty());
    }

    #[test]
    fn load_more_preserves_selection_by_name() {
        // History inicial parcial (Step06..Step08); selecciono Step07.
        let initial = vec![
            span_named("Step06", 6_000),
            span_named("Step07", 7_000),
            span_named("Step08", 8_000),
        ];
        let mut v = view_in_detail_with_history(initial, None, true);
        v.on_key(key(KeyCode::Char('j'))); // 0 (Step06) → 1 (Step07)
        assert_eq!(v.selected_state_span().unwrap().name, "Step07");

        // Llega el load-more: el timeline crece con estados más viejos al frente.
        let arn = match &v.level {
            Level::Detail { execution_arn, .. } => execution_arn.clone(),
            _ => unreachable!(),
        };
        let grown: Vec<StateSpanDto> = (1..=8)
            .map(|n| span_named(&format!("Step{n:02}"), (n as i64) * 1_000))
            .collect();
        v.on_message(&Message::ExecutionDetailLoaded {
            execution_arn: arn,
            detail: ok_detail_dto(),
            history: grown,
            failed_state: None,
            history_more: false,
        });

        // Step07 sigue seleccionado (en un índice distinto) y ya no es parcial.
        assert_eq!(v.selected_state_span().unwrap().name, "Step07");
        assert!(!v.history_partial);
    }

    #[test]
    fn arrow_on_empty_filter_is_safe() {
        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![machine("m1", MachineType::Standard)]));
        v.set_filter("zzz"); // 0 coincidencias
        assert_eq!(v.visible_len(), 0);
        v.on_key(key(KeyCode::Down));
        assert_eq!(v.state.selected(), None);
    }

    #[test]
    fn status_color_maps_terminal_states() {
        assert_eq!(status_color(ExecStatus::Succeeded), Color::Green);
        assert_eq!(status_color(ExecStatus::Failed), Color::Red);
        assert_eq!(status_color(ExecStatus::Aborted), Color::Red);
        assert_eq!(status_color(ExecStatus::Running), Color::Yellow);
        assert_eq!(status_color(ExecStatus::PendingRedrive), Color::Cyan);
    }

    #[test]
    fn fmt_dur_formats_durations() {
        assert_eq!(fmt_dur(3_400), "3.4s");
        assert_eq!(fmt_dur(125_000), "2m 5s");
        assert_eq!(duration_label(Some(0), None), "en curso");
        assert_eq!(duration_label(None, None), "—");
    }

    #[test]
    fn render_machines_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = SfnView::new();
        v.on_message(&machines_msg(vec![
            machine("order-saga", MachineType::Standard),
            machine("ingest-fast", MachineType::Express),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("order-saga"));
        assert!(text.contains("express"), "badge de tipo visible");
    }

    #[test]
    fn render_detail_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = view_in_detail(ExecStatus::Failed);
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("ejecución"));
        assert!(text.contains("timeline"));
        assert!(text.contains("ProcessOrder"), "el estado fallido aparece");
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
}
