//! Vista `lambda`: funciones de Lambda -> drill a su configuración (`get_function`).
//! Pura y síncrona, espeja a `sqs`: estado, drill/back, filtro, navegación y render.
//! NUNCA importa `aws-sdk-*`; recibe DTOs planos vía `on_message`.
//!
//! Solo lectura (v0): no hay acciones mutantes. `l` salta a los logs de la función
//! (`/aws/lambda/<fn>`) reusando el handoff `ActivateViewWithContext` → `logs`.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::View;
use crate::action::{Action, ConsoleTarget, ViewContext};
use crate::message::{FunctionDetailDto, FunctionDto, LogWindow, Message};
use crate::ui::detail::DetailPanel;
use crate::util::{fuzzy_score, lambda_log_group_from_arn, ranked};

/// Ventana por defecto al saltar a los logs de una función (1h).
const LOGS_WINDOW_MS: i64 = 60 * 60_000;

/// Nivel de drill actual.
enum Level {
    Functions,
    Detail { function_arn: String },
}

pub struct LambdaView {
    level: Level,
    functions: Vec<FunctionDto>,
    /// Se alcanzó el tope de paginación de funciones (hay más sin traer).
    functions_partial: bool,
    detail: Option<FunctionDetailDto>,
    /// Panel con el valor completo de una env var (`enter` en el detalle): la fila lo
    /// colapsa a 60 chars; el panel lo muestra entero, scrolleable/copiable. `None` = normal.
    detail_panel: Option<DetailPanel>,
    filter: String,
    loading: bool,
    state: ListState,
}

impl LambdaView {
    pub fn new() -> Self {
        Self {
            level: Level::Functions,
            functions: Vec::new(),
            functions_partial: false,
            detail: None,
            detail_panel: None,
            filter: String::new(),
            loading: false,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    // --- Filtrado / selección -------------------------------------------------

    fn filtered_function_indices(&self) -> Vec<usize> {
        ranked(self.functions.len(), &self.filter, |i| {
            fuzzy_score(&self.functions[i].name, &self.filter)
        })
    }

    /// Variables de entorno del detalle (vacío si aún no cargó).
    fn env(&self) -> &[(String, String)] {
        self.detail
            .as_ref()
            .map(|d| d.env.as_slice())
            .unwrap_or(&[])
    }

    fn filtered_env_indices(&self) -> Vec<usize> {
        let env = self.env();
        // Una env var matchea por clave o por valor; tomamos el mejor score de los dos.
        ranked(env.len(), &self.filter, |i| {
            fuzzy_score(&env[i].0, &self.filter)
                .into_iter()
                .chain(fuzzy_score(&env[i].1, &self.filter))
                .max()
        })
    }

    fn visible_len(&self) -> usize {
        match self.level {
            Level::Functions => self.filtered_function_indices().len(),
            Level::Detail { .. } => self.filtered_env_indices().len(),
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

    fn selected_function(&self) -> Option<FunctionDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_function_indices().get(sel)?;
        Some(self.functions[idx].clone())
    }

    fn selected_env(&self) -> Option<(String, String)> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_env_indices().get(sel)?;
        self.env().get(idx).cloned()
    }

    /// Abre el panel con el valor completo de la env var seleccionada (la fila lo colapsa
    /// a 60 chars). `enter` sobre una env var en el detalle.
    fn open_env_detail(&mut self) {
        if let Some((k, v)) = self.selected_env() {
            self.detail_panel = Some(DetailPanel::new(format!("env {k}"), v));
        }
    }

    /// ARN a copiar con `y` (la función seleccionada o la del detalle).
    fn current_arn(&self) -> Option<String> {
        match &self.level {
            Level::Functions => self.selected_function().map(|f| f.arn),
            Level::Detail { function_arn } => Some(function_arn.clone()),
        }
    }

    /// Nombre de la función del nivel actual, para abrir en la consola.
    fn current_name(&self) -> Option<String> {
        match &self.level {
            Level::Functions => self.selected_function().map(|f| f.name),
            Level::Detail { function_arn } => function_arn.rsplit(':').next().map(str::to_string),
        }
    }

    // --- Navegación -----------------------------------------------------------

    fn drill(&mut self) -> Vec<Action> {
        match self.level {
            Level::Functions => match self.selected_function() {
                Some(f) => {
                    let arn = f.arn.clone();
                    self.level = Level::Detail {
                        function_arn: arn.clone(),
                    };
                    self.detail = None;
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![
                        Action::ClearFilter,
                        Action::RecordRecent {
                            key: arn.clone(),
                            label: f.name,
                        },
                        Action::LoadFunctionDetail { function_arn: arn },
                    ]
                }
                None => vec![],
            },
            Level::Detail { .. } => vec![],
        }
    }

    /// `esc`: despoja un nivel de drill. En la raíz (functions) no hay nada que
    /// despojar → emite `Back` para que el `App` vuelva al menú.
    fn back(&mut self) -> Vec<Action> {
        self.detail_panel = None; // cerrar cualquier panel al subir de nivel
        if matches!(self.level, Level::Detail { .. }) {
            self.level = Level::Functions;
            self.loading = false;
            self.state.select(Some(0));
            self.clamp_selection();
            vec![Action::ClearFilter]
        } else {
            vec![Action::Back]
        }
    }

    fn refresh(&mut self) -> Vec<Action> {
        self.loading = true;
        match &self.level {
            Level::Functions => vec![Action::LoadFunctions],
            Level::Detail { function_arn } => vec![Action::LoadFunctionDetail {
                function_arn: function_arn.clone(),
            }],
        }
    }

    /// `l`: salta a los logs de la función (`/aws/lambda/<fn>`) reusando el handoff a
    /// `logs`. Read-only → sin gate. No ofrece nada si no hay función.
    fn open_logs(&self) -> Vec<Action> {
        let Some(group) = self
            .current_arn()
            .as_deref()
            .and_then(lambda_log_group_from_arn)
        else {
            return vec![];
        };
        vec![Action::ActivateViewWithContext {
            id: "logs".to_string(),
            context: ViewContext::LogGroupTail {
                group,
                window: LogWindow::Last(LOGS_WINDOW_MS),
            },
        }]
    }

    // --- Render ---------------------------------------------------------------

    fn functions_title(&self) -> String {
        let total = self.functions.len();
        let shown = self.filtered_function_indices().len();
        let partial = if self.functions_partial {
            " · parcial"
        } else {
            ""
        };
        // Mientras llegan más páginas (streaming) con la lista ya visible, avisamos.
        let loading = if self.loading && total > 0 {
            " · cargando…"
        } else {
            ""
        };
        if self.filter.is_empty() {
            format!(" {total} funciones{partial}{loading} ")
        } else {
            format!(
                " {shown}/{total} funciones{partial}{loading} · filtro: {} ",
                self.filter
            )
        }
    }

    fn env_title(&self) -> String {
        let total = self.env().len();
        let shown = self.filtered_env_indices().len();
        if self.filter.is_empty() {
            format!(" env vars · {total} · enter expande ")
        } else {
            format!(" env vars · {shown}/{total} · filtro: {} ", self.filter)
        }
    }

    /// Bloque de configuración (arriba del detalle), análogo a `attrs` de sqs.
    fn config_lines(&self) -> Vec<Line<'static>> {
        let Some(d) = &self.detail else {
            let msg = if self.loading { "cargando…" } else { "—" };
            return vec![Line::from(msg.to_string())];
        };
        let dash = || "—".to_string();
        let opt = |o: &Option<String>| o.clone().unwrap_or_else(dash);
        let row = |k: &'static str, v: String| {
            Line::from(vec![
                Span::styled(format!("{k:<11}"), Style::new().dark_gray()),
                Span::raw(v),
            ])
        };
        let mem = d.memory.map(|m| m.to_string()).unwrap_or_else(dash);
        let timeout = d.timeout.map(|t| t.to_string()).unwrap_or_else(dash);
        let code = d.code_size.map(fmt_size).unwrap_or_else(dash);
        let mut lines = vec![
            row("runtime", opt(&d.runtime)),
            row("handler", opt(&d.handler)),
            row("memoria", format!("{mem} MB · timeout {timeout}s")),
            row("code", format!("{code} · mod {}", opt(&d.last_modified))),
            row("role", opt(&d.role)),
            row(
                "tracing",
                format!(
                    "{} · DLQ {}",
                    opt(&d.tracing),
                    d.dlq_target.clone().unwrap_or_else(dash)
                ),
            ),
        ];
        if !d.layers.is_empty() {
            lines.push(row("layers", d.layers.len().to_string()));
        }
        if let Some(desc) = &d.description {
            lines.push(row("desc", desc.clone()));
        }
        lines
    }
}

impl Default for LambdaView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for LambdaView {
    fn id(&self) -> &'static str {
        "lambda"
    }

    fn description(&self) -> &'static str {
        "Funciones Lambda: config + logs"
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        // Con el panel de valor abierto, sus teclas (scroll/copiar/cerrar) mandan.
        if let Some(p) = &self.detail_panel {
            return p.hints();
        }
        match self.level {
            Level::Functions => vec![("y", "copiar ARN"), ("O", "consola"), ("l", "logs")],
            Level::Detail { .. } => {
                let mut hints = vec![];
                if !self.env().is_empty() {
                    hints.push(("enter", "ver valor"));
                }
                hints.extend([("y", "copiar ARN"), ("O", "consola"), ("l", "logs")]);
                hints
            }
        }
    }

    fn title(&self) -> String {
        match &self.level {
            Level::Functions => "lambda".to_string(),
            Level::Detail { function_arn } => {
                let name = function_arn.rsplit(':').next().unwrap_or(function_arn);
                format!("lambda / {name}")
            }
        }
    }

    fn on_activate(&mut self) -> Vec<Action> {
        self.level = Level::Functions;
        self.detail = None;
        self.detail_panel = None;
        self.loading = true;
        self.state.select(Some(0));
        vec![Action::LoadFunctions]
    }

    /// Abrir un favorito/reciente: la `key` es el ARN de la función → drillea directo a su
    /// detalle. Otros contextos (LogGroupTail) no le conciernen → activación normal.
    fn on_context(&mut self, context: &ViewContext) -> Vec<Action> {
        match context {
            ViewContext::Favorite { key } => {
                self.level = Level::Detail {
                    function_arn: key.clone(),
                };
                self.detail = None;
                self.detail_panel = None;
                self.loading = true;
                self.state.select(Some(0));
                vec![Action::LoadFunctionDetail {
                    function_arn: key.clone(),
                }]
            }
            _ => self.on_activate(),
        }
    }

    /// Favorito = la función seleccionada (solo en el nivel de funciones).
    fn selected_favorite(&self) -> Option<(String, String)> {
        match self.level {
            Level::Functions => self.selected_function().map(|f| (f.arn, f.name)),
            Level::Detail { .. } => None,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // Panel de valor abierto: `y` copia su contenido, el resto scrollea/cierra.
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
            // En el detalle, `enter` expande el valor de la env var (la lista navegable son
            // las env vars); en functions, drillea.
            KeyCode::Enter if matches!(self.level, Level::Detail { .. }) => {
                self.open_env_detail();
                vec![]
            }
            KeyCode::Enter => self.drill(),
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('l') => self.open_logs(),
            KeyCode::Char('y') => self
                .current_arn()
                .map(|text| Action::CopyToClipboard { text })
                .into_iter()
                .collect(),
            KeyCode::Char('O') => self
                .current_name()
                .map(|name| Action::OpenConsole {
                    target: ConsoleTarget::LambdaFunction { name },
                })
                .into_iter()
                .collect(),
            _ => vec![],
        }
    }

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::FunctionsLoaded {
                functions,
                append,
                more,
                partial,
            } => {
                // Streaming: la 1ª página reemplaza, las siguientes se anexan.
                if *append {
                    self.functions.extend(functions.iter().cloned());
                } else {
                    self.functions = functions.clone();
                }
                self.functions_partial = *partial;
                if matches!(self.level, Level::Functions) {
                    // Sigue cargando mientras lleguen más páginas (la lista ya se navega).
                    self.loading = *more;
                    self.clamp_selection();
                }
            }
            Message::FunctionDetailLoaded {
                function_arn,
                detail,
            } => {
                // Aceptar solo si corresponde a la función del drill actual.
                if let Level::Detail {
                    function_arn: current,
                } = &self.level
                    && current == function_arn
                {
                    self.detail = Some(detail.clone());
                    self.loading = false;
                    self.clamp_selection();
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
        // Panel del valor de una env var: ocupa el cuerpo entero.
        if let Some(p) = self.detail_panel.as_mut() {
            p.render(frame, area);
            return;
        }
        if matches!(self.level, Level::Functions) {
            let block = Block::bordered().title(self.functions_title());
            let items: Vec<ListItem> = self
                .filtered_function_indices()
                .into_iter()
                .map(|i| function_item(&self.functions[i]))
                .collect();
            if items.is_empty() {
                let msg = empty_msg(self.loading, &self.filter);
                frame.render_widget(Paragraph::new(msg).block(block), area);
                return;
            }
            let list = List::new(items)
                .block(block)
                .highlight_style(Style::new().reversed())
                .highlight_symbol("› ");
            frame.render_stateful_widget(list, area, &mut self.state);
            return;
        }

        // Detalle: bloque de config arriba, lista de env vars abajo.
        let config_lines = self.config_lines();
        let config_h = (config_lines.len() as u16 + 2).min(area.height.saturating_sub(3).max(3));
        let [cfg_area, env_area] =
            Layout::vertical([Constraint::Length(config_h), Constraint::Min(0)]).areas(area);
        frame.render_widget(
            Paragraph::new(config_lines).block(Block::bordered().title(" configuración ")),
            cfg_area,
        );

        let block = Block::bordered().title(self.env_title());
        let items: Vec<ListItem> = self
            .filtered_env_indices()
            .into_iter()
            .map(|i| env_item(&self.env()[i]))
            .collect();
        if items.is_empty() {
            let msg = if self.loading {
                "cargando…"
            } else if !self.filter.is_empty() {
                "(sin coincidencias para el filtro)"
            } else {
                "(sin variables de entorno)"
            };
            frame.render_widget(Paragraph::new(msg).block(block), env_area);
            return;
        }
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed())
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, env_area, &mut self.state);
    }
}

// --- Construcción de filas y helpers ------------------------------------------

fn empty_msg(loading: bool, filter: &str) -> &'static str {
    if loading {
        "cargando…"
    } else if filter.is_empty() {
        "(sin resultados)"
    } else {
        "(sin coincidencias para el filtro)"
    }
}

/// Tamaño legible del paquete de código.
fn fmt_size(bytes: i64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn function_item(f: &FunctionDto) -> ListItem<'static> {
    let mut spans = vec![Span::raw(f.name.clone())];
    if let Some(rt) = &f.runtime {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("[{rt}]"), Style::new().dark_gray()));
    }
    if let Some(mem) = f.memory {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("{mem}MB"), Style::new().dark_gray()));
    }
    if let Some(lm) = &f.last_modified {
        // Solo la fecha (los primeros 10 chars de la marca ISO); el resto satura la fila.
        let date: String = lm.chars().take(10).collect();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(date, Style::new().dark_gray()));
    }
    ListItem::new(Line::from(spans))
}

fn env_item(kv: &(String, String)) -> ListItem<'static> {
    let (k, v) = kv;
    let val = v.replace('\n', " ");
    let val = if val.chars().count() > 60 {
        format!("{}…", val.chars().take(60).collect::<String>())
    } else {
        val
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{k} = "), Style::new().dark_gray()),
        Span::raw(val),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(name: &str) -> FunctionDto {
        FunctionDto {
            name: name.to_string(),
            arn: format!("arn:aws:lambda:us-east-1:000000000000:function:{name}"),
            runtime: Some("python3.12".to_string()),
            last_modified: Some("2026-05-01T12:00:00.000+0000".to_string()),
            memory: Some(256),
        }
    }

    fn detail_with_env(env: Vec<(&str, &str)>) -> FunctionDetailDto {
        FunctionDetailDto {
            runtime: Some("python3.12".to_string()),
            handler: Some("app.handler".to_string()),
            memory: Some(256),
            timeout: Some(30),
            code_size: Some(4_823_551),
            last_modified: Some("2026-05-01T12:00:00.000+0000".to_string()),
            role: Some("arn:aws:iam::000:role/exec".to_string()),
            description: Some("fn de prueba".to_string()),
            layers: vec!["arn:aws:lambda:us-east-1:000:layer:deps:7".to_string()],
            tracing: Some("Active".to_string()),
            dlq_target: None,
            env: env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        use ratatui::crossterm::event::KeyModifiers;
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn activate_requests_functions() {
        let mut v = LambdaView::new();
        assert!(matches!(
            v.on_activate().as_slice(),
            [Action::LoadFunctions]
        ));
    }

    #[test]
    fn ingests_functions_via_message() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order"), function("process-payment")],
            append: false,
            more: false,
            partial: false,
        });
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn partial_pagination_signals_in_title() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        assert!(!v.functions_title().contains("parcial"));
        // Última página que cortó por el tope (`partial`) → la vista lo señala.
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: true,
        });
        assert!(v.functions_title().contains("parcial"));
    }

    #[test]
    fn streaming_pages_append_and_finish() {
        let mut v = LambdaView::new();
        v.loading = true; // como tras on_activate
        // 1ª página (more=true): ya se muestra y sigue "cargando".
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("a"), function("b")],
            append: false,
            more: true,
            partial: false,
        });
        assert_eq!(v.visible_len(), 2);
        assert!(v.loading, "sigue cargando mientras vienen más páginas");
        assert!(v.functions_title().contains("cargando"));
        // 2ª página (more=false): se anexa y termina de cargar.
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("c")],
            append: true,
            more: false,
            partial: false,
        });
        assert_eq!(v.visible_len(), 3, "la 2ª página se anexa");
        assert!(!v.loading, "terminó de cargar");
    }

    #[test]
    fn filter_narrows_function_list() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![
                function("create-order"),
                function("create-invoice"),
                function("process-payment"),
            ],
            append: false,
            more: false,
            partial: false,
        });
        v.set_filter("CREATE"); // case-insensitive
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn enter_drills_into_function_detail() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order"), function("process-payment")],
            append: false,
            more: false,
            partial: false,
        });
        v.on_key(key(KeyCode::Down)); // selecciona process-payment
        let actions = v.on_key(key(KeyCode::Enter));
        match actions.as_slice() {
            [
                Action::ClearFilter,
                Action::RecordRecent { key, label },
                Action::LoadFunctionDetail { function_arn },
            ] => {
                assert!(function_arn.ends_with(":function:process-payment"));
                assert_eq!(
                    label, "process-payment",
                    "recuerda la función por su nombre"
                );
                assert_eq!(key, function_arn, "key = ARN");
            }
            other => {
                panic!("se esperaba ClearFilter+RecordRecent+LoadFunctionDetail, llegó {other:?}")
            }
        }
        assert!(matches!(v.level, Level::Detail { .. }));

        v.on_message(&Message::FunctionDetailLoaded {
            function_arn: function("process-payment").arn,
            detail: detail_with_env(vec![("LOG_LEVEL", "info"), ("STAGE", "prod")]),
        });
        assert_eq!(v.visible_len(), 2, "dos env vars navegables");

        v.on_key(key(KeyCode::Esc));
        assert!(matches!(v.level, Level::Functions));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn favorite_getter_and_open_via_context() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        // En el nivel de funciones el favorito es la función seleccionada (arn + nombre).
        let fav = v.selected_favorite().expect("hay una función seleccionada");
        assert!(fav.0.ends_with(":function:create-order"), "key = arn");
        assert_eq!(fav.1, "create-order", "label = nombre");

        // Abrir un favorito via contexto → drillea directo al detalle de ese ARN.
        let arn = function("create-order").arn;
        match v
            .on_context(&ViewContext::Favorite { key: arn.clone() })
            .as_slice()
        {
            [Action::LoadFunctionDetail { function_arn }] => assert_eq!(*function_arn, arn),
            other => panic!("se esperaba LoadFunctionDetail, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Detail { .. }));
    }

    #[test]
    fn enter_in_detail_expands_env_value_and_esc_closes() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        v.on_key(key(KeyCode::Enter)); // → Detail
        let long = format!("flags {}", "x".repeat(200));
        v.on_message(&Message::FunctionDetailLoaded {
            function_arn: function("create-order").arn,
            detail: detail_with_env(vec![("FEATURE_FLAGS", long.as_str())]),
        });

        // `enter` expande el valor completo (no drillea).
        let actions = v.on_key(key(KeyCode::Enter));
        assert!(actions.is_empty());
        assert_eq!(
            v.detail_panel.as_ref().expect("panel").content(),
            long,
            "muestra el valor completo, no los 60 chars de la fila"
        );
        // `y` copia el valor completo.
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert_eq!(*text, long),
            other => panic!("se esperaba copiar el valor, llegó {other:?}"),
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
    fn detail_from_wrong_function_is_ignored() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        v.on_key(key(KeyCode::Enter)); // drill create-order
        v.on_message(&Message::FunctionDetailLoaded {
            function_arn: function("otra").arn, // función equivocada
            detail: detail_with_env(vec![("K", "v")]),
        });
        assert_eq!(v.visible_len(), 0, "no se acepta detalle de otra función");
    }

    #[test]
    fn esc_at_root_emits_back() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
        assert!(matches!(v.level, Level::Functions));
    }

    #[test]
    fn esc_in_detail_pops_to_functions() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        v.on_key(key(KeyCode::Enter)); // drill al detalle
        assert!(matches!(v.level, Level::Detail { .. }));
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::ClearFilter]));
        assert!(matches!(v.level, Level::Functions));
    }

    #[test]
    fn y_copies_function_arn() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => {
                assert!(text.ends_with(":function:create-order"))
            }
            other => panic!("se esperaba CopyToClipboard, llegó {other:?}"),
        }
    }

    #[test]
    fn o_opens_function_in_console() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        match v.on_key(key(KeyCode::Char('O'))).as_slice() {
            [
                Action::OpenConsole {
                    target: ConsoleTarget::LambdaFunction { name },
                },
            ] => assert_eq!(name, "create-order"),
            other => panic!("se esperaba OpenConsole LambdaFunction, llegó {other:?}"),
        }
    }

    #[test]
    fn l_opens_lambda_logs_crosslink() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        // En la lista: `l` abre los logs de la función seleccionada.
        match v.on_key(key(KeyCode::Char('l'))).as_slice() {
            [Action::ActivateViewWithContext { id, context }] => {
                assert_eq!(id, "logs");
                let ViewContext::LogGroupTail { group, window } = context else {
                    panic!("se esperaba LogGroupTail, llegó {context:?}");
                };
                assert_eq!(group, "/aws/lambda/create-order");
                assert!(matches!(window, LogWindow::Last(_)));
            }
            other => panic!("se esperaba ActivateViewWithContext logs, llegó {other:?}"),
        }
        // También funciona dentro del detalle (usa el ARN del nivel).
        v.on_key(key(KeyCode::Enter));
        assert!(
            !v.on_key(key(KeyCode::Char('l'))).is_empty(),
            "`l` también abre logs en el detalle"
        );
    }

    #[test]
    fn hints_offer_logs_link() {
        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        assert!(v.hints().iter().any(|(k, _)| *k == "l"));
    }

    #[test]
    fn render_detail_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LambdaView::new();
        v.on_message(&Message::FunctionsLoaded {
            functions: vec![function("create-order")],
            append: false,
            more: false,
            partial: false,
        });
        v.on_key(key(KeyCode::Enter));
        v.on_message(&Message::FunctionDetailLoaded {
            function_arn: function("create-order").arn,
            detail: detail_with_env(vec![("LOG_LEVEL", "info"), ("STAGE", "prod")]),
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("configuración"));
        assert!(text.contains("env vars"));
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
