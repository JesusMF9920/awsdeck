//! Vista `events`: EventBridge. Drill de 3 niveles: event buses → rules (estado
//! coloreado) → detalle (event_pattern + targets). Pura y síncrona; NUNCA importa
//! `aws-sdk-*` (recibe DTOs planos vía `on_message`).
//!
//! `S` en el nivel de buses emite la intención `SendEvent` (un evento de prueba
//! canned contra el bus seleccionado); la vista NO sabe de modo escritura ni
//! confirm: ese gate vive en el `App` (reusa el de `PurgeQueue`/`RedriveExecution`).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::View;
use crate::action::{Action, ConsoleTarget};
use crate::message::{EventBusDto, Message, RuleDetailDto, RuleDto, RuleState, TargetDto};
use crate::util::{fuzzy_score, ranked};

/// Nivel de drill actual. Cada nivel carga los identificadores que `back()`
/// necesita para reconstruir el padre y que `on_message` usa como guard.
enum Level {
    Buses,
    Rules {
        event_bus_name: String,
    },
    Detail {
        event_bus_name: String,
        rule_name: String,
    },
}

pub struct EventsView {
    level: Level,
    buses: Vec<EventBusDto>,
    rules: Vec<RuleDto>,
    detail: Option<RuleDetailDto>,
    targets: Vec<TargetDto>,
    filter: String,
    loading: bool,
    /// Se alcanzó el tope de paginación de buses (hay más sin traer).
    buses_partial: bool,
    /// Se alcanzó el tope de paginación de rules.
    rules_partial: bool,
    state: ListState,
}

impl EventsView {
    pub fn new() -> Self {
        Self {
            level: Level::Buses,
            buses: Vec::new(),
            rules: Vec::new(),
            detail: None,
            targets: Vec::new(),
            filter: String::new(),
            loading: false,
            buses_partial: false,
            rules_partial: false,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    // --- Filtrado / selección -------------------------------------------------

    fn filtered_bus_indices(&self) -> Vec<usize> {
        ranked(self.buses.len(), &self.filter, |i| {
            fuzzy_score(&self.buses[i].name, &self.filter)
        })
    }

    fn filtered_rule_indices(&self) -> Vec<usize> {
        ranked(self.rules.len(), &self.filter, |i| {
            fuzzy_score(&self.rules[i].name, &self.filter)
        })
    }

    /// Los targets del detalle también se filtran (por id) → `/` consistente en
    /// los 3 niveles.
    fn filtered_target_indices(&self) -> Vec<usize> {
        ranked(self.targets.len(), &self.filter, |i| {
            fuzzy_score(&self.targets[i].id, &self.filter)
        })
    }

    /// Tamaño de la lista navegable del nivel activo.
    fn visible_len(&self) -> usize {
        match self.level {
            Level::Buses => self.filtered_bus_indices().len(),
            Level::Rules { .. } => self.filtered_rule_indices().len(),
            Level::Detail { .. } => self.filtered_target_indices().len(),
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

    fn selected_bus(&self) -> Option<EventBusDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_bus_indices().get(sel)?;
        Some(self.buses[idx].clone())
    }

    fn selected_rule(&self) -> Option<RuleDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_rule_indices().get(sel)?;
        Some(self.rules[idx].clone())
    }

    /// Texto a copiar con `y`: ARN del bus / nombre de la rule / ARN del target
    /// seleccionado (en el detalle, la lista navegable son los targets).
    fn copy_text(&self) -> Option<String> {
        match &self.level {
            Level::Buses => self.selected_bus().map(|b| b.arn),
            Level::Rules { .. } => self.selected_rule().map(|r| r.name),
            Level::Detail { .. } => {
                let idx = *self.filtered_target_indices().get(self.state.selected()?)?;
                Some(self.targets[idx].arn.clone())
            }
        }
    }

    /// Recurso del nivel actual a abrir en la consola de EventBridge (bus / rule).
    fn console_target(&self) -> Option<ConsoleTarget> {
        match &self.level {
            Level::Buses => self
                .selected_bus()
                .map(|b| ConsoleTarget::EventBus { name: b.name }),
            Level::Rules { event_bus_name } => self.selected_rule().map(|r| ConsoleTarget::Rule {
                event_bus: event_bus_name.clone(),
                name: r.name,
            }),
            Level::Detail {
                event_bus_name,
                rule_name,
            } => Some(ConsoleTarget::Rule {
                event_bus: event_bus_name.clone(),
                name: rule_name.clone(),
            }),
        }
    }

    // --- Navegación -----------------------------------------------------------

    fn drill(&mut self) -> Vec<Action> {
        if matches!(self.level, Level::Buses) {
            return match self.selected_bus() {
                Some(b) => {
                    self.level = Level::Rules {
                        event_bus_name: b.name.clone(),
                    };
                    self.rules.clear();
                    self.rules_partial = false;
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![
                        Action::ClearFilter,
                        Action::LoadRules {
                            event_bus_name: b.name,
                        },
                    ]
                }
                None => vec![],
            };
        }

        // Rules → Detail (clona el contexto del nivel antes de mutarlo).
        let bus = if let Level::Rules { event_bus_name } = &self.level {
            Some(event_bus_name.clone())
        } else {
            None
        };
        if let Some(event_bus_name) = bus {
            return match self.selected_rule() {
                Some(r) => {
                    self.level = Level::Detail {
                        event_bus_name,
                        rule_name: r.name.clone(),
                    };
                    self.detail = None;
                    self.targets.clear();
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![
                        Action::ClearFilter,
                        Action::LoadRuleDetail {
                            event_bus_name: r.event_bus_name,
                            rule_name: r.name,
                        },
                    ]
                }
                None => vec![],
            };
        }

        vec![] // Detail: enter no hace nada (v3 es solo-lectura del detalle)
    }

    /// `esc`: despoja un nivel; en la raíz (buses) emite `Back` (→ menú).
    fn back(&mut self) -> Vec<Action> {
        let bus = if let Level::Detail { event_bus_name, .. } = &self.level {
            Some(event_bus_name.clone())
        } else {
            None
        };
        if let Some(event_bus_name) = bus {
            self.level = Level::Rules { event_bus_name };
            self.detail = None;
            self.targets.clear();
            self.loading = false;
            self.state.select(Some(0));
            self.clamp_selection();
            return vec![Action::ClearFilter];
        }
        if matches!(self.level, Level::Rules { .. }) {
            self.level = Level::Buses;
            self.loading = false;
            self.state.select(Some(0));
            self.clamp_selection();
            return vec![Action::ClearFilter];
        }
        vec![Action::Back]
    }

    fn refresh(&mut self) -> Vec<Action> {
        let actions = match &self.level {
            Level::Buses => vec![Action::LoadEventBuses],
            Level::Rules { event_bus_name } => vec![Action::LoadRules {
                event_bus_name: event_bus_name.clone(),
            }],
            Level::Detail {
                event_bus_name,
                rule_name,
            } => vec![Action::LoadRuleDetail {
                event_bus_name: event_bus_name.clone(),
                rule_name: rule_name.clone(),
            }],
        };
        self.loading = true;
        actions
    }

    /// `S` en el nivel de buses: emite la intención de enviar un evento de prueba
    /// contra el bus seleccionado. El App la gatea (modo escritura + confirm).
    fn send_intent(&self) -> Vec<Action> {
        if matches!(self.level, Level::Buses)
            && let Some(b) = self.selected_bus()
        {
            return vec![Action::SendEvent {
                event_bus_name: b.name,
            }];
        }
        vec![]
    }

    // --- Render ---------------------------------------------------------------

    fn buses_title(&self) -> String {
        let total = self.buses.len();
        let shown = self.filtered_bus_indices().len();
        let partial = if self.buses_partial {
            " · parcial"
        } else {
            ""
        };
        if self.filter.is_empty() {
            format!(" {total} event buses{partial} · [S] enviar evento ")
        } else {
            format!(" {shown}/{total} buses{partial} · filtro: {} ", self.filter)
        }
    }

    fn rules_title(&self) -> String {
        let name = match &self.level {
            Level::Rules { event_bus_name } | Level::Detail { event_bus_name, .. } => {
                event_bus_name.as_str()
            }
            Level::Buses => "",
        };
        let total = self.rules.len();
        let shown = self.filtered_rule_indices().len();
        let partial = if self.rules_partial {
            " · parcial"
        } else {
            ""
        };
        if self.filter.is_empty() {
            format!(" {name} · {total} rules{partial} ")
        } else {
            format!(
                " {name} · {shown}/{total}{partial} · filtro: {} ",
                self.filter
            )
        }
    }

    fn targets_title(&self) -> String {
        let total = self.targets.len();
        if self.filter.is_empty() {
            format!(" targets · {total} ")
        } else {
            let shown = self.filtered_target_indices().len();
            format!(" targets · {shown}/{total} · filtro: {} ", self.filter)
        }
    }

    fn detail_meta(&self) -> Paragraph<'static> {
        let block = Block::bordered().title(" rule ");
        let Some(d) = &self.detail else {
            let msg = if self.loading { "cargando…" } else { "—" };
            return Paragraph::new(msg).block(block);
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("{:<9}", "estado"), Style::new().dark_gray()),
            Span::styled(d.state.label(), Style::new().fg(state_color(d.state))),
        ])];
        if let Some(desc) = &d.description {
            lines.push(row("descr", oneline(desc, 80)));
        }
        if let Some(sched) = &d.schedule_expression {
            lines.push(row("schedule", oneline(sched, 80)));
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

impl Default for EventsView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for EventsView {
    fn id(&self) -> &'static str {
        "events"
    }

    fn description(&self) -> &'static str {
        "EventBridge: buses, rules, patrón y send"
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match self.level {
            // `S` envía un evento de prueba al bus (gated por modo escritura + confirm).
            Level::Buses => vec![("y", "copiar ARN"), ("O", "consola"), ("S", "enviar evento")],
            Level::Rules { .. } => vec![("y", "copiar nombre"), ("O", "consola")],
            Level::Detail { .. } => vec![("y", "copiar ARN target"), ("O", "consola")],
        }
    }

    fn title(&self) -> String {
        match &self.level {
            Level::Buses => "events".to_string(),
            Level::Rules { event_bus_name } => format!("events / {event_bus_name}"),
            Level::Detail {
                event_bus_name,
                rule_name,
            } => format!("events / {event_bus_name} / {rule_name}"),
        }
    }

    fn on_activate(&mut self) -> Vec<Action> {
        self.level = Level::Buses;
        self.rules.clear();
        self.detail = None;
        self.targets.clear();
        self.loading = true;
        self.buses_partial = false;
        self.rules_partial = false;
        self.state.select(Some(0));
        vec![Action::LoadEventBuses]
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
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
            KeyCode::Enter => self.drill(),
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('S') => self.send_intent(),
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

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::EventBusesLoaded { buses, more } => {
                self.buses = buses.clone();
                self.buses_partial = *more;
                if matches!(self.level, Level::Buses) {
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::RulesLoaded {
                event_bus_name,
                rules,
                more,
            } => {
                if let Level::Rules {
                    event_bus_name: current,
                } = &self.level
                    && current == event_bus_name
                {
                    self.rules = rules.clone();
                    self.rules_partial = *more;
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::RuleDetailLoaded {
                event_bus_name,
                rule_name,
                detail,
                targets,
            } => {
                if let Level::Detail {
                    event_bus_name: cur_bus,
                    rule_name: cur_rule,
                } = &self.level
                    && cur_bus == event_bus_name
                    && cur_rule == rule_name
                {
                    self.detail = Some(detail.clone());
                    self.targets = targets.clone();
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            // El App muestra el error en la status bar; aquí cortamos el loading.
            Message::Error(_) => self.loading = false,
            // EventSent y mensajes de otras vistas: el App ya muestra la info.
            _ => {}
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.state.select(Some(0)); // top = mejor match (estilo fzf)
        self.clamp_selection();
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        match &self.level {
            Level::Buses => {
                let block = Block::bordered().title(self.buses_title());
                let items: Vec<ListItem> = self
                    .filtered_bus_indices()
                    .into_iter()
                    .map(|i| bus_item(&self.buses[i]))
                    .collect();
                self.render_list(frame, area, block, items);
            }
            Level::Rules { .. } => {
                let block = Block::bordered().title(self.rules_title());
                let items: Vec<ListItem> = self
                    .filtered_rule_indices()
                    .into_iter()
                    .map(|i| rule_item(&self.rules[i]))
                    .collect();
                self.render_list(frame, area, block, items);
            }
            Level::Detail { .. } => {
                let [meta, pattern, targets] = Layout::vertical([
                    Constraint::Length(5),
                    Constraint::Percentage(45),
                    Constraint::Min(0),
                ])
                .areas(area);

                frame.render_widget(self.detail_meta(), meta);

                let pat_block = Block::bordered().title(" patrón ");
                let pat = self
                    .detail
                    .as_ref()
                    .and_then(|d| d.event_pattern.clone())
                    .unwrap_or_else(|| "(sin event_pattern)".to_string());
                frame.render_widget(Paragraph::new(pat).block(pat_block), pattern);

                let block = Block::bordered().title(self.targets_title());
                let items: Vec<ListItem> = self
                    .filtered_target_indices()
                    .into_iter()
                    .map(|i| target_item(&self.targets[i]))
                    .collect();
                self.render_list(frame, targets, block, items);
            }
        }
    }
}

// --- Helpers de presentación --------------------------------------------------

fn state_color(s: RuleState) -> Color {
    match s {
        RuleState::Enabled => Color::Green,
        RuleState::Disabled => Color::DarkGray,
    }
}

/// Colapsa un texto multilínea a una sola línea truncada (para previews).
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

fn bus_item(b: &EventBusDto) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::raw(b.name.clone()),
        Span::raw("  "),
        Span::styled(b.arn.clone(), Style::new().dark_gray()),
    ]))
}

fn rule_item(r: &RuleDto) -> ListItem<'static> {
    let badge = match r.state {
        RuleState::Enabled => Span::styled("[enabled]", Style::new().green()),
        RuleState::Disabled => Span::styled("[disabled]", Style::new().red()),
    };
    let desc = r.description.clone().unwrap_or_default();
    ListItem::new(Line::from(vec![
        Span::raw(format!("{:<28}", r.name)),
        badge,
        Span::raw("  "),
        Span::styled(oneline(&desc, 50), Style::new().dark_gray()),
    ]))
}

fn target_item(t: &TargetDto) -> ListItem<'static> {
    let mut spans = vec![
        Span::raw(format!("{:<16}", t.id)),
        Span::styled(t.arn.clone(), Style::new().dark_gray()),
    ];
    if let Some(input) = &t.input {
        spans.push(Span::styled(
            format!("  input: {}", oneline(input, 40)),
            Style::new().dark_gray(),
        ));
    }
    ListItem::new(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn bus(name: &str) -> EventBusDto {
        EventBusDto {
            arn: format!("arn:aws:events:us-east-1:000:event-bus/{name}"),
            name: name.to_string(),
        }
    }

    fn rule(name: &str, state: RuleState) -> RuleDto {
        RuleDto {
            name: name.to_string(),
            event_bus_name: "default".to_string(),
            state,
            description: Some(format!("desc {name}")),
        }
    }

    fn buses_msg(buses: Vec<EventBusDto>) -> Message {
        Message::EventBusesLoaded { buses, more: false }
    }

    fn key(code: KeyCode) -> KeyEvent {
        use ratatui::crossterm::event::KeyModifiers;
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Lleva la vista a `Detail` con un detalle + targets.
    fn view_in_detail() -> EventsView {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // → Rules
        v.on_message(&Message::RulesLoaded {
            event_bus_name: "default".into(),
            rules: vec![rule("orders-created", RuleState::Enabled)],
            more: false,
        });
        v.on_key(key(KeyCode::Enter)); // → Detail
        v.on_message(&Message::RuleDetailLoaded {
            event_bus_name: "default".into(),
            rule_name: "orders-created".into(),
            detail: RuleDetailDto {
                state: RuleState::Enabled,
                description: Some("route orders".into()),
                event_pattern: Some("{\n  \"source\": [\"my.app\"]\n}".into()),
                schedule_expression: None,
            },
            targets: vec![
                TargetDto {
                    id: "to-lambda".into(),
                    arn: "arn:…:function:fulfill".into(),
                    input: None,
                },
                TargetDto {
                    id: "to-sqs".into(),
                    arn: "arn:…:orders-dlq".into(),
                    input: Some("{\"x\":1}".into()),
                },
            ],
        });
        v
    }

    #[test]
    fn activate_requests_event_buses() {
        let mut v = EventsView::new();
        assert!(matches!(
            v.on_activate().as_slice(),
            [Action::LoadEventBuses]
        ));
    }

    #[test]
    fn ingests_buses_via_message() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default"), bus("app-bus")]));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn filter_narrows_bus_list() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![
            bus("default"),
            bus("app-bus"),
            bus("ingest-bus"),
        ]));
        v.set_filter("INGEST"); // case-insensitive
        assert_eq!(v.visible_len(), 1);
    }

    #[test]
    fn partial_flag_shows_in_titles() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        assert!(!v.buses_title().contains("parcial"));
        v.on_message(&Message::EventBusesLoaded {
            buses: vec![bus("default")],
            more: true,
        });
        assert!(v.buses_title().contains("parcial"));
    }

    #[test]
    fn enter_drills_into_bus_rules() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        let actions = v.on_key(key(KeyCode::Enter));
        match actions.as_slice() {
            [Action::ClearFilter, Action::LoadRules { event_bus_name }] => {
                assert_eq!(event_bus_name, "default")
            }
            other => panic!("se esperaba ClearFilter+LoadRules, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Rules { .. }));
    }

    #[test]
    fn enter_drills_into_rule_detail_and_back() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // → Rules
        v.on_message(&Message::RulesLoaded {
            event_bus_name: "default".into(),
            rules: vec![rule("orders-created", RuleState::Enabled)],
            more: false,
        });
        let actions = v.on_key(key(KeyCode::Enter)); // → Detail
        match actions.as_slice() {
            [
                Action::ClearFilter,
                Action::LoadRuleDetail {
                    event_bus_name,
                    rule_name,
                },
            ] => {
                assert_eq!(event_bus_name, "default");
                assert_eq!(rule_name, "orders-created");
            }
            other => panic!("se esperaba ClearFilter+LoadRuleDetail, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Detail { .. }));

        // esc: Detail → Rules (limpia el filtro al subir).
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::ClearFilter]));
        assert!(matches!(v.level, Level::Rules { .. }));
    }

    #[test]
    fn esc_at_root_emits_back() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
        assert!(matches!(v.level, Level::Buses));
    }

    #[test]
    fn esc_in_rules_pops_with_clearfilter() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // → Rules
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::ClearFilter]));
        assert!(matches!(v.level, Level::Buses));
    }

    #[test]
    fn rules_from_wrong_bus_are_ignored() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // drill default → Rules
        v.on_message(&Message::RulesLoaded {
            event_bus_name: "OTRO".into(),
            rules: vec![rule("x", RuleState::Enabled)],
            more: false,
        });
        assert_eq!(v.visible_len(), 0, "rules de otro bus se descartan");
    }

    #[test]
    fn detail_from_wrong_rule_is_ignored() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // → Rules
        v.on_message(&Message::RulesLoaded {
            event_bus_name: "default".into(),
            rules: vec![rule("orders-created", RuleState::Enabled)],
            more: false,
        });
        v.on_key(key(KeyCode::Enter)); // → Detail de orders-created
        v.on_message(&Message::RuleDetailLoaded {
            event_bus_name: "default".into(),
            rule_name: "OTRA".into(),
            detail: RuleDetailDto {
                state: RuleState::Enabled,
                description: None,
                event_pattern: None,
                schedule_expression: None,
            },
            targets: vec![],
        });
        assert!(v.detail.is_none(), "detalle de otra rule se descarta");
    }

    #[test]
    fn y_copies_bus_arn() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert!(text.contains("event-bus/default")),
            other => panic!("se esperaba CopyToClipboard, llegó {other:?}"),
        }
    }

    #[test]
    fn o_opens_bus_in_console() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        match v.on_key(key(KeyCode::Char('O'))).as_slice() {
            [
                Action::OpenConsole {
                    target: ConsoleTarget::EventBus { name },
                },
            ] => assert_eq!(name, "default"),
            other => panic!("se esperaba OpenConsole EventBus, llegó {other:?}"),
        }
    }

    #[test]
    fn hints_offer_send_event_only_on_buses() {
        let mut v = EventsView::new();
        assert!(
            v.hints().iter().any(|(k, _)| *k == "S"),
            "en buses se anuncia enviar evento"
        );
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // → Rules
        assert!(
            !v.hints().iter().any(|(k, _)| *k == "S"),
            "en rules no se ofrece S (gated, solo en buses)"
        );
    }

    #[test]
    fn send_emits_intent_only_at_bus_level() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        // En Buses: S emite la intención.
        match v.on_key(key(KeyCode::Char('S'))).as_slice() {
            [Action::SendEvent { event_bus_name }] => assert_eq!(event_bus_name, "default"),
            other => panic!("se esperaba SendEvent, llegó {other:?}"),
        }
        // En Rules: S no hace nada (SendEvent es a nivel de bus).
        v.on_key(key(KeyCode::Enter)); // → Rules
        assert!(v.on_key(key(KeyCode::Char('S'))).is_empty());
    }

    #[test]
    fn filter_narrows_targets_in_detail() {
        let mut v = view_in_detail();
        assert_eq!(v.visible_len(), 2, "ambos targets sin filtro");
        v.set_filter("sqs"); // fuzzy sobre el id → solo to-sqs
        assert_eq!(v.visible_len(), 1);
        v.set_filter("");
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn render_buses_without_panicking() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default"), bus("app-bus")]));
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| v.render(f, f.area())).unwrap();
    }

    #[test]
    fn render_detail_without_panicking() {
        let mut v = view_in_detail();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| v.render(f, f.area())).unwrap();
    }
}
