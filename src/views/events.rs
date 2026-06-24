//! Vista `events`: EventBridge. Drill de 3 niveles: event buses → rules (estado
//! coloreado) → detalle (event_pattern + targets). Pura y síncrona; NUNCA importa
//! `aws-sdk-*` (recibe DTOs planos vía `on_message`).
//!
//! `S` en el nivel de buses abre un form multi-campo (`source`/`detail-type`/`detail`
//! JSON, vía `ui::form`); al enviar valida el JSON y emite `SendEvent` con el payload.
//! La vista NO sabe de modo escritura ni confirm: ese gate vive en el `App` (reusa el de
//! `PurgeQueue`/`RedriveExecution`). Con el form abierto declara `wants_raw_input` para
//! recibir las teclas crudas.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use super::View;
use crate::action::{Action, ConsoleTarget, ViewContext};
use crate::config::EventPreset;
use crate::message::{EventBusDto, Message, RuleDetailDto, RuleDto, RuleState, TargetDto};
use crate::ui::detail::DetailPanel;
use crate::ui::form::{Form, FormOutcome};
use crate::util::{fuzzy_score, ranked};

/// Separador de la `key` compuesta de un favorito profundo (`bus⟂rule`): el unit
/// separator ASCII (`\u{1f}`), que nunca aparece en nombres de bus/rule de AWS, así que
/// distingue un favorito de rule (con separador) de uno de bus (sin él). Opaco para el
/// core (viaja dentro de `ViewContext::Favorite { key }`).
const KEY_SEP: char = '\u{1f}';

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
    /// Panel de detalle abierto (el `event_pattern` con `P`, o el `input` de un target
    /// con `enter`): contenido completo scrolleable/copiable. `None` = vista normal.
    detail_panel: Option<DetailPanel>,
    /// Form de envío de evento abierto (`S` en buses): `(bus, form)`. El bus se captura
    /// al abrir (las teclas van al form, la selección no cambia). `None` = sin form.
    event_form: Option<(String, Form)>,
    filter: String,
    loading: bool,
    /// Se alcanzó el tope de paginación de buses (hay más sin traer).
    buses_partial: bool,
    /// Se alcanzó el tope de paginación de rules.
    rules_partial: bool,
    state: ListState,
    /// Presets de evento (de `config.toml`); el chooser de `S` los ofrece para prellenar
    /// el form. Vacío = `S` abre el form canned directo (comportamiento previo).
    presets: Vec<EventPreset>,
    /// Chooser de presets abierto (`S` con presets): elige uno antes del form. `None` =
    /// sin chooser. El índice 0 del chooser es "(en blanco)" = el default canned.
    preset_chooser: Option<ListState>,
}

impl EventsView {
    pub fn new() -> Self {
        Self {
            level: Level::Buses,
            buses: Vec::new(),
            rules: Vec::new(),
            detail: None,
            targets: Vec::new(),
            detail_panel: None,
            event_form: None,
            filter: String::new(),
            loading: false,
            buses_partial: false,
            rules_partial: false,
            state: ListState::default().with_selected(Some(0)),
            presets: Vec::new(),
            preset_chooser: None,
        }
    }

    /// Inyecta los presets de evento leídos de `config.toml` (lo llama el composition
    /// root). Sin presets, `S` abre el form canned directo.
    pub fn with_presets(mut self, presets: Vec<EventPreset>) -> Self {
        self.presets = presets;
        self
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

    fn selected_target(&self) -> Option<TargetDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_target_indices().get(sel)?;
        self.targets.get(idx).cloned()
    }

    /// Abre el panel con el `input` completo del target seleccionado (la fila lo colapsa
    /// a ~40 chars). `enter` sobre un target en el detalle.
    fn open_target_detail(&mut self) {
        if let Some(t) = self.selected_target() {
            let body = t
                .input
                .unwrap_or_else(|| "(este target no define input)".to_string());
            self.detail_panel = Some(DetailPanel::new(format!("target {}", t.id), body));
        }
    }

    /// Abre el panel con el `event_pattern` completo de la rule (el pane lo trunca/corta).
    /// `P` en el detalle; cae al `schedule_expression` si la rule es de agenda.
    fn open_pattern_detail(&mut self) {
        if let Some(d) = &self.detail {
            let body = d
                .event_pattern
                .clone()
                .or_else(|| {
                    d.schedule_expression
                        .clone()
                        .map(|s| format!("schedule: {s}"))
                })
                .unwrap_or_else(|| "(esta rule no tiene event_pattern)".to_string());
            self.detail_panel = Some(DetailPanel::new("event_pattern", body));
        }
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
                    let name = b.name.clone();
                    self.level = Level::Rules {
                        event_bus_name: name.clone(),
                    };
                    self.rules.clear();
                    self.rules_partial = false;
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![
                        Action::ClearFilter,
                        Action::RecordRecent {
                            key: name.clone(),
                            label: name.clone(),
                        },
                        Action::LoadRules {
                            event_bus_name: name,
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
        self.detail_panel = None; // cerrar cualquier panel al subir de nivel
        self.event_form = None; // y cerrar el form de envío si estaba abierto
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

    /// `S` en el nivel de buses: si hay presets configurados, abre el chooser para elegir
    /// uno; si no, abre el form canned directo (comportamiento previo).
    fn start_send(&mut self) {
        if !matches!(self.level, Level::Buses) || self.selected_bus().is_none() {
            return;
        }
        if self.presets.is_empty() {
            self.open_send_form_with(
                "awsdeck.manual",
                "awsdeck test event",
                r#"{"sentBy":"awsdeck"}"#,
            );
        } else {
            self.preset_chooser = Some(ListState::default().with_selected(Some(0)));
        }
    }

    /// El usuario eligió en el chooser (`enter`): índice 0 = "(en blanco)" canned; el resto
    /// = ese preset. Cierra el chooser y abre el form prellenado.
    fn choose_preset(&mut self) {
        let sel = self
            .preset_chooser
            .as_ref()
            .and_then(|s| s.selected())
            .unwrap_or(0);
        self.preset_chooser = None;
        if sel == 0 {
            self.open_send_form_with(
                "awsdeck.manual",
                "awsdeck test event",
                r#"{"sentBy":"awsdeck"}"#,
            );
        } else if let Some(p) = self.presets.get(sel - 1) {
            // Clonar para cortar el préstamo de `self.presets` antes de `&mut self`.
            let (source, detail_type, detail) =
                (p.source.clone(), p.detail_type.clone(), p.detail.clone());
            self.open_send_form_with(&source, &detail_type, &detail);
        }
    }

    /// Navega el chooser de presets (`j/k`); clamp a `[0, presets.len()]` (el 0 es canned).
    fn chooser_move(&mut self, delta: i32) {
        if let Some(state) = self.preset_chooser.as_mut() {
            let len = self.presets.len() + 1; // +1 por "(en blanco)"
            let cur = state.selected().unwrap_or(0) as i32;
            let next = (cur + delta).clamp(0, len as i32 - 1) as usize;
            state.select(Some(next));
        }
    }

    /// Abre el form de envío prellenado con `source`/`detail-type`/`detail` y stashea el
    /// bus elegido. `time`/`resources` arrancan vacíos (opcionales). El envío (con el
    /// payload tecleado) se gatea en el App al `Submit`.
    fn open_send_form_with(&mut self, source: &str, detail_type: &str, detail: &str) {
        if matches!(self.level, Level::Buses)
            && let Some(b) = self.selected_bus()
        {
            let form = Form::new(
                format!("enviar evento → {}", b.name),
                vec![
                    ("source", source),
                    ("detail-type", detail_type),
                    ("detail", detail),
                    // Opcionales (vacío = omitir): timestamp UTC y ARNs de recursos.
                    // (El form es de una línea por campo — `enter` envía —, así que los
                    // resources van separados por coma.)
                    ("time (UTC, vacío=ahora)", ""),
                    ("resources (ARNs, coma)", ""),
                ],
            );
            self.event_form = Some((b.name, form));
        }
    }

    /// Valida y envía el form (`enter`): si `detail` no es JSON, muestra el error y deja
    /// el form abierto; si es válido, lo cierra y emite `SendEvent` (que el App gatea).
    fn submit_form(&mut self) -> Vec<Action> {
        let Some((_, form)) = self.event_form.as_ref() else {
            return vec![];
        };
        let vals = form.values();
        let source = vals.first().cloned().unwrap_or_default();
        let detail_type = vals.get(1).cloned().unwrap_or_default();
        let detail = vals.get(2).cloned().unwrap_or_default();
        let time_str = vals.get(3).cloned().unwrap_or_default();
        let resources_str = vals.get(4).cloned().unwrap_or_default();
        if serde_json::from_str::<serde_json::Value>(detail.trim()).is_err() {
            if let Some((_, f)) = self.event_form.as_mut() {
                f.set_error("detail no es JSON válido (revisá comillas/llaves)");
            }
            return vec![];
        }
        // `time` opcional: vacío = ahora; si no, parsea UTC (mismo parser que `:since`).
        let time = if time_str.trim().is_empty() {
            None
        } else {
            match crate::util::parse_datetime(time_str.trim()) {
                Some(ms) => Some(ms),
                None => {
                    if let Some((_, f)) = self.event_form.as_mut() {
                        f.set_error("time inválido: usa YYYY-MM-DD[THH:MM] (UTC)");
                    }
                    return vec![];
                }
            }
        };
        // `resources` opcional: ARNs separados por coma (se ignoran los vacíos).
        let resources: Vec<String> = resources_str
            .split(',')
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        let event_bus_name = self.event_form.take().map(|(b, _)| b).unwrap_or_default();
        vec![Action::SendEvent {
            event_bus_name,
            source,
            detail_type,
            detail,
            time,
            resources,
        }]
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

    /// Renderiza el chooser de presets como popup centrado (sobre la lista de buses). El
    /// índice 0 es "(en blanco)" = el default canned; el resto, los presets de config.
    fn render_preset_chooser(&mut self, frame: &mut Frame, area: Rect) {
        let Some(state) = self.preset_chooser.as_mut() else {
            return;
        };
        let mut items: Vec<ListItem> = vec![ListItem::new(Line::from(Span::styled(
            "(en blanco)",
            Style::new().dark_gray(),
        )))];
        items.extend(
            self.presets
                .iter()
                .map(|p| ListItem::new(Line::from(p.name.clone()))),
        );
        let height = (items.len() as u16 + 2).min(area.height.max(3));
        let popup = crate::ui::popup_area(area, 50, height);
        frame.render_widget(Clear, popup);
        let list = List::new(items)
            .block(Block::bordered().title(" preset · enter usa · esc cancela "))
            .highlight_style(Style::new().reversed())
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, popup, state);
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

    fn wants_raw_input(&self) -> bool {
        self.event_form.is_some() || self.preset_chooser.is_some()
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        // Chooser de presets abierto: navegar / elegir / cancelar.
        if self.preset_chooser.is_some() {
            return vec![("j/k", "preset"), ("enter", "usar"), ("esc", "cancelar")];
        }
        // Form de envío abierto: sus teclas (campo/enviar/cancelar) mandan.
        if let Some((_, form)) = &self.event_form {
            return form.hints();
        }
        // Con el panel abierto, sus teclas (scroll/copiar/cerrar) mandan.
        if let Some(p) = &self.detail_panel {
            return p.hints();
        }
        match self.level {
            // `S` envía un evento de prueba al bus (gated por modo escritura + confirm).
            Level::Buses => vec![
                ("y", "copiar ARN"),
                ("O", "consola"),
                ("S", "enviar evento"),
            ],
            Level::Rules { .. } => vec![("y", "copiar nombre"), ("O", "consola")],
            Level::Detail { .. } => vec![
                ("enter", "ver input"),
                ("P", "ver patrón"),
                ("y", "copiar ARN target"),
                ("O", "consola"),
            ],
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
        self.detail_panel = None;
        self.event_form = None;
        self.targets.clear();
        self.loading = true;
        self.buses_partial = false;
        self.rules_partial = false;
        self.state.select(Some(0));
        vec![Action::LoadEventBuses]
    }

    /// Abrir un favorito/reciente. `key` simple (nombre de bus) → drillea a sus rules;
    /// `key` compuesta (`bus⟂rule`, favorito profundo) → abre el detalle de la rule.
    /// Otros contextos (LogGroupTail) no le conciernen → activación normal.
    fn on_context(&mut self, context: &ViewContext) -> Vec<Action> {
        match context {
            // Favorito profundo: una rule concreta → directo a su detalle.
            ViewContext::Favorite { key } if key.contains(KEY_SEP) => {
                let (bus, rule) = key.split_once(KEY_SEP).unwrap();
                let (event_bus_name, rule_name) = (bus.to_string(), rule.to_string());
                self.level = Level::Detail {
                    event_bus_name: event_bus_name.clone(),
                    rule_name: rule_name.clone(),
                };
                self.detail = None;
                self.targets.clear();
                self.detail_panel = None;
                self.event_form = None;
                self.loading = true;
                self.state.select(Some(0));
                vec![Action::LoadRuleDetail {
                    event_bus_name,
                    rule_name,
                }]
            }
            ViewContext::Favorite { key } => {
                self.level = Level::Rules {
                    event_bus_name: key.clone(),
                };
                self.rules.clear();
                self.rules_partial = false;
                self.detail_panel = None;
                self.event_form = None;
                self.loading = true;
                self.state.select(Some(0));
                vec![Action::LoadRules {
                    event_bus_name: key.clone(),
                }]
            }
            _ => self.on_activate(),
        }
    }

    /// Favorito según el nivel: el bus seleccionado (raíz), o —favorito profundo— una rule
    /// concreta con `key` compuesta `bus⟂rule` (en el nivel de rules o en su detalle).
    fn selected_favorite(&self) -> Option<(String, String)> {
        match &self.level {
            Level::Buses => self.selected_bus().map(|b| (b.name.clone(), b.name)),
            Level::Rules { event_bus_name } => self.selected_rule().map(|r| {
                (
                    format!("{event_bus_name}{KEY_SEP}{}", r.name),
                    format!("{event_bus_name} · {}", r.name),
                )
            }),
            Level::Detail {
                event_bus_name,
                rule_name,
            } => Some((
                format!("{event_bus_name}{KEY_SEP}{rule_name}"),
                format!("{event_bus_name} · {rule_name}"),
            )),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // Chooser de presets abierto: `j/k` navega, `enter` elige (abre el form
        // prellenado), `esc` cancela. (El App nos manda las teclas crudas vía
        // `wants_raw_input`, así que `*`/`/`/`q` no las intercepta el core.)
        if self.preset_chooser.is_some() {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => self.chooser_move(1),
                KeyCode::Char('k') | KeyCode::Up => self.chooser_move(-1),
                KeyCode::Enter => self.choose_preset(),
                KeyCode::Esc => self.preset_chooser = None,
                _ => {}
            }
            return vec![];
        }
        // Form de envío abierto: rutea TODAS las teclas al form (el App nos las manda
        // crudas vía `wants_raw_input`). `enter` valida+envía, `esc` cancela.
        if self.event_form.is_some() {
            let outcome = self.event_form.as_mut().unwrap().1.on_key(key);
            return match outcome {
                FormOutcome::Cancel => {
                    self.event_form = None;
                    vec![]
                }
                FormOutcome::Submit => self.submit_form(),
                FormOutcome::Editing => vec![],
            };
        }
        // Panel de detalle abierto: `y` copia su contenido, el resto scrollea/cierra.
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
            // En el detalle, `enter` expande el input del target (la lista navegable son
            // los targets); en los demás niveles, drillea.
            KeyCode::Enter if matches!(self.level, Level::Detail { .. }) => {
                self.open_target_detail();
                vec![]
            }
            KeyCode::Enter => self.drill(),
            // `P` expande el event_pattern completo (scroll + copia).
            KeyCode::Char('P') if matches!(self.level, Level::Detail { .. }) => {
                self.open_pattern_detail();
                vec![]
            }
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('S') => {
                self.start_send();
                vec![]
            }
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
            Message::Error { .. } => self.loading = false,
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
        // Panel de detalle: ocupa el cuerpo entero con el contenido completo.
        if let Some(p) = self.detail_panel.as_mut() {
            p.render(frame, area);
            return;
        }
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

                let pat_block = Block::bordered().title(" patrón · P expande ");
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
        // Chooser de presets: popup centrado (antes que el form; son excluyentes).
        self.render_preset_chooser(frame, area);
        // Form de envío: popup centrado sobre el cuerpo (la lista de buses queda detrás).
        if let Some((_, form)) = &self.event_form {
            form.render(frame, area);
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

    fn preset(name: &str) -> EventPreset {
        EventPreset {
            name: name.to_string(),
            source: format!("src.{name}"),
            detail_type: format!("Type{name}"),
            detail: r#"{"k":1}"#.to_string(),
        }
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
    fn enter_on_target_opens_input_panel() {
        let mut v = view_in_detail(); // sel 0 = to-lambda (input None)
        let actions = v.on_key(key(KeyCode::Enter));
        assert!(
            actions.is_empty(),
            "enter en el detalle abre panel, no drillea"
        );
        let p = v.detail_panel.as_ref().expect("panel abierto");
        assert!(p.content().contains("no define input"), "target sin input");

        // Cierra y baja al target con input.
        v.on_key(key(KeyCode::Esc));
        v.on_key(key(KeyCode::Char('j'))); // → to-sqs (input Some)
        v.on_key(key(KeyCode::Enter));
        assert_eq!(
            v.detail_panel.as_ref().expect("panel").content(),
            r#"{"x":1}"#
        );
    }

    #[test]
    fn capital_p_opens_pattern_panel_copies_and_esc_closes() {
        let mut v = view_in_detail();
        v.on_key(key(KeyCode::Char('P')));
        assert!(
            v.detail_panel
                .as_ref()
                .is_some_and(|p| p.content().contains("my.app")),
            "P expande el event_pattern completo"
        );
        // `y` copia el patrón mostrado.
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert!(text.contains("my.app")),
            other => panic!("se esperaba copiar el patrón, llegó {other:?}"),
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
    fn detail_hints_offer_input_and_pattern() {
        let v = view_in_detail();
        let hints = v.hints();
        assert!(hints.iter().any(|(k, _)| *k == "enter"), "enter: ver input");
        assert!(hints.iter().any(|(k, _)| *k == "P"), "P: ver patrón");
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
            [
                Action::ClearFilter,
                Action::RecordRecent { key, .. },
                Action::LoadRules { event_bus_name },
            ] => {
                assert_eq!(event_bus_name, "default");
                assert_eq!(key, "default", "recuerda el bus abierto");
            }
            other => panic!("se esperaba ClearFilter+RecordRecent+LoadRules, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Rules { .. }));
    }

    #[test]
    fn favorite_getter_and_open_via_context() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        // En el nivel de buses el favorito es el bus seleccionado.
        assert_eq!(
            v.selected_favorite(),
            Some(("default".to_string(), "default".to_string()))
        );
        // Abrir un favorito via contexto → drillea directo a sus rules.
        match v
            .on_context(&ViewContext::Favorite {
                key: "default".to_string(),
            })
            .as_slice()
        {
            [Action::LoadRules { event_bus_name }] => assert_eq!(event_bus_name, "default"),
            other => panic!("se esperaba LoadRules, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Rules { .. }));
    }

    #[test]
    fn deep_favorite_targets_a_rule_and_opens_its_detail() {
        let sep = '\u{1f}';
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Enter)); // → Rules
        v.on_message(&Message::RulesLoaded {
            event_bus_name: "default".into(),
            rules: vec![rule("orders-created", RuleState::Enabled)],
            more: false,
        });
        // En el nivel de rules el favorito es la rule seleccionada (key compuesta bus⟂rule).
        assert_eq!(
            v.selected_favorite(),
            Some((
                format!("default{sep}orders-created"),
                "default · orders-created".to_string()
            ))
        );
        // Abrir ese favorito profundo → directo al detalle de la rule.
        let mut v2 = EventsView::new();
        match v2
            .on_context(&ViewContext::Favorite {
                key: format!("default{sep}orders-created"),
            })
            .as_slice()
        {
            [
                Action::LoadRuleDetail {
                    event_bus_name,
                    rule_name,
                },
            ] => {
                assert_eq!(event_bus_name, "default");
                assert_eq!(rule_name, "orders-created");
            }
            other => panic!("se esperaba LoadRuleDetail, llegó {other:?}"),
        }
        assert!(matches!(v2.level, Level::Detail { .. }));
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
    fn s_opens_send_form_only_at_bus_level() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        // En Buses: S abre el form (no emite acción todavía) y captura entrada cruda.
        assert!(v.on_key(key(KeyCode::Char('S'))).is_empty());
        assert!(v.event_form.is_some(), "S abre el form");
        assert!(
            v.wants_raw_input(),
            "con el form abierto captura teclas crudas"
        );
        // esc cierra el form sin emitir.
        v.on_key(key(KeyCode::Esc));
        assert!(v.event_form.is_none());
        assert!(!v.wants_raw_input());

        // En Rules: S no abre form (es a nivel de bus).
        v.on_key(key(KeyCode::Enter)); // → Rules
        assert!(v.on_key(key(KeyCode::Char('S'))).is_empty());
        assert!(v.event_form.is_none());
    }

    #[test]
    fn submit_emits_send_event_with_payload_and_validates_json() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S'))); // abre el form (defaults JSON válidos)

        // JSON por defecto válido → enter emite SendEvent con el payload tecleado.
        // time/resources vacíos por defecto → None / [].
        match v.on_key(key(KeyCode::Enter)).as_slice() {
            [
                Action::SendEvent {
                    event_bus_name,
                    source,
                    detail_type,
                    detail,
                    time,
                    resources,
                },
            ] => {
                assert_eq!(event_bus_name, "default");
                assert_eq!(source, "awsdeck.manual");
                assert_eq!(detail_type, "awsdeck test event");
                assert!(detail.contains("sentBy"));
                assert_eq!(*time, None, "time vacío → None (ahora)");
                assert!(resources.is_empty(), "resources vacío → []");
            }
            other => panic!("se esperaba SendEvent con payload, llegó {other:?}"),
        }
        assert!(v.event_form.is_none(), "tras enviar, el form se cierra");
    }

    #[test]
    fn submit_parses_time_and_resources_when_present() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S'))); // abre el form

        // Tab al campo time (4º) y teclear una fecha UTC válida.
        v.on_key(key(KeyCode::Tab)); // source → detail-type
        v.on_key(key(KeyCode::Tab)); // detail-type → detail
        v.on_key(key(KeyCode::Tab)); // detail → time
        for c in "2026-06-23T15:30".chars() {
            v.on_key(key(KeyCode::Char(c)));
        }
        // Tab a resources (5º) y teclear dos ARNs separados por coma.
        v.on_key(key(KeyCode::Tab)); // time → resources
        for c in "arn:aws:s3:::a, arn:aws:s3:::b".chars() {
            v.on_key(key(KeyCode::Char(c)));
        }
        // `enter` envía el form (en el form Enter = Submit).
        match v.on_key(key(KeyCode::Enter)).as_slice() {
            [
                Action::SendEvent {
                    time, resources, ..
                },
            ] => {
                assert_eq!(*time, crate::util::parse_datetime("2026-06-23T15:30"));
                assert_eq!(resources, &vec!["arn:aws:s3:::a", "arn:aws:s3:::b"]);
            }
            other => panic!("se esperaba SendEvent con time/resources, llegó {other:?}"),
        }
    }

    #[test]
    fn submit_with_invalid_time_keeps_form_open_without_emitting() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S')));
        // Ir al campo time (4º) y teclear basura.
        v.on_key(key(KeyCode::Tab));
        v.on_key(key(KeyCode::Tab));
        v.on_key(key(KeyCode::Tab));
        for c in "no-es-fecha".chars() {
            v.on_key(key(KeyCode::Char(c)));
        }
        assert!(v.submit_form().is_empty(), "time inválido no emite");
        assert!(v.event_form.is_some(), "el form sigue abierto");
    }

    #[test]
    fn submit_with_invalid_json_keeps_form_open_without_emitting() {
        let mut v = EventsView::new();
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S')));
        // Ir al campo `detail` (3º) y romper el JSON tecleando una llave suelta.
        v.on_key(key(KeyCode::Tab)); // source → detail-type
        v.on_key(key(KeyCode::Tab)); // detail-type → detail
        v.on_key(key(KeyCode::Char('{'))); // detail ahora inválido
        // enter NO emite: el JSON es inválido → el form sigue abierto.
        assert!(v.on_key(key(KeyCode::Enter)).is_empty());
        assert!(v.event_form.is_some(), "JSON inválido deja el form abierto");
    }

    #[test]
    fn s_with_presets_opens_chooser_not_form() {
        let mut v = EventsView::new().with_presets(vec![preset("a"), preset("b")]);
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S')));
        assert!(v.preset_chooser.is_some(), "abre el chooser");
        assert!(v.event_form.is_none(), "todavía no hay form");
        assert!(v.wants_raw_input(), "el chooser recibe teclas crudas");
    }

    #[test]
    fn s_without_presets_opens_canned_form_directly() {
        let mut v = EventsView::new(); // sin presets
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S')));
        assert!(v.preset_chooser.is_none(), "sin chooser");
        let (_, form) = v.event_form.as_ref().expect("form canned directo");
        assert_eq!(form.values()[0], "awsdeck.manual");
    }

    #[test]
    fn chooser_enter_on_preset_opens_prefilled_form() {
        let mut v = EventsView::new().with_presets(vec![preset("a"), preset("b")]);
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S'))); // chooser: [0]=(en blanco), [1]=a, [2]=b
        v.on_key(key(KeyCode::Char('j'))); // → preset "a" (índice 1)
        v.on_key(key(KeyCode::Enter));
        assert!(v.preset_chooser.is_none(), "el chooser se cerró");
        let (_, form) = v.event_form.as_ref().expect("form abierto");
        let vals = form.values();
        assert_eq!(vals[0], "src.a");
        assert_eq!(vals[1], "Typea");
        assert_eq!(vals[2], r#"{"k":1}"#);
    }

    #[test]
    fn chooser_index_zero_opens_canned_form() {
        let mut v = EventsView::new().with_presets(vec![preset("a")]);
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S'))); // chooser arranca en índice 0 = (en blanco)
        v.on_key(key(KeyCode::Enter));
        let (_, form) = v.event_form.as_ref().expect("form canned");
        assert_eq!(form.values()[0], "awsdeck.manual");
    }

    #[test]
    fn chooser_esc_closes_without_form() {
        let mut v = EventsView::new().with_presets(vec![preset("a")]);
        v.on_message(&buses_msg(vec![bus("default")]));
        v.on_key(key(KeyCode::Char('S')));
        v.on_key(key(KeyCode::Esc));
        assert!(v.preset_chooser.is_none() && v.event_form.is_none());
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
