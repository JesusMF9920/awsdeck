//! Vista `logs`: CloudWatch Log Groups -> drill a Log Streams. Pura y síncrona:
//! mantiene estado (groups/streams, nivel de drill, selección, filtro), traduce
//! teclas a `Action`s y dibuja. NUNCA importa `aws-sdk-*`; recibe datos vía
//! `on_message` con DTOs planos, así que se testea inyectando `Message`s.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::View;
use crate::action::Action;
use crate::message::{LogEventDto, LogGroupDto, LogStreamDto, LogWindow, Message};
use crate::util::{
    fmt_clock_millis, fmt_epoch_millis, fuzzy_score, parse_datetime, parse_duration, ranked,
};

/// Ventanas de tiempo cicladas con `w`/`W` (etiqueta + millis). `:since`/`:from`/`:to`
/// pueden fijar ventanas fuera de esta lista.
const WINDOW_PRESETS: [(&str, i64); 6] = [
    ("15m", 15 * 60_000),
    ("1h", 60 * 60_000),
    ("6h", 6 * 60 * 60_000),
    ("24h", 24 * 60 * 60_000),
    ("3d", 3 * 24 * 60 * 60_000),
    ("7d", 7 * 24 * 60 * 60_000),
];
/// Preset por defecto al abrir el tail (índice en `WINDOW_PRESETS`): `1h`.
const DEFAULT_PRESET: usize = 1;

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
    /// Display precomputado de cada evento (lowercase + preview + estilo), paralelo a
    /// `events`. Evita recomputar `to_lowercase()`/`replace()`/severidad por tecla y por
    /// frame en buffers grandes (rangos amplios → hasta ~10k líneas). Se mantiene en sync
    /// vía `set_events`/`extend_events`.
    event_rows: Vec<EventRow>,
    /// Índices visibles (post-filtro) del nivel actual: la fuente de verdad para
    /// navegación y render. Se recomputa solo al cambiar filtro/lista/nivel
    /// (`recompute_filtered`), no en cada pulsación ni en cada frame.
    filtered: Vec<usize>,
    filter: String,
    loading: bool,
    /// Patrón server-side activo del **tail** (`filter_pattern`); `None` = sin filtro.
    /// (Los groups filtran 100% local, no usan este campo.)
    last_query: Option<String>,
    /// `true` si el servidor tiene más groups que los traídos (next_token).
    partial: bool,
    /// Ventana de tiempo activa del tail (logs del group).
    tail_window: LogWindow,
    /// Índice del preset activo en `WINDOW_PRESETS` (para ciclar con `w`/`W`).
    tail_preset: usize,
    /// `next_token` de la última página del tail (para `o` = cargar más). `None` = no hay más.
    tail_token: Option<String>,
    /// Generación de la consulta de tail vigente: sube en cada consulta *fresca*
    /// (ventana/patrón/drill) y NO en load-more; descarta respuestas con generation viejo.
    tail_gen: u64,
    /// Línea expandida: índice ABSOLUTO en `events` del evento abierto en el panel de
    /// detalle (`enter` sobre una línea). `None` = mostrando la lista.
    detail: Option<usize>,
    /// Scroll vertical del panel de detalle (líneas).
    detail_scroll: u16,
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
            event_rows: Vec::new(),
            filtered: Vec::new(),
            filter: String::new(),
            loading: false,
            last_query: None,
            partial: false,
            tail_window: LogWindow::Last(WINDOW_PRESETS[DEFAULT_PRESET].1),
            tail_preset: DEFAULT_PRESET,
            tail_token: None,
            tail_gen: 0,
            detail: None,
            detail_scroll: 0,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    /// Abre el panel de detalle del evento seleccionado (índice absoluto en `events`).
    fn open_detail(&mut self) {
        if let Some(sel) = self.state.selected()
            && let Some(&idx) = self.filtered.get(sel)
        {
            self.detail = Some(idx);
            self.detail_scroll = 0;
        }
    }

    fn close_detail(&mut self) {
        self.detail = None;
        self.detail_scroll = 0;
    }

    /// Teclas mientras el panel de detalle está abierto: scroll + cerrar.
    fn detail_key(&mut self, key: KeyEvent) -> Vec<Action> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.detail_scroll = self.detail_scroll.saturating_add(1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.detail_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.detail_scroll = u16::MAX, // clamp al render
            KeyCode::Esc | KeyCode::Enter => self.close_detail(),
            _ => {}
        }
        vec![]
    }

    /// Group del nivel actual: el seleccionado en `Groups`, o el del nivel si ya estás
    /// dentro de uno. `None` solo en `Groups` sin selección.
    fn current_group(&self) -> Option<String> {
        match &self.level {
            Level::Groups => self.selected_group_name(),
            Level::Streams { group } | Level::Tail { group } => Some(group.clone()),
            Level::Events { group, .. } => Some(group.clone()),
        }
    }

    /// Construye la consulta de tail con la ventana/patrón actuales. Sin `token` es una
    /// consulta *fresca* → sube `tail_gen` (descarta respuestas viejas en vuelo); con
    /// `token` es load-more → conserva `generation` para que su respuesta sí se acepte.
    fn tail_query(&mut self, token: Option<String>) -> Vec<Action> {
        let Level::Tail { group } = &self.level else {
            return vec![];
        };
        let group = group.clone();
        if token.is_none() {
            self.tail_gen = self.tail_gen.wrapping_add(1);
        }
        self.loading = true;
        vec![Action::LoadLogTail {
            group,
            pattern: self.last_query.clone(),
            window: self.tail_window,
            token,
            generation: self.tail_gen,
        }]
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

    /// Filtro de líneas de log: substring case-insensitive sobre el mensaje (no fuzzy —
    /// más apropiado para texto largo), preservando el orden cronológico. Usa el
    /// lowercase precomputado (`event_rows`), así que no aloca por evento.
    fn filtered_event_indices(&self) -> Vec<usize> {
        let needle = self.filter.to_lowercase();
        (0..self.event_rows.len())
            .filter(|&i| needle.is_empty() || self.event_rows[i].lc.contains(&needle))
            .collect()
    }

    /// Recalcula `self.filtered` (índices visibles del nivel actual). Se llama solo al
    /// cambiar filtro/lista/nivel — nunca por tecla de navegación ni por frame.
    fn recompute_filtered(&mut self) {
        self.filtered = match self.level {
            Level::Groups => self.filtered_group_indices(),
            Level::Streams { .. } => self.filtered_stream_indices(),
            Level::Events { .. } | Level::Tail { .. } => self.filtered_event_indices(),
        };
    }

    /// Reemplaza el buffer de eventos y su display precomputado, y recomputa el filtro.
    /// Único punto (con `extend_events`) que muta `events`/`event_rows` en sync.
    fn set_events(&mut self, events: Vec<LogEventDto>) {
        self.event_rows = events.iter().map(EventRow::new).collect();
        self.events = events;
        self.recompute_filtered();
    }

    /// Append (load-more del tail): extiende eventos + display y recomputa el filtro.
    fn extend_events(&mut self, events: Vec<LogEventDto>) {
        self.event_rows.extend(events.iter().map(EventRow::new));
        self.events.extend(events);
        self.recompute_filtered();
    }

    fn visible_len(&self) -> usize {
        self.filtered.len()
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
            self.filtered.iter().position(|&i| self.groups[i].name == n)
        });
        self.state.select(Some(pos.unwrap_or(0)));
        self.clamp_selection();
    }

    fn selected_group_name(&self) -> Option<String> {
        let sel = self.state.selected()?;
        let idx = *self.filtered.get(sel)?;
        Some(self.groups[idx].name.clone())
    }

    fn selected_stream_name(&self) -> Option<String> {
        let sel = self.state.selected()?;
        let idx = *self.filtered.get(sel)?;
        Some(self.streams[idx].name.clone())
    }

    /// Texto a copiar con `y` según el contexto: en el detalle, el mensaje completo de
    /// la línea; en groups, el ARN del group (o el nombre); en streams, el nombre del
    /// stream; en las hojas de líneas, el mensaje de la línea seleccionada.
    fn copy_text(&self) -> Option<String> {
        if let Some(idx) = self.detail {
            return self.events.get(idx).map(|e| e.message.clone());
        }
        match self.level {
            Level::Groups => {
                let idx = *self.filtered.get(self.state.selected()?)?;
                let g = &self.groups[idx];
                Some(g.arn.clone().unwrap_or_else(|| g.name.clone()))
            }
            Level::Streams { .. } => self.selected_stream_name(),
            Level::Events { .. } | Level::Tail { .. } => {
                let idx = *self.filtered.get(self.state.selected()?)?;
                Some(self.events[idx].message.clone())
            }
        }
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
                    self.recompute_filtered();
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
                        self.set_events(Vec::new());
                        self.loading = true;
                        self.state.select(Some(0));
                        vec![Action::ClearFilter, Action::LoadLogEvents { group, stream }]
                    }
                    None => vec![],
                }
            }
            // Hojas (líneas de log): enter expande la línea seleccionada.
            Level::Events { .. } | Level::Tail { .. } => {
                self.open_detail();
                vec![]
            }
        }
    }

    /// `t`: logs del group (`filter_log_events` sobre todos sus streams) en la ventana
    /// por defecto (1h). Resuelve el group del nivel actual (seleccionado en `Groups`).
    /// `esc` vuelve a groups. `w`/`o`/`:since`/`:from-to` ajustan rango/paginación.
    fn tail(&mut self) -> Vec<Action> {
        self.open_tail(
            LogWindow::Last(WINDOW_PRESETS[DEFAULT_PRESET].1),
            DEFAULT_PRESET,
        )
    }

    /// Abre (o reabre) el nivel `Tail` del group actual con `window`, resetea filtro y
    /// dispara una consulta fresca. `preset` mantiene el ciclado de `w` coherente.
    fn open_tail(&mut self, window: LogWindow, preset: usize) -> Vec<Action> {
        match self.current_group() {
            Some(group) => {
                self.level = Level::Tail { group };
                self.set_events(Vec::new());
                self.tail_window = window;
                self.tail_preset = preset;
                self.tail_token = None;
                self.last_query = None; // el tail arranca sin filtro server-side
                self.state.select(Some(0));
                let mut actions = vec![Action::ClearFilter];
                actions.extend(self.tail_query(None));
                actions
            }
            None => vec![],
        }
    }

    /// `w`/`W`: cicla la ventana entre presets y re-consulta (solo en `Tail`).
    fn cycle_window(&mut self, forward: bool) -> Vec<Action> {
        if !matches!(self.level, Level::Tail { .. }) {
            return vec![];
        }
        let n = WINDOW_PRESETS.len();
        self.tail_preset = if forward {
            (self.tail_preset + 1) % n
        } else {
            (self.tail_preset + n - 1) % n
        };
        self.tail_window = LogWindow::Last(WINDOW_PRESETS[self.tail_preset].1);
        self.set_events(Vec::new());
        self.tail_token = None;
        self.state.select(Some(0));
        self.tail_query(None)
    }

    /// `o`: carga la siguiente página del tail (append) si hay `next_token`.
    fn load_more(&mut self) -> Vec<Action> {
        match (&self.level, self.tail_token.clone()) {
            (Level::Tail { .. }, Some(token)) => self.tail_query(Some(token)),
            _ => vec![],
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
                self.recompute_filtered();
                self.state.select(Some(0));
                self.clamp_selection();
                self.loading = false;
                vec![]
            }
            // Streams/Tail → Groups. Los groups ya están completos en cache (carga
            // sin filtro server-side), así que volver no recarga: solo cambia de nivel.
            Level::Streams { .. } | Level::Tail { .. } => {
                self.level = Level::Groups;
                self.recompute_filtered();
                self.state.select(Some(0));
                self.clamp_selection();
                self.loading = false;
                vec![]
            }
            Level::Groups => vec![Action::Back],
        }
    }

    fn refresh(&mut self) -> Vec<Action> {
        // El tail recarga la ventana/patrón actuales (consulta fresca, sube generation).
        if matches!(self.level, Level::Tail { .. }) {
            self.set_events(Vec::new());
            self.tail_token = None;
            self.state.select(Some(0));
            return self.tail_query(None);
        }
        self.loading = true;
        match &self.level {
            // Recargar la lista completa de groups.
            Level::Groups => vec![Action::LoadLogGroups],
            Level::Streams { group } => vec![Action::LoadLogStreams {
                group: group.clone(),
            }],
            Level::Events { group, stream } => vec![Action::LoadLogEvents {
                group: group.clone(),
                stream: stream.clone(),
            }],
            Level::Tail { .. } => vec![], // manejado arriba
        }
    }

    // --- Render ---------------------------------------------------------------

    /// Etiqueta legible de la ventana del tail (deriva de `tail_window`, no del preset).
    fn window_label(&self) -> String {
        match self.tail_window {
            LogWindow::Last(ms) => fmt_window_duration(ms),
            LogWindow::Range { from, to } => match to {
                Some(t) => format!("{}→{}", short_dt(from), short_dt(t)),
                None => format!("{}→ahora", short_dt(from)),
            },
        }
    }

    fn body_title(&self) -> String {
        let shown = self.filtered.len();
        let (kind, total) = match self.level {
            Level::Groups => ("log groups", self.groups.len()),
            Level::Streams { .. } => ("streams", self.streams.len()),
            Level::Events { .. } => ("eventos", self.events.len()),
            Level::Tail { .. } => ("líneas", self.events.len()),
        };
        // En el tail, antepone la ventana de tiempo activa.
        let window = if matches!(self.level, Level::Tail { .. }) {
            format!("ventana {} · ", self.window_label())
        } else {
            String::new()
        };
        let partial = if !self.partial {
            ""
        } else {
            match self.level {
                Level::Groups => " · parcial (tope de páginas)",
                Level::Events { .. } => " · parcial (más viejas arriba)",
                Level::Tail { .. } => " · parcial · o: cargar más",
                Level::Streams { .. } => "",
            }
        };
        if self.filter.is_empty() {
            // Cue del hermano `Tail`: en groups/streams recuerda que `t` ve TODOS los
            // streams del group por rango de tiempo (segundo canal, además del footer).
            let cue = match self.level {
                Level::Groups => " · t: logs por tiempo",
                Level::Streams { .. } => " · t: todos los streams por tiempo",
                _ => "",
            };
            format!(" {window}{total} {kind}{partial}{cue} ")
        } else {
            format!(
                " {window}{shown}/{total} {kind} · filtro: {}{partial} ",
                self.filter
            )
        }
    }

    /// Pinta el evento expandido: cabecera (hora · stream) + mensaje completo con wrap
    /// y scroll; JSON pretty si parsea. Ocupa el cuerpo entero (en vez de la lista).
    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        let Some(idx) = self.detail else { return };
        if idx >= self.events.len() {
            // El evento ya no existe (una recarga lo movió): cierra el detalle.
            self.close_detail();
            frame.render_widget(
                Paragraph::new("(evento no disponible)").block(Block::bordered()),
                area,
            );
            return;
        }
        let e = &self.events[idx];
        let when =
            e.ts.map(fmt_epoch_millis)
                .unwrap_or_else(|| "—".to_string());
        let stream = e
            .stream
            .as_deref()
            .map(|s| format!(" · {s}"))
            .unwrap_or_default();
        let body = pretty_or_raw(&e.message); // string propio: libera el préstamo de `e`
        let total = body.lines().count() as u16;

        // Clampa el scroll (soporta `G` = u16::MAX) al final del contenido.
        self.detail_scroll = self.detail_scroll.min(total.saturating_sub(1));

        let title = format!(" {when}{stream} · esc cierra · j/k scroll ");
        let para = Paragraph::new(body)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.detail_scroll, 0));
        frame.render_widget(para, area);
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

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        // Con el detalle abierto, las teclas scrollean/cierran/copian.
        if self.detail.is_some() {
            return vec![("j/k", "scroll"), ("y", "copiar"), ("esc", "cerrar")];
        }
        match self.level {
            // `t` abre el tail (todos los streams del group por rango): es el feature
            // que cuesta descubrir, así que se anuncia donde aplica.
            Level::Groups | Level::Streams { .. } => vec![("t", "logs por tiempo"), ("y", "copiar")],
            // Dentro del tail: cómo cambiar la ventana / paginar / fijar rango.
            Level::Tail { .. } => vec![("w", "ventana"), ("o", "más"), (":since", "rango")],
            Level::Events { .. } => vec![("y", "copiar línea")],
        }
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
        self.set_events(Vec::new()); // limpia events + event_rows + recomputa filtered
        self.loading = true;
        self.last_query = None;
        self.partial = false;
        self.tail_token = None;
        self.tail_preset = DEFAULT_PRESET;
        self.tail_window = LogWindow::Last(WINDOW_PRESETS[DEFAULT_PRESET].1);
        self.state.select(Some(0));
        vec![Action::LoadLogGroups]
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // `y` copia al portapapeles en cualquier nivel (incluido el detalle), antes de
        // que el panel de detalle capture las teclas.
        if key.code == KeyCode::Char('y') {
            return self
                .copy_text()
                .map(|text| Action::CopyToClipboard { text })
                .into_iter()
                .collect();
        }
        // Con el panel de detalle abierto, las teclas scrollean/cierran.
        if self.detail.is_some() {
            return self.detail_key(key);
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
            KeyCode::Enter => self.drill(),
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') => self.refresh(),
            // Logs del group (todos sus streams) por rango de tiempo.
            KeyCode::Char('t') => self.tail(),
            // En el tail: `w`/`W` ciclan la ventana, `o` carga más (paginación).
            KeyCode::Char('w') => self.cycle_window(true),
            KeyCode::Char('W') => self.cycle_window(false),
            KeyCode::Char('o') => self.load_more(),
            _ => vec![],
        }
    }

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::LogGroupsLoaded { groups, more } => {
                // Capturar la selección ANTES de reemplazar, para preservarla por
                // nombre y no pisar la navegación del usuario con la recarga async.
                let keep = self.selected_group_name();
                self.groups = groups.clone();
                self.partial = *more;
                if matches!(self.level, Level::Groups) {
                    self.loading = false;
                    self.recompute_filtered();
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
                    self.recompute_filtered();
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
                    self.set_events(events.clone());
                    self.partial = *more;
                    self.loading = false;
                    self.select_edge(true); // newest abajo (convención de terminal)
                }
            }
            Message::LogTailLoaded {
                group,
                events,
                next_token,
                append,
                generation,
            } => {
                // Descarta respuestas de una consulta vieja (ventana/patrón/drill
                // cambiaron → subió el generation) y de otro group.
                if *generation != self.tail_gen {
                    return;
                }
                if let Level::Tail { group: g } = &self.level
                    && g == group
                {
                    if *append {
                        self.extend_events(events.clone());
                    } else {
                        self.set_events(events.clone());
                    }
                    self.tail_token = next_token.clone();
                    self.partial = next_token.is_some();
                    self.loading = false;
                    // En carga fresca, salta al fondo (newest abajo); en append conserva
                    // la posición del usuario.
                    if !*append {
                        self.select_edge(true);
                    }
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
        self.recompute_filtered();
        self.state.select(Some(0)); // top = mejor match (estilo fzf)
        self.clamp_selection();
    }

    fn search(&mut self, query: &str) -> Vec<Action> {
        // Solo el tail busca server-side (filter_pattern). Groups (y los demás
        // niveles) filtran local: groups con fuzzy sobre el cache completo.
        match &self.level {
            // Groups filtra 100% local (fuzzy sobre el cache completo): no consulta al
            // server. El tail sí busca server-side (filter_pattern).
            Level::Groups => Vec::new(),
            Level::Tail { .. } => {
                self.last_query = (!query.is_empty()).then(|| query.to_string());
                self.set_events(Vec::new());
                self.tail_token = None;
                self.state.select(Some(0));
                self.tail_query(None) // consulta fresca (sube generation)
            }
            _ => Vec::new(),
        }
    }

    fn on_command(&mut self, cmd: &str) -> Vec<Action> {
        // Solo comandos de rango de tiempo; el resto no es nuestro (Vec vacío → el App
        // lo trata como id de vista). `:since <dur>` / `:from <dt> [to <dt>]` (UTC).
        let (verb, rest) = match cmd.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (cmd, ""),
        };
        let window = match verb {
            "since" => match parse_duration(rest) {
                Some(ms) => LogWindow::Last(ms),
                None => return Vec::new(),
            },
            "from" => {
                let (from_s, to_s) = match rest.split_once(" to ") {
                    Some((f, t)) => (f.trim(), Some(t.trim())),
                    None => (rest, None),
                };
                let Some(from) = parse_datetime(from_s) else {
                    return Vec::new();
                };
                let to = match to_s {
                    Some(t) => match parse_datetime(t) {
                        Some(x) => Some(x),
                        None => return Vec::new(),
                    },
                    None => None,
                };
                LogWindow::Range { from, to }
            }
            _ => return Vec::new(),
        };
        // Ventana custom: la etiqueta sale de la ventana, no del preset; conserva el
        // preset actual para que `w` siga ciclando desde un índice válido (sin overflow).
        let preset = self.tail_preset;
        self.open_tail(window, preset)
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Panel de detalle: ocupa el cuerpo entero con el mensaje completo.
        if self.detail.is_some() {
            self.render_detail(frame, area);
            return;
        }

        let block = Block::bordered().title(self.body_title());

        let items: Vec<ListItem> = match self.level {
            Level::Groups => self
                .filtered
                .iter()
                .map(|&i| group_item(&self.groups[i]))
                .collect(),
            Level::Streams { .. } => self
                .filtered
                .iter()
                .map(|&i| stream_item(&self.streams[i]))
                .collect(),
            Level::Events { .. } | Level::Tail { .. } => self
                .filtered
                .iter()
                .map(|&i| event_item(&self.event_rows[i]))
                .collect(),
        };

        if items.is_empty() {
            let msg = if self.loading {
                "cargando…"
            } else if !self.filter.is_empty() {
                "(sin coincidencias para el filtro)"
            } else {
                // Mensaje por nivel: deja claro que un stream/tail vacío no es un bug.
                match self.level {
                    Level::Events { .. } => "(este stream no tiene eventos)",
                    Level::Tail { .. } => {
                        "(sin eventos en la última hora — prueba otro group o `r`)"
                    }
                    _ => "(sin resultados)",
                }
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

/// Display precomputado de una línea de log: todo lo caro (lowercase para el filtro,
/// preview colapsado, formato de hora/stream, color por severidad) se calcula una vez al
/// ingerir el evento, no por tecla ni por frame.
struct EventRow {
    /// `HH:MM:SS` (o `--:--:--`), tenue.
    clock: String,
    /// Sufijo corto del stream (solo presente en el tail, donde se mezclan streams).
    stream: Option<String>,
    /// Mensaje colapsado a una línea (sin saltos) para la fila.
    preview: String,
    /// Color por severidad del mensaje.
    style: Style,
    /// Mensaje en lowercase, para el filtro substring case-insensitive sin alocar.
    lc: String,
}

impl EventRow {
    fn new(e: &LogEventDto) -> Self {
        EventRow {
            clock: e
                .ts
                .map(fmt_clock_millis)
                .unwrap_or_else(|| "--:--:--".to_string()),
            stream: e.stream.as_deref().map(stream_suffix),
            preview: e.message.replace(['\n', '\r'], " "),
            style: severity_style(&e.message),
            lc: e.message.to_lowercase(),
        }
    }
}

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

/// Una línea de log: `HH:MM:SS  [stream]  message`, ensamblada desde el `EventRow`
/// precomputado (sin recomputar lowercase/preview/severidad por frame). El `[stream]`
/// solo aparece en el tail (varios streams mezclados).
fn event_item(row: &EventRow) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(row.clock.clone(), Style::new().dark_gray()),
        Span::raw("  "),
    ];
    if let Some(stream) = &row.stream {
        spans.push(Span::styled(format!("{stream}  "), Style::new().cyan()));
    }
    spans.push(Span::styled(row.preview.clone(), row.style));
    ListItem::new(Line::from(spans))
}

/// Pretty-print del mensaje si es JSON válido; si no, el texto tal cual. Para el panel
/// de detalle (`enter`), donde sí queremos saltos de línea e indentación.
fn pretty_or_raw(msg: &str) -> String {
    serde_json::from_str::<serde_json::Value>(msg.trim())
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| msg.to_string())
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

/// Millis de ventana → etiqueta corta (`90m`, `6h`, `2d`). Deriva la unidad de la
/// divisibilidad para que `:since 90m` muestre `90m` y `Last(6h)` muestre `6h`.
fn fmt_window_duration(ms: i64) -> String {
    let m = ms / 60_000;
    if m % (60 * 24) == 0 {
        format!("{}d", m / (60 * 24))
    } else if m % 60 == 0 {
        format!("{}h", m / 60)
    } else {
        format!("{m}m")
    }
}

/// Epoch millis → `YYYY-MM-DD HH:MM` (UTC, sin segundos), para rótulos de rango.
fn short_dt(ms: i64) -> String {
    let s = fmt_epoch_millis(ms);
    s.get(..16).map(str::to_string).unwrap_or(s)
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

    /// Construye un `LogGroupsLoaded` para los tests.
    fn loaded(groups: Vec<LogGroupDto>) -> Message {
        Message::LogGroupsLoaded { groups, more: false }
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

    /// Respuesta de tail fresca (no-append, sin más páginas) con la `generation` dada.
    fn tail_loaded(group: &str, generation: u64, events: Vec<LogEventDto>) -> Message {
        Message::LogTailLoaded {
            group: group.into(),
            events,
            next_token: None,
            append: false,
            generation,
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
            [
                Action::ClearFilter,
                Action::LoadLogTail {
                    group,
                    pattern,
                    window,
                    token,
                    ..
                },
            ] => {
                assert_eq!(group, "/svc");
                assert!(pattern.is_none(), "el tail arranca sin filtro");
                assert_eq!(*window, LogWindow::Last(WINDOW_PRESETS[1].1), "default 1h");
                assert!(token.is_none(), "consulta fresca, sin paginar");
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
        v.on_key(key(KeyCode::Char('t'))); // tail de /svc → gen=1
        v.on_message(&tail_loaded("/otro", 1, vec![ev("x", 1)]));
        assert_eq!(v.visible_len(), 0, "tail de otro group ignorado");
    }

    #[test]
    fn tail_search_is_server_side() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t')));
        let actions = v.search("error");
        match actions.as_slice() {
            [
                Action::LoadLogTail {
                    group,
                    pattern,
                    token,
                    ..
                },
            ] => {
                assert_eq!(group, "/svc");
                assert_eq!(pattern.as_deref(), Some("error"), "filtro server-side");
                assert!(token.is_none(), "buscar es consulta fresca, no paginar");
            }
            other => panic!("el tail busca server-side (LoadLogTail), llegó {other:?}"),
        }
    }

    #[test]
    fn tail_discards_stale_results() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // gen=1
        let _ = v.search("err"); // consulta fresca → gen=2

        // Respuesta de una generación VIEJA (gen=1) → descartada.
        v.on_message(&tail_loaded("/svc", 1, vec![ev("viejo", 1)]));
        assert_eq!(
            v.visible_len(),
            0,
            "respuesta de generación vieja descartada"
        );

        // Respuesta de la generación VIGENTE (gen=2) → aceptada.
        v.on_message(&tail_loaded("/svc", 2, vec![ev("error nuevo", 1)]));
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
        v.on_key(key(KeyCode::Char('t'))); // gen=1
        v.on_message(&tail_loaded(
            "/svc",
            1,
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
            1,
            vec![LogEventDto {
                ts: Some(1),
                message: "INFO a".into(),
                stream: Some("2026/06/21/[$LATEST]abc123".into()),
            }],
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("ventana"),
            "el título muestra la ventana de tiempo"
        );
        assert!(text.contains("abc123"), "muestra el sufijo del stream");
    }

    #[test]
    fn enter_opens_line_detail() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        into_events(&mut v);
        let long = format!("INFO payload {}", "z".repeat(120));
        v.on_message(&events_loaded("/svc", "stream-a", vec![ev(&long, 1)]));
        v.on_key(key(KeyCode::Enter)); // abre el detalle del evento seleccionado

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("esc cierra"),
            "el panel de detalle muestra el hint"
        );
        assert!(
            text.contains("zzzz"),
            "muestra el contenido completo (wrapped) que la lista truncaría"
        );
    }

    #[test]
    fn esc_closes_line_detail() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        into_events(&mut v);
        v.on_message(&events_loaded("/svc", "stream-a", vec![ev("INFO hola", 1)]));
        v.on_key(key(KeyCode::Enter)); // abre
        v.on_key(key(KeyCode::Esc)); // cierra

        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("eventos"),
            "vuelve a la lista (título eventos)"
        );
        assert!(!text.contains("esc cierra"), "el panel de detalle se cerró");
    }

    #[test]
    fn detail_pretty_prints_json() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = LogsView::new();
        into_events(&mut v);
        v.on_message(&events_loaded(
            "/svc",
            "stream-a",
            vec![ev(r#"{"orderId":"A-1","ok":true}"#, 1)],
        ));
        v.on_key(key(KeyCode::Enter));

        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("orderId"), "muestra la clave del JSON pretty");
        assert!(text.contains("A-1"), "muestra el valor");
    }

    #[test]
    fn w_cycles_window_and_bumps_gen() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // tail 1h, gen=1
        let actions = v.on_key(key(KeyCode::Char('w'))); // 1h → 6h, gen=2
        match actions.as_slice() {
            [
                Action::LoadLogTail {
                    window,
                    token,
                    generation,
                    ..
                },
            ] => {
                assert_eq!(*window, LogWindow::Last(WINDOW_PRESETS[2].1), "1h → 6h");
                assert!(token.is_none());
                assert_eq!(*generation, 2, "consulta fresca sube el gen");
            }
            other => panic!("se esperaba LoadLogTail, llegó {other:?}"),
        }
    }

    #[test]
    fn o_loads_more_with_token_same_gen() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // gen=1
        // Una página con next_token → la vista guarda el token para `o`.
        v.on_message(&Message::LogTailLoaded {
            group: "/svc".into(),
            events: vec![ev("a", 1)],
            next_token: Some("tok".into()),
            append: false,
            generation: 1,
        });
        let actions = v.on_key(key(KeyCode::Char('o'))); // load-more
        match actions.as_slice() {
            [
                Action::LoadLogTail {
                    token, generation, ..
                },
            ] => {
                assert_eq!(token.as_deref(), Some("tok"));
                assert_eq!(*generation, 1, "load-more conserva el gen");
            }
            other => panic!("se esperaba LoadLogTail con token, llegó {other:?}"),
        }
    }

    #[test]
    fn append_extends_buffer() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // gen=1
        v.on_message(&tail_loaded("/svc", 1, vec![ev("a", 1), ev("b", 2)]));
        assert_eq!(v.visible_len(), 2);
        // Continuación (append) con el mismo gen → extiende, no reemplaza.
        v.on_message(&Message::LogTailLoaded {
            group: "/svc".into(),
            events: vec![ev("c", 3), ev("d", 4)],
            next_token: None,
            append: true,
            generation: 1,
        });
        assert_eq!(v.visible_len(), 4, "append extiende el buffer");
    }

    #[test]
    fn y_copies_group_identifier() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            // `group()` no trae ARN → cae al nombre.
            [Action::CopyToClipboard { text }] => assert_eq!(text, "/svc"),
            other => panic!("se esperaba CopyToClipboard, llegó {other:?}"),
        }
    }

    #[test]
    fn y_in_events_copies_the_line() {
        let mut v = LogsView::new();
        into_events(&mut v);
        v.on_message(&events_loaded("/svc", "stream-a", vec![ev("ERROR boom", 1)]));
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert_eq!(text, "ERROR boom"),
            other => panic!("se esperaba copiar la línea, llegó {other:?}"),
        }
    }

    #[test]
    fn filtered_cache_matches_recompute() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // gen=1
        v.on_message(&tail_loaded(
            "/svc",
            1,
            vec![ev("INFO a", 1), ev("ERROR b", 2), ev("INFO c", 3)],
        ));
        // El cache (self.filtered) es idéntico al cálculo directo, con y sin filtro.
        assert_eq!(v.filtered, v.filtered_event_indices());
        assert_eq!(v.filtered.len(), 3);
        v.set_filter("error");
        assert_eq!(v.filtered, v.filtered_event_indices());
        assert_eq!(v.filtered.len(), 1, "filtro substring sobre el lowercase precomputado");
        v.set_filter("");
        assert_eq!(v.filtered.len(), 3, "limpiar el filtro restituye el cache");
    }

    #[test]
    fn append_extends_lowercase_cache_and_filter() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        v.on_key(key(KeyCode::Char('t'))); // gen=1
        v.on_message(&tail_loaded("/svc", 1, vec![ev("INFO ok", 1)]));
        v.set_filter("boom");
        assert_eq!(v.visible_len(), 0, "nada matchea aún");
        // Append con una línea que SÍ matchea el filtro activo: aparece (event_rows y
        // el cache se extienden en sync).
        v.on_message(&Message::LogTailLoaded {
            group: "/svc".into(),
            events: vec![ev("ERROR boom", 2)],
            next_token: None,
            append: true,
            generation: 1,
        });
        assert_eq!(v.event_rows.len(), 2, "event_rows crece con el append");
        assert_eq!(v.visible_len(), 1, "el append respeta el filtro vía el cache");
    }

    #[test]
    fn on_command_since_sets_window() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        // Desde groups, `:since 2d` abre el tail con ventana Last(2d).
        let actions = v.on_command("since 2d");
        let ok = actions.iter().any(|a| {
            matches!(a, Action::LoadLogTail { window, .. } if *window == LogWindow::Last(2 * 86_400_000))
        });
        assert!(ok, "since 2d → LoadLogTail Last(2d): {actions:?}");
        assert!(matches!(v.level, Level::Tail { .. }));
    }

    #[test]
    fn on_command_from_to_sets_range() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        let actions = v.on_command("from 2026-06-19 to 2026-06-20");
        let ok = actions.iter().any(|a| {
            matches!(
                a,
                Action::LoadLogTail {
                    window: LogWindow::Range { to: Some(_), .. },
                    ..
                }
            )
        });
        assert!(ok, "from..to → LoadLogTail Range con `to`: {actions:?}");
    }

    #[test]
    fn on_command_unknown_or_invalid_returns_empty() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        assert!(v.on_command("blah").is_empty(), "comando ajeno → vacío");
        assert!(
            v.on_command("since notaduration").is_empty(),
            "duración inválida → vacío"
        );
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
    fn groups_search_is_local_not_server_side() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        // En groups, `search` ya no consulta al server: el filtrado es 100% local.
        assert!(
            v.search("xy").is_empty(),
            "groups filtra local; no emite LoadLogGroups"
        );
    }

    #[test]
    fn local_fuzzy_finds_substring_without_prefix() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/aws/lambda/orders-service-staging-CreateOrderV3"),
            group("/aws/lambda/payments-worker"),
            group("/ecs/checkout"),
        ]));
        // El caso del usuario: teclear `CreateOrder` (subcadena, sin el prefijo largo).
        v.set_filter("CreateOrder");
        assert_eq!(v.visible_len(), 1, "encuentra el group por subcadena");
        let first = v.filtered_group_indices()[0];
        assert!(v.groups[first].name.ends_with("CreateOrderV3"));
        // Y case-insensitive: lo mismo en minúsculas.
        v.set_filter("createorder");
        assert_eq!(v.visible_len(), 1, "el fuzzy local es case-insensitive");
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
    fn async_reload_does_not_stomp_navigation() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![
            group("/svc-a"),
            group("/svc-b"),
            group("/svc-c"),
        ]));
        // El usuario navega y, mientras, llega una recarga async (p. ej. `r` refresh).
        v.on_key(key(KeyCode::Down));
        v.on_key(key(KeyCode::Down)); // en "/svc-c"
        assert_eq!(v.selected_group_name().as_deref(), Some("/svc-c"));

        // Llega la recarga (misma data): la selección NO salta al tope (restore por nombre).
        v.on_message(&loaded(vec![
            group("/svc-a"),
            group("/svc-b"),
            group("/svc-c"),
        ]));
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

    #[test]
    fn hints_announce_tail_then_window_by_level() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        // Groups y Streams anuncian `t` (abre el tail de TODOS los streams por tiempo).
        assert!(v.hints().iter().any(|(k, _)| *k == "t"), "groups anuncia t");
        v.on_key(key(KeyCode::Enter));
        v.on_message(&stream("s1"));
        assert!(v.hints().iter().any(|(k, _)| *k == "t"), "streams anuncia t");
        // Dentro del tail: anuncia ventana/paginación/rango y ya no `t`.
        v.on_key(key(KeyCode::Char('t')));
        let keys: Vec<&str> = v.hints().iter().map(|(k, _)| *k).collect();
        assert!(
            keys.contains(&"w") && keys.contains(&"o") && keys.contains(&":since"),
            "el tail anuncia w/o/:since: {keys:?}"
        );
        assert!(!keys.contains(&"t"), "en el tail ya no se ofrece t");
    }

    #[test]
    fn hints_in_line_detail_offer_close() {
        let mut v = LogsView::new();
        into_events(&mut v);
        v.on_message(&events_loaded("/svc", "stream-a", vec![ev("hola", 1)]));
        v.on_key(key(KeyCode::Enter)); // abre el detalle de la línea
        assert!(
            v.hints().iter().any(|(k, _)| *k == "esc"),
            "el detalle ofrece cerrar"
        );
    }

    #[test]
    fn body_title_announces_tail_only_without_filter() {
        let mut v = LogsView::new();
        v.on_message(&loaded(vec![group("/svc")]));
        assert!(
            v.body_title().contains("t:"),
            "groups anuncia el tail en el título: {}",
            v.body_title()
        );
        // Con filtro aplicado, la rama con-filtro no arrastra el cue.
        v.set_filter("svc");
        assert!(
            !v.body_title().contains("t:"),
            "con filtro no se muestra el cue: {}",
            v.body_title()
        );
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
