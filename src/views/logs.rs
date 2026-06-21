//! Vista `logs`: CloudWatch Log Groups -> drill a Log Streams. Pura y síncrona:
//! mantiene estado (groups/streams, nivel de drill, selección, filtro), traduce
//! teclas a `Action`s y dibuja. NUNCA importa `aws-sdk-*`; recibe datos vía
//! `on_message` con DTOs planos, así que se testea inyectando `Message`s.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::View;
use crate::action::Action;
use crate::message::{LogEventDto, LogGroupDto, LogStreamDto, Message};
use crate::util::{fmt_clock_millis, fmt_epoch_millis, fuzzy_score, ranked};

/// Nivel de drill actual.
enum Level {
    Groups,
    Streams {
        group: String,
    },
    /// Eventos de un stream (`get_log_events`).
    Events {
        group: String,
        stream: String,
    },
    /// Tail del group (`filter_log_events` sobre todos sus streams). Sibling de
    /// `Streams`: ambos bajan un nivel desde un group y `esc` vuelve a `Groups`.
    Tail {
        group: String,
    },
}

pub struct LogsView {
    level: Level,
    groups: Vec<LogGroupDto>,
    streams: Vec<LogStreamDto>,
    /// Buffer de líneas para las hojas `Events`/`Tail` (solo una activa a la vez).
    events: Vec<LogEventDto>,
    filter: String,
    loading: bool,
    /// Última query server-side enviada (None = primeros 50). Guard "latest wins":
    /// se descartan respuestas cuya query no coincide.
    last_query: Option<String>,
    /// `true` si el servidor tiene más groups que los traídos (next_token).
    partial: bool,
    /// Selección de la lista visible (índice dentro de la lista filtrada).
    state: ListState,
}

impl LogsView {
    pub fn new() -> Self {
        Self {
            level: Level::Groups,
            groups: Vec::new(),
            streams: Vec::new(),
            events: Vec::new(),
            filter: String::new(),
            loading: false,
            last_query: None,
            partial: false,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    // --- Filtrado / selección -------------------------------------------------

    fn filtered_group_indices(&self) -> Vec<usize> {
        ranked(self.groups.len(), &self.filter, |i| {
            fuzzy_score(&self.groups[i].name, &self.filter)
        })
    }

    fn filtered_stream_indices(&self) -> Vec<usize> {
        ranked(self.streams.len(), &self.filter, |i| {
            fuzzy_score(&self.streams[i].name, &self.filter)
        })
    }

    /// Filtro de líneas de log: substring case-insensitive sobre el mensaje (no
    /// fuzzy — más apropiado para texto largo), preservando el orden cronológico.
    fn filtered_event_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        (0..self.events.len())
            .filter(|&i| {
                needle.is_empty() || self.events[i].message.to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn visible_len(&self) -> usize {
        match self.level {
            Level::Groups => self.filtered_group_indices().len(),
            Level::Streams { .. } => self.filtered_stream_indices().len(),
            Level::Events { .. } | Level::Tail { .. } => self.filtered_event_indices().len(),
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

    /// Mantiene la selección dentro de rango tras un cambio de datos o de filtro.
    fn clamp_selection(&mut self) {
        let len = self.visible_len();
        match self.state.selected() {
            _ if len == 0 => self.state.select(None),
            Some(i) if i >= len => self.state.select(Some(len - 1)),
            None => self.state.select(Some(0)),
            Some(_) => {}
        }
    }

    /// Tras reemplazar `groups` con datos async (refresh o respuesta de la búsqueda
    /// server-side), re-selecciona el group con este nombre si sigue en la lista
    /// filtrada; si no está (o no había selección), cae al tope (mejor match). Así
    /// una recarga no pisa la posición que el usuario movió con las flechas:
    /// `set_filter` ya dejó la selección en el mejor match al teclear, de modo que
    /// esto conserva ese baseline cuando el usuario no navegó.
    fn restore_selection(&mut self, name: Option<&str>) {
        let pos = name.and_then(|n| {
            self.filtered_group_indices()
                .iter()
                .position(|&i| self.groups[i].name == n)
        });
        self.state.select(Some(pos.unwrap_or(0)));
        self.clamp_selection();
    }

    fn selected_group_name(&self) -> Option<String> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_group_indices().get(sel)?;
        Some(self.groups[idx].name.clone())
    }

    fn selected_stream_name(&self) -> Option<String> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_stream_indices().get(sel)?;
        Some(self.streams[idx].name.clone())
    }

    // --- Navegación -----------------------------------------------------------

    fn drill(&mut self) -> Vec<Action> {
        match &self.level {
            Level::Groups => match self.selected_group_name() {
                Some(group) => {
                    self.level = Level::Streams {
                        group: group.clone(),
                    };
                    self.streams.clear();
                    self.loading = true;
                    self.state.select(Some(0));
                    // ClearFilter evita que el filtro de groups (server-side) se
                    // arrastre a los streams (client-side, otro dominio).
                    vec![Action::ClearFilter, Action::LoadLogStreams { group }]
                }
                None => vec![],
            },
            Level::Streams { group } => {
                let group = group.clone();
                match self.selected_stream_name() {
                    Some(stream) => {
                        self.level = Level::Events {
                            group: group.clone(),
                            stream: stream.clone(),
                        };
                        self.events.clear();
                        self.loading = true;
                        self.state.select(Some(0));
                        vec![Action::ClearFilter, Action::LoadLogEvents { group, stream }]
                    }
                    None => vec![],
                }
            }
            // Hojas (líneas de log): enter no hace nada (v0 es solo-lectura).
            Level::Events { .. } | Level::Tail { .. } => vec![],
        }
    }

    /// `t` desde la raíz: tail del group seleccionado (`filter_log_events` sobre
    /// todos sus streams). Sibling del drill a streams; `esc` vuelve a groups.
    fn tail(&mut self) -> Vec<Action> {
        match self.selected_group_name() {
            Some(group) => {
                self.level = Level::Tail {
                    group: group.clone(),
                };
                self.events.clear();
                self.loading = true;
                self.last_query = None; // el tail arranca sin filtro server-side
                self.state.select(Some(0));
                vec![
                    Action::ClearFilter,
                    Action::LoadLogTail {
                        group,
                        pattern: None,
                    },
                ]
            }
            None => vec![],
        }
    }

    /// `esc`: despoja un nivel de drill. En la raíz (groups) no hay nada que
    /// despojar → emite `Back` para que el `App` vuelva al menú. (El `App` ya limpió
    /// el filtro en la 1a etapa de `esc`, así que aquí no hace falta `ClearFilter`.)
    fn back(&mut self) -> Vec<Action> {
        match &self.level {
            // Events → Streams: los streams siguen en cache, no se recargan.
            Level::Events { group, .. } => {
                self.level = Level::Streams {
                    group: group.clone(),
                };
                self.state.select(Some(0));
                self.clamp_selection();
                self.loading = false;
                vec![]
            }
            // Streams/Tail → Groups. Si veníamos de una búsqueda server-side, los
            // groups en cache están acotados a esa query → recargamos la página
            // completa. Sin búsqueda previa, siguen en cache.
            Level::Streams { .. } | Level::Tail { .. } => {
                self.level = Level::Groups;
                self.state.select(Some(0));
                self.clamp_selection();
                if self.last_query.take().is_some() {
                    self.loading = true;
                    vec![Action::LoadLogGroups { query: None }]
                } else {
                    self.loading = false;
                    vec![]
                }
            }
            Level::Groups => vec![Action::Back],
        }
    }

    fn refresh(&mut self) -> Vec<Action> {
        self.loading = true;
        match &self.level {
            // Recargar la página actual (misma query si hay una búsqueda activa).
            Level::Groups => vec![Action::LoadLogGroups {
                query: self.last_query.clone(),
            }],
            Level::Streams { group } => vec![Action::LoadLogStreams {
                group: group.clone(),
            }],
            Level::Events { group, stream } => vec![Action::LoadLogEvents {
                group: group.clone(),
                stream: stream.clone(),
            }],
            // El tail recarga con el mismo filtro server-side vigente (si lo hay).
            Level::Tail { group } => vec![Action::LoadLogTail {
                group: group.clone(),
                pattern: self.last_query.clone(),
            }],
        }
    }

    // --- Render ---------------------------------------------------------------

    fn body_title(&self) -> String {
        let (kind, total, shown) = match self.level {
            Level::Groups => (
                "log groups",
                self.groups.len(),
                self.filtered_group_indices().len(),
            ),
            Level::Streams { .. } => (
                "streams",
                self.streams.len(),
                self.filtered_stream_indices().len(),
            ),
            Level::Events { .. } => (
                "eventos",
                self.events.len(),
                self.filtered_event_indices().len(),
            ),
            Level::Tail { .. } => (
                "tail",
                self.events.len(),
                self.filtered_event_indices().len(),
            ),
        };
        let partial = if !self.partial {
            ""
        } else {
            match self.level {
                Level::Groups => " · parcial (/ busca server-side)",
                Level::Events { .. } => " · parcial (más viejas arriba)",
                Level::Tail { .. } => " · parcial (acota con /)",
                Level::Streams { .. } => "",
            }
        };
        if self.filter.is_empty() {
            format!(" {total} {kind}{partial} ")
        } else {
            format!(
                " {shown}/{total} {kind} · filtro: {}{partial} ",
                self.filter
            )
        }
    }
}

impl Default for LogsView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for LogsView {
    fn id(&self) -> &'static str {
        "logs"
    }

    fn description(&self) -> &'static str {
        "CloudWatch Logs — groups, streams, eventos y tail"
    }

    fn title(&self) -> String {
        match &self.level {
            Level::Groups => "logs".to_string(),
            Level::Streams { group } => format!("logs / {group}"),
            Level::Events { group, stream } => format!("logs / {group} / {stream}"),
            Level::Tail { group } => format!("logs / {group} (tail)"),
        }
    }

    fn on_activate(&mut self) -> Vec<Action> {
        self.level = Level::Groups;
        self.streams.clear();
        self.events.clear();
        self.loading = true;
        self.last_query = None;
        self.partial = false;
        self.state.select(Some(0));
        vec![Action::LoadLogGroups { query: None }]
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
            // Tail del group seleccionado (solo desde la raíz; sibling de streams).
            KeyCode::Char('t') if matches!(self.level, Level::Groups) => self.tail(),
            _ => vec![],
        }
    }

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::LogGroupsLoaded {
                groups,
                query,
                more,
            } => {
                // Guard "latest wins": descartar respuestas de búsquedas viejas.
                if query != &self.last_query {
                    return;
                }
                // Capturar la selección ANTES de reemplazar, para preservarla por
                // nombre y no pisar la navegación del usuario con la recarga async.
                let keep = self.selected_group_name();
                self.groups = groups.clone();
                self.partial = *more;
                if matches!(self.level, Level::Groups) {
                    self.loading = false;
                    self.restore_selection(keep.as_deref());
                }
            }
            Message::LogStreamsLoaded { group, streams } => {
                // Aceptar solo si corresponden al group del drill actual.
                if let Level::Streams { group: current } = &self.level
                    && current == group
                {
                    self.streams = streams.clone();
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::LogEventsLoaded {
                group,
                stream,
                events,
                more,
            } => {
                // Aceptar solo si corresponden al stream del drill actual.
                if let Level::Events {
                    group: g,
                    stream: s,
                } = &self.level
                    && g == group
                    && s == stream
                {
                    self.events = events.clone();
                    self.partial = *more;
                    self.loading = false;
                    self.select_edge(true); // newest abajo (convención de terminal)
                }
            }
            Message::LogTailLoaded {
                group,
                query,
                events,
                more,
            } => {
                // Guard "latest wins" (filtro server-side) + correspondencia de group.
                if query != &self.last_query {
                    return;
                }
                if let Level::Tail { group: g } = &self.level
                    && g == group
                {
                    self.events = events.clone();
                    self.partial = *more;
                    self.loading = false;
                    self.select_edge(true);
                }
            }
            // El App ya muestra el error en la status bar; aquí cortamos el loading.
            Message::Error(_) => self.loading = false,
            // Mensajes de otras vistas (p. ej. SQS): se ignoran.
            _ => {}
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.state.select(Some(0)); // top = mejor match (estilo fzf)
        self.clamp_selection();
    }

    fn search(&mut self, query: &str) -> Vec<Action> {
        // Búsqueda server-side: en groups (logGroupNamePattern) y en el tail del
        // group (filter_pattern). En los demás niveles, `/` filtra local.
        match &self.level {
            Level::Groups => {
                self.last_query = (!query.is_empty()).then(|| query.to_string());
                self.loading = true;
                vec![Action::LoadLogGroups {
                    query: self.last_query.clone(),
                }]
            }
            Level::Tail { group } => {
                let group = group.clone();
                self.last_query = (!query.is_empty()).then(|| query.to_string());
                self.loading = true;
                vec![Action::LoadLogTail {
                    group,
                    pattern: self.last_query.clone(),
                }]
            }
            _ => Vec::new(),
        }
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered().title(self.body_title());

        let items: Vec<ListItem> = match self.level {
            Level::Groups => self
                .filtered_group_indices()
                .into_iter()
                .map(|i| group_item(&self.groups[i]))
                .collect(),
            Level::Streams { .. } => self
                .filtered_stream_indices()
                .into_iter()
                .map(|i| stream_item(&self.streams[i]))
                .collect(),
            Level::Events { .. } | Level::Tail { .. } => self
                .filtered_event_indices()
                .into_iter()
                .map(|i| event_item(&self.events[i]))
                .collect(),
        };

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

// --- Construcción de filas y formato ------------------------------------------

fn group_item(g: &LogGroupDto) -> ListItem<'static> {
    let size = g
        .stored_bytes
        .map(human_bytes)
        .unwrap_or_else(|| "—".to_string());
    ListItem::new(Line::from(vec![
        Span::raw(g.name.clone()),
        Span::raw("  "),
        Span::styled(format!("[{size}]"), Style::new().dark_gray()),
    ]))
}

fn stream_item(s: &LogStreamDto) -> ListItem<'static> {
    let when = s
        .last_event_ts
        .map(fmt_epoch_millis)
        .unwrap_or_else(|| "—".to_string());
    ListItem::new(Line::from(vec![
        Span::raw(s.name.clone()),
        Span::raw("  "),
        Span::styled(when, Style::new().dark_gray()),
    ]))
}

/// Una línea de log: `HH:MM:SS  [stream]  message`. El `[stream]` solo aparece en el
/// tail (varios streams mezclados). Color por severidad del mensaje.
fn event_item(e: &LogEventDto) -> ListItem<'static> {
    let when =
        e.ts.map(fmt_clock_millis)
            .unwrap_or_else(|| "--:--:--".to_string());
    let mut spans = vec![
        Span::styled(when, Style::new().dark_gray()),
        Span::raw("  "),
    ];
    if let Some(stream) = &e.stream {
        spans.push(Span::styled(
            format!("{}  ", stream_suffix(stream)),
            Style::new().cyan(),
        ));
    }
    spans.push(Span::styled(e.message.clone(), severity_style(&e.message)));
    ListItem::new(Line::from(spans))
}

/// Sufijo corto e identificable de un nombre de stream (la parte tras `]`, p. ej.
/// `2026/06/21/[$LATEST]ab12cd` → `ab12cd`), recortado para no robar ancho a la línea.
fn stream_suffix(name: &str) -> String {
    let tail = name
        .rsplit(']')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    tail.chars().take(12).collect()
}

/// Resalta líneas de error (rojo) y warning (amarillo); el resto en estilo normal.
fn severity_style(msg: &str) -> Style {
    if msg.contains("ERROR") || msg.contains("Exception") || msg.contains("panic") {
        Style::new().red()
    } else if msg.contains("WARN") {
        Style::new().yellow()
    } else {
        Style::new()
    }
}

fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str) -> LogGroupDto {
        LogGroupDto {
            name: name.to_string(),
            stored_bytes: Some(1024),
            arn: None,
        }
    }

    /// Construye un `LogGroupsLoaded` sin query (página inicial), para los tests.
    fn loaded(groups: Vec<LogGroupDto>) -> Message {
        Message::LogGroupsLoaded {
            groups,
            query: None,
            more: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        use ratatui::crossterm::event::KeyModifiers;
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn stream(name: &str) -> Message {
        Message::LogStreamsLoaded {
            group: "/svc".into(),
            streams: vec![LogStreamDto {
                name: name.to_string(),
                last_event_ts: None,
            }],
        }
    }

    fn ev(msg: &str, ts: i64) -> LogEventDto {
        LogEventDto {
            ts: Some(ts),
            message: msg.to_string(),
            stream: None,
        }
    }

    fn events_loaded(group: &str, stream: &str, events: Vec<LogEventDto>) -> Message {
        Message::LogEventsLoaded {
            group: group.into(),
            stream: stream.into(),
            events,
            more: false,
        }
    }

    fn tail_loaded(group: &str, query: Option<&str>, events: Vec<LogEventDto>) -> Message {
        Message::LogTailLoaded {
            group: group.into(),
            query: query.map(str::to_string),
            events,
            more: false,
        }
    }

    /// Helper: deja a la vista en `Level::Events` del stream `stream-a` de `/svc`.
    fn into_events(v: &mut LogsView) {
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Enter)); // groups → streams
        v.on_message(&stream("stream-a"));
        v.on_key(key(KeyCode::Enter)); // streams → events
    }

    #[test]
    fn enter_on_stream_drills_into_events() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Enter)); // groups → streams
        v.on_message(&stream("stream-a"));
        let actions = v.on_key(key(KeyCode::Enter)); // streams → events
        match actions.as_slice() {
            [Action::ClearFilter, Action::LoadLogEvents { group, stream }] => {
                assert_eq!(group, "/svc");
                assert_eq!(stream, "stream-a");
            }
            other => panic!("se esperaba ClearFilter+LoadLogEvents, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Events { .. }));
    }

    #[test]
    fn t_on_group_opens_tail() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        let actions = v.on_key(key(KeyCode::Char('t')));
        match actions.as_slice() {
            [Action::ClearFilter, Action::LoadLogTail { group, pattern }] => {
                assert_eq!(group, "/svc");
                assert!(pattern.is_none(), "el tail arranca sin filtro");
            }
            other => panic!("se esperaba ClearFilter+LoadLogTail, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Tail { .. }));
    }

    #[test]
    fn ingests_events_selects_bottom() {
        let mut v = LogsView::new();
        into_events(&mut v);
        v.on_message(&events_loaded(
            "/svc",
            "stream-a",
            vec![ev("a", 1), ev("b", 2), ev("c", 3)],
        ));
        assert_eq!(v.visible_len(), 3);
        assert_eq!(
            v.state.selected(),
            Some(2),
            "newest (último) preseleccionado, convención de terminal"
        );
    }

    #[test]
    fn events_from_wrong_stream_ignored() {
        let mut v = LogsView::new();
        into_events(&mut v); // events de stream-a
        v.on_message(&events_loaded("/svc", "otro-stream", vec![ev("x", 1)]));
        assert_eq!(v.visible_len(), 0, "no se aceptan eventos de otro stream");
    }

    #[test]
    fn tail_from_wrong_group_ignored() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // tail de /svc (last_query = None)
        v.on_message(&tail_loaded("/otro", None, vec![ev("x", 1)]));
        assert_eq!(v.visible_len(), 0, "tail de otro group ignorado");
    }

    #[test]
    fn tail_search_is_server_side() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t')));
        let actions = v.search("error");
        match actions.as_slice() {
            [Action::LoadLogTail { group, pattern }] => {
                assert_eq!(group, "/svc");
                assert_eq!(pattern.as_deref(), Some("error"), "filtro server-side");
            }
            other => panic!("el tail busca server-side (LoadLogTail), llegó {other:?}"),
        }
    }

    #[test]
    fn tail_discards_stale_results() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t')));
        let _ = v.search("err"); // last_query = Some("err")

        // Respuesta de un filtro VIEJO ("er") → descartada.
        v.on_message(&tail_loaded("/svc", Some("er"), vec![ev("viejo", 1)]));
        assert_eq!(v.visible_len(), 0, "respuesta de filtro viejo descartada");

        // Respuesta del filtro VIGENTE ("err") → aceptada.
        v.on_message(&tail_loaded(
            "/svc",
            Some("err"),
            vec![ev("error nuevo", 1)],
        ));
        assert_eq!(v.visible_len(), 1);
    }

    #[test]
    fn esc_in_events_pops_to_streams() {
        let mut v = LogsView::new();
        into_events(&mut v);
        assert!(matches!(v.level, Level::Events { .. }));
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(
            actions.is_empty(),
            "esc en events se consume en la vista (streams en cache)"
        );
        assert!(matches!(v.level, Level::Streams { .. }));
        assert_eq!(v.visible_len(), 1, "los streams siguen en cache");
    }

    #[test]
    fn esc_in_tail_pops_to_groups() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/a"), group("/b")]));
        v.on_key(key(KeyCode::Char('t'))); // tail de /a
        assert!(matches!(v.level, Level::Tail { .. }));
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(actions.is_empty(), "sin búsqueda previa, no recarga");
        assert!(matches!(v.level, Level::Groups));
        assert_eq!(v.visible_len(), 2, "los groups siguen en cache");
    }

    #[test]
    fn filter_narrows_events_local() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t')));
        v.on_message(&tail_loaded(
            "/svc",
            None,
            vec![ev("INFO ok", 1), ev("ERROR boom", 2)],
        ));
        assert_eq!(v.visible_len(), 2);
        v.set_filter("error");
        assert_eq!(
            v.visible_len(),
            1,
            "filtro local por substring (case-insensitive)"
        );
    }

    #[test]
    fn render_events_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        into_events(&mut v);
        v.on_message(&events_loaded(
            "/svc",
            "stream-a",
            vec![ev("INFO hello", 1), ev("ERROR boom", 2)],
        ));

        let mut terminal = Terminal::new(TestBackend::new(70, 8)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("boom"), "debe pintar la línea de log");
        assert!(text.contains("eventos"), "el título muestra el conteo");
    }

    #[test]
    fn render_tail_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t')));
        v.on_message(&tail_loaded(
            "/svc",
            None,
            vec![LogEventDto {
                ts: Some(1),
                message: "INFO a".into(),
                stream: Some("2026/06/21/[$LATEST]abc123".into()),
            }],
        ));

        let mut terminal = Terminal::new(TestBackend::new(70, 8)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("tail"), "el título muestra (tail)");
        assert!(text.contains("abc123"), "muestra el sufijo del stream");
    }

    #[test]
    fn activate_requests_log_groups() {
        let mut v = LogsView::new();
        let actions = v.on_activate();
        assert!(matches!(
            actions.as_slice(),
            [Action::LoadLogGroups { query: None }]
        ));
    }

    #[test]
    fn ingests_groups_via_message() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/aws/lambda/a"), group("/ecs/b")]));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn filter_narrows_the_list() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/aws/lambda/orders"),
            group("/aws/lambda/payments"),
            group("/ecs/checkout"),
        ]));
        v.set_filter("lambda");
        assert_eq!(v.visible_len(), 2);
        v.set_filter("CHECKOUT"); // case-insensitive
        assert_eq!(v.visible_len(), 1);
    }

    #[test]
    fn fuzzy_filter_ranks_best_match_first() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/aws/lambda/reordered-thing"),
            group("/aws/lambda/orders-api"),
            group("/ecs/checkout"),
        ]));
        // "ordapi" no es substring contiguo de ninguno, pero sí subsecuencia de orders-api.
        v.set_filter("ordapi");
        assert_eq!(v.visible_len(), 1);
        let first = v.filtered_group_indices()[0];
        assert_eq!(v.groups[first].name, "/aws/lambda/orders-api");
    }

    #[test]
    fn discards_stale_search_results() {
        let mut v = LogsView::new();
        // La vista pide la búsqueda "xy".
        let actions = v.search("xy");
        assert!(
            matches!(actions.as_slice(), [Action::LoadLogGroups { query: Some(q) }] if q == "xy")
        );

        // Llega una respuesta de una búsqueda VIEJA ("x") -> se descarta.
        v.on_message(&Message::LogGroupsLoaded {
            groups: vec![group("/vieja")],
            query: Some("x".into()),
            more: false,
        });
        assert_eq!(v.visible_len(), 0, "respuesta de búsqueda vieja descartada");

        // Llega la respuesta de la búsqueda vigente ("xy") -> se acepta.
        v.on_message(&Message::LogGroupsLoaded {
            groups: vec![group("/aws/xy-thing")],
            query: Some("xy".into()),
            more: true,
        });
        assert_eq!(v.visible_len(), 1);
        assert!(v.partial, "more=true marca la lista como parcial");
    }

    #[test]
    fn enter_drills_into_selected_group_streams() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/aws/lambda/orders"),
            group("/ecs/checkout"),
        ]));
        v.on_key(key(KeyCode::Down)); // selecciona el segundo
        let actions = v.on_key(key(KeyCode::Enter));
        match actions.as_slice() {
            [Action::ClearFilter, Action::LoadLogStreams { group }] => {
                assert_eq!(group, "/ecs/checkout")
            }
            other => panic!("se esperaba ClearFilter+LoadLogStreams, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Streams { .. }));

        // Llega la data de streams del group correcto.
        v.on_message(&Message::LogStreamsLoaded {
            group: "/ecs/checkout".into(),
            streams: vec![
                LogStreamDto {
                    name: "s1".into(),
                    last_event_ts: Some(1_700_000_000_000),
                },
                LogStreamDto {
                    name: "s2".into(),
                    last_event_ts: None,
                },
            ],
        });
        assert_eq!(v.visible_len(), 2);

        // esc regresa a groups.
        v.on_key(key(KeyCode::Esc));
        assert!(matches!(v.level, Level::Groups));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn esc_at_root_emits_back() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/a"), group("/b")]));
        // En la raíz (groups) no hay drill que despojar: esc pide volver al menú.
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
        assert!(
            matches!(v.level, Level::Groups),
            "esc en raíz no cambia nivel"
        );
    }

    #[test]
    fn esc_in_streams_pops_to_groups_without_back() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/a")]));
        v.on_key(key(KeyCode::Enter)); // drill a streams
        assert!(matches!(v.level, Level::Streams { .. }));
        // esc despoja un nivel (no emite Back: aún hay a dónde volver dentro de la vista).
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(actions.is_empty(), "esc en streams se consume en la vista");
        assert!(matches!(v.level, Level::Groups));
    }

    #[test]
    fn debounced_reload_does_not_stomp_navigation() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/svc-a"),
            group("/svc-b"),
            group("/svc-c"),
        ]));
        // El usuario lanza una búsqueda server-side y, antes de que responda, navega.
        let _ = v.search("svc"); // last_query = Some("svc")
        v.on_key(key(KeyCode::Down));
        v.on_key(key(KeyCode::Down)); // en "/svc-c"
        assert_eq!(v.selected_group_name().as_deref(), Some("/svc-c"));

        // Llega la respuesta debounced (misma data): la selección NO salta al tope.
        v.on_message(&Message::LogGroupsLoaded {
            groups: vec![group("/svc-a"), group("/svc-b"), group("/svc-c")],
            query: Some("svc".into()),
            more: false,
        });
        assert_eq!(
            v.selected_group_name().as_deref(),
            Some("/svc-c"),
            "la recarga async no debe pisar la navegación del usuario"
        );
    }

    #[test]
    fn reload_falls_back_to_top_when_selection_gone() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/a"), group("/b"), group("/c")]));
        v.on_key(key(KeyCode::Down));
        v.on_key(key(KeyCode::Down)); // en "/c"
        assert_eq!(v.selected_group_name().as_deref(), Some("/c"));

        // La recarga ya no contiene "/c": la selección cae al tope (mejor match).
        v.on_message(&loaded(vec![group("/a"), group("/b")]));
        assert_eq!(v.selected_group_name().as_deref(), Some("/a"));
    }

    #[test]
    fn esc_at_root_empty_list_emits_back() {
        // El caso más común del bug original: vista recién activada (sin data) + esc.
        let mut v = LogsView::new();
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
    }

    #[test]
    fn arrow_on_empty_filter_is_safe() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/a")]));
        v.set_filter("zzz"); // 0 coincidencias
        assert_eq!(v.visible_len(), 0);
        // Navegar sobre una lista filtrada vacía (ruta de las flechas en filtro) no
        // debe panickear: move_selection cae en len==0 → select(None).
        v.on_key(key(KeyCode::Down));
        assert_eq!(v.state.selected(), None);
    }

    #[test]
    fn streams_from_wrong_group_are_ignored() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/a")]));
        v.on_key(key(KeyCode::Enter)); // drill a /a
        v.on_message(&Message::LogStreamsLoaded {
            group: "/otro".into(), // group equivocado
            streams: vec![LogStreamDto {
                name: "x".into(),
                last_event_ts: None,
            }],
        });
        assert_eq!(v.visible_len(), 0, "no se aceptan streams de otro group");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }

    #[test]
    fn render_lists_groups_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/aws/lambda/orders"),
            group("/ecs/checkout"),
        ]));

        let mut terminal = Terminal::new(TestBackend::new(70, 8)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("orders"), "debe listar el group seleccionado");
        assert!(text.contains("log groups"), "el título muestra el conteo");
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
