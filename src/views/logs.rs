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
use crate::message::{LogGroupDto, LogStreamDto, Message};

/// Nivel de drill actual.
enum Level {
    Groups,
    Streams { group: String },
}

pub struct LogsView {
    level: Level,
    groups: Vec<LogGroupDto>,
    streams: Vec<LogStreamDto>,
    filter: String,
    loading: bool,
    /// Selección de la lista visible (índice dentro de la lista filtrada).
    state: ListState,
}

impl LogsView {
    pub fn new() -> Self {
        Self {
            level: Level::Groups,
            groups: Vec::new(),
            streams: Vec::new(),
            filter: String::new(),
            loading: false,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    // --- Filtrado / selección -------------------------------------------------

    fn filtered_group_indices(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.groups
            .iter()
            .enumerate()
            .filter(|(_, g)| f.is_empty() || g.name.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }

    fn filtered_stream_indices(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.streams
            .iter()
            .enumerate()
            .filter(|(_, s)| f.is_empty() || s.name.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }

    fn visible_len(&self) -> usize {
        match self.level {
            Level::Groups => self.filtered_group_indices().len(),
            Level::Streams { .. } => self.filtered_stream_indices().len(),
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

    fn selected_group_name(&self) -> Option<String> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_group_indices().get(sel)?;
        Some(self.groups[idx].name.clone())
    }

    // --- Navegación -----------------------------------------------------------

    fn drill(&mut self) -> Vec<Action> {
        match self.level {
            Level::Groups => match self.selected_group_name() {
                Some(group) => {
                    self.level = Level::Streams {
                        group: group.clone(),
                    };
                    self.streams.clear();
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![Action::LoadLogStreams { group }]
                }
                None => vec![],
            },
            // Ya en el último nivel: enter no hace nada (v0 es solo-lectura).
            Level::Streams { .. } => vec![],
        }
    }

    fn back(&mut self) {
        if matches!(self.level, Level::Streams { .. }) {
            self.level = Level::Groups;
            self.loading = false; // los groups siguen en cache
            self.state.select(Some(0));
            self.clamp_selection();
        }
    }

    fn refresh(&mut self) -> Vec<Action> {
        self.loading = true;
        match &self.level {
            Level::Groups => vec![Action::LoadLogGroups],
            Level::Streams { group } => vec![Action::LoadLogStreams {
                group: group.clone(),
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
        };
        if self.filter.is_empty() {
            format!(" {total} {kind} ")
        } else {
            format!(" {shown}/{total} {kind} · filtro: {} ", self.filter)
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

    fn title(&self) -> String {
        match &self.level {
            Level::Groups => "logs".to_string(),
            Level::Streams { group } => format!("logs / {group}"),
        }
    }

    fn on_activate(&mut self) -> Vec<Action> {
        self.level = Level::Groups;
        self.streams.clear();
        self.loading = true;
        self.state.select(Some(0));
        vec![Action::LoadLogGroups]
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
            KeyCode::Esc => {
                self.back();
                vec![]
            }
            KeyCode::Char('r') => self.refresh(),
            _ => vec![],
        }
    }

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::LogGroupsLoaded(groups) => {
                self.groups = groups.clone();
                if matches!(self.level, Level::Groups) {
                    self.loading = false;
                    self.clamp_selection();
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
            // El App ya muestra el error en la status bar; aquí cortamos el loading.
            Message::Error(_) => self.loading = false,
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.clamp_selection();
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

/// Epoch en milisegundos -> `YYYY-MM-DD HH:MM:SSZ` (UTC), sin crate de fechas.
fn fmt_epoch_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Días desde 1970-01-01 -> (año, mes, día). Algoritmo de Howard Hinnant.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
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

    fn key(code: KeyCode) -> KeyEvent {
        use ratatui::crossterm::event::KeyModifiers;
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn activate_requests_log_groups() {
        let mut v = LogsView::new();
        let actions = v.on_activate();
        assert!(matches!(actions.as_slice(), [Action::LoadLogGroups]));
    }

    #[test]
    fn ingests_groups_via_message() {
        let mut v = LogsView::new();
        v.on_message(&Message::LogGroupsLoaded(vec![
            group("/aws/lambda/a"),
            group("/ecs/b"),
        ]));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn filter_narrows_the_list() {
        let mut v = LogsView::new();
        v.on_message(&Message::LogGroupsLoaded(vec![
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
    fn enter_drills_into_selected_group_streams() {
        let mut v = LogsView::new();
        v.on_message(&Message::LogGroupsLoaded(vec![
            group("/aws/lambda/orders"),
            group("/ecs/checkout"),
        ]));
        v.on_key(key(KeyCode::Down)); // selecciona el segundo
        let actions = v.on_key(key(KeyCode::Enter));
        match actions.as_slice() {
            [Action::LoadLogStreams { group }] => assert_eq!(group, "/ecs/checkout"),
            other => panic!("se esperaba LoadLogStreams, llegó {other:?}"),
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
    fn streams_from_wrong_group_are_ignored() {
        let mut v = LogsView::new();
        v.on_message(&Message::LogGroupsLoaded(vec![group("/a")]));
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
    fn formats_bytes_and_timestamps() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        // 1700000000000 ms = 2023-11-14 22:13:20 UTC
        assert_eq!(fmt_epoch_millis(1_700_000_000_000), "2023-11-14 22:13:20Z");
    }

    #[test]
    fn render_lists_groups_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        v.on_message(&Message::LogGroupsLoaded(vec![
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
