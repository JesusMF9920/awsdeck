//! Vista `sqs`: colas de SQS -> drill al detalle (attributes + peek de mensajes).
//! Pura y síncrona, espeja a `logs`: estado, drill/back, filtro, navegación y
//! render. NUNCA importa `aws-sdk-*`; recibe DTOs planos vía `on_message`.
//!
//! `p` en el detalle emite la intención `PurgeQueue`; la vista NO sabe de modo
//! escritura ni confirm: ese gate vive en el `App`.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::View;
use crate::action::{Action, ConsoleTarget, ViewContext};
use crate::message::{Message, QueueAttrsDto, QueueDto, QueueMessageDto};
use crate::ui::detail::DetailPanel;
use crate::util::{fmt_epoch_millis, fuzzy_score, ranked};

/// Nivel de drill actual.
enum Level {
    Queues,
    Detail { queue_url: String },
}

pub struct SqsView {
    level: Level,
    queues: Vec<QueueDto>,
    attrs: Option<QueueAttrsDto>,
    messages: Vec<QueueMessageDto>,
    /// Panel con el cuerpo completo de un mensaje (`enter` en el detalle): la fila lo
    /// colapsa a 60 chars; el panel lo muestra entero, scrolleable/copiable. `None` = normal.
    detail_panel: Option<DetailPanel>,
    filter: String,
    loading: bool,
    state: ListState,
}

impl SqsView {
    pub fn new() -> Self {
        Self {
            level: Level::Queues,
            queues: Vec::new(),
            attrs: None,
            messages: Vec::new(),
            detail_panel: None,
            filter: String::new(),
            loading: false,
            state: ListState::default().with_selected(Some(0)),
        }
    }

    // --- Filtrado / selección -------------------------------------------------

    fn filtered_queue_indices(&self) -> Vec<usize> {
        ranked(self.queues.len(), &self.filter, |i| {
            fuzzy_score(&self.queues[i].name, &self.filter)
        })
    }

    fn filtered_message_indices(&self) -> Vec<usize> {
        // Un mensaje matchea por id o por body; tomamos el mejor score de los dos.
        ranked(self.messages.len(), &self.filter, |i| {
            let m = &self.messages[i];
            fuzzy_score(&m.id, &self.filter)
                .into_iter()
                .chain(fuzzy_score(&m.body, &self.filter))
                .max()
        })
    }

    fn visible_len(&self) -> usize {
        match self.level {
            Level::Queues => self.filtered_queue_indices().len(),
            Level::Detail { .. } => self.filtered_message_indices().len(),
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

    fn selected_queue(&self) -> Option<QueueDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_queue_indices().get(sel)?;
        Some(self.queues[idx].clone())
    }

    fn selected_message(&self) -> Option<QueueMessageDto> {
        let sel = self.state.selected()?;
        let idx = *self.filtered_message_indices().get(sel)?;
        self.messages.get(idx).cloned()
    }

    /// Abre el panel con el cuerpo completo del mensaje seleccionado (la fila lo colapsa a
    /// 60 chars). `enter` sobre un mensaje en el detalle.
    fn open_message_detail(&mut self) {
        if let Some(m) = self.selected_message() {
            let id: String = m.id.chars().take(8).collect();
            self.detail_panel = Some(DetailPanel::new(format!("msg {id}"), m.body));
        }
    }

    /// Texto a copiar con `y`: la URL de la cola (en la lista) o su ARN (en el detalle,
    /// si lo conocemos por los attributes; si no, la URL).
    fn copy_text(&self) -> Option<String> {
        match &self.level {
            Level::Queues => self.selected_queue().map(|q| q.url),
            Level::Detail { queue_url } => self
                .attrs
                .as_ref()
                .and_then(|a| a.arn.clone())
                .or_else(|| Some(queue_url.clone())),
        }
    }

    /// URL de la cola del nivel actual (seleccionada o la del detalle), para abrir en
    /// la consola.
    fn queue_url(&self) -> Option<String> {
        match &self.level {
            Level::Queues => self.selected_queue().map(|q| q.url),
            Level::Detail { queue_url } => Some(queue_url.clone()),
        }
    }

    // --- Navegación -----------------------------------------------------------

    fn drill(&mut self) -> Vec<Action> {
        match self.level {
            Level::Queues => match self.selected_queue() {
                Some(q) => {
                    let url = q.url.clone();
                    let label = name_from_url(&url).to_string();
                    self.level = Level::Detail {
                        queue_url: url.clone(),
                    };
                    self.attrs = None;
                    self.messages.clear();
                    self.loading = true;
                    self.state.select(Some(0));
                    vec![
                        Action::ClearFilter,
                        Action::RecordRecent {
                            key: url.clone(),
                            label,
                        },
                        Action::LoadQueueDetail { queue_url: url },
                    ]
                }
                None => vec![],
            },
            Level::Detail { .. } => vec![],
        }
    }

    /// `esc`: despoja un nivel de drill. En la raíz (queues) no hay nada que
    /// despojar → emite `Back` para que el `App` vuelva al menú.
    fn back(&mut self) -> Vec<Action> {
        self.detail_panel = None; // cerrar cualquier panel al subir de nivel
        if matches!(self.level, Level::Detail { .. }) {
            self.level = Level::Queues;
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
            Level::Queues => vec![Action::LoadQueues],
            Level::Detail { queue_url } => vec![Action::LoadQueueDetail {
                queue_url: queue_url.clone(),
            }],
        }
    }

    /// `p` en el detalle: emite la intención de purgar (el App la gatea).
    fn purge_intent(&self) -> Vec<Action> {
        match &self.level {
            Level::Detail { queue_url } => vec![Action::PurgeQueue {
                queue_url: queue_url.clone(),
            }],
            Level::Queues => vec![],
        }
    }

    /// `d` en el detalle: emite la intención de redrive SOLO si la cola es un DLQ
    /// (`is_dlq`). El App la gatea (modo escritura + confirm).
    fn redrive_intent(&self) -> Vec<Action> {
        match &self.level {
            Level::Detail { queue_url } if self.attrs.as_ref().is_some_and(|a| a.is_dlq()) => {
                vec![Action::RedriveDlq {
                    queue_url: queue_url.clone(),
                }]
            }
            _ => vec![],
        }
    }

    // --- Render ---------------------------------------------------------------

    fn queues_title(&self) -> String {
        let total = self.queues.len();
        let shown = self.filtered_queue_indices().len();
        if self.filter.is_empty() {
            format!(" {total} colas ")
        } else {
            format!(" {shown}/{total} colas · filtro: {} ", self.filter)
        }
    }

    fn messages_title(&self) -> String {
        let total = self.messages.len();
        let shown = self.filtered_message_indices().len();
        if self.filter.is_empty() {
            format!(" peek · {total} msgs · best-effort (receive_count++) ")
        } else {
            format!(" peek · {shown}/{total} · filtro: {} ", self.filter)
        }
    }

    fn attrs_paragraph(&self) -> Paragraph<'static> {
        let block = Block::bordered().title(" attributes ");
        let Some(a) = &self.attrs else {
            let msg = if self.loading { "cargando…" } else { "—" };
            return Paragraph::new(msg).block(block);
        };
        let opt = |o: Option<i64>| o.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string());
        let dlq = if a.has_dlq() {
            format!(
                "{} (maxReceiveCount {})",
                a.dlq_target_arn.clone().unwrap_or_default(),
                opt(a.max_receive_count)
            )
        } else {
            "—".to_string()
        };
        let row = |k: &'static str, v: String| {
            Line::from(vec![
                Span::styled(format!("{k:<11}"), Style::new().dark_gray()),
                Span::raw(v),
            ])
        };
        let mut lines = vec![
            row("visible", opt(a.visible)),
            row("in-flight", opt(a.in_flight)),
            row("delayed", opt(a.delayed)),
            row("DLQ", dlq),
            row("ARN", a.arn.clone().unwrap_or_else(|| "—".to_string())),
        ];
        // Si la cola ES un DLQ (tiene colas origen), anunciarlo: habilita `d` redrive.
        if a.is_dlq() {
            lines.push(row(
                "DLQ de",
                format!("{} colas origen · [d] redrive", a.dlq_sources.len()),
            ));
        }
        Paragraph::new(lines).block(block)
    }
}

impl Default for SqsView {
    fn default() -> Self {
        Self::new()
    }
}

impl View for SqsView {
    fn id(&self) -> &'static str {
        "sqs"
    }

    fn description(&self) -> &'static str {
        "Colas SQS: attributes, peek, purge"
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        // Con el panel del cuerpo abierto, sus teclas (scroll/copiar/cerrar) mandan.
        if let Some(p) = &self.detail_panel {
            return p.hints();
        }
        match self.level {
            // En el detalle de una cola, `p` purga y `d` redrivea el DLQ (ambas gated).
            Level::Detail { .. } => {
                let mut hints = vec![
                    ("enter", "ver cuerpo"),
                    ("y", "copiar ARN"),
                    ("O", "consola"),
                    ("p", "purgar"),
                ];
                // `d` solo si la cola ES un DLQ (tiene colas origen).
                if self.attrs.as_ref().is_some_and(|a| a.is_dlq()) {
                    hints.push(("d", "redrive DLQ"));
                }
                hints
            }
            Level::Queues => vec![("y", "copiar URL"), ("O", "consola")],
        }
    }

    fn title(&self) -> String {
        match &self.level {
            Level::Queues => "sqs".to_string(),
            Level::Detail { queue_url } => format!("sqs / {}", name_from_url(queue_url)),
        }
    }

    fn on_activate(&mut self) -> Vec<Action> {
        self.level = Level::Queues;
        self.attrs = None;
        self.messages.clear();
        self.detail_panel = None;
        self.loading = true;
        self.state.select(Some(0));
        vec![Action::LoadQueues]
    }

    /// Abrir un favorito/reciente: la `key` es la URL de la cola → drillea directo a su
    /// detalle. Otros contextos (LogGroupTail) no le conciernen → activación normal.
    fn on_context(&mut self, context: &ViewContext) -> Vec<Action> {
        match context {
            ViewContext::Favorite { key } => {
                self.level = Level::Detail {
                    queue_url: key.clone(),
                };
                self.attrs = None;
                self.messages.clear();
                self.detail_panel = None;
                self.loading = true;
                self.state.select(Some(0));
                vec![Action::LoadQueueDetail {
                    queue_url: key.clone(),
                }]
            }
            _ => self.on_activate(),
        }
    }

    /// Favorito = la cola seleccionada (solo en el nivel de colas).
    fn selected_favorite(&self) -> Option<(String, String)> {
        match self.level {
            Level::Queues => self
                .selected_queue()
                .map(|q| (q.url.clone(), name_from_url(&q.url).to_string())),
            Level::Detail { .. } => None,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        // Panel del cuerpo abierto: `y` copia su contenido, el resto scrollea/cierra.
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
            // En el detalle, `enter` expande el cuerpo del mensaje (la lista navegable son
            // los mensajes peekeados); en queues, drillea.
            KeyCode::Enter if matches!(self.level, Level::Detail { .. }) => {
                self.open_message_detail();
                vec![]
            }
            KeyCode::Enter => self.drill(),
            KeyCode::Esc => self.back(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('p') => self.purge_intent(),
            KeyCode::Char('d') => self.redrive_intent(),
            KeyCode::Char('y') => self
                .copy_text()
                .map(|text| Action::CopyToClipboard { text })
                .into_iter()
                .collect(),
            KeyCode::Char('O') => self
                .queue_url()
                .map(|url| Action::OpenConsole {
                    target: ConsoleTarget::SqsQueue { url },
                })
                .into_iter()
                .collect(),
            _ => vec![],
        }
    }

    fn on_message(&mut self, message: &Message) {
        match message {
            Message::QueuesLoaded(queues) => {
                self.queues = queues.clone();
                if matches!(self.level, Level::Queues) {
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::QueueDetailLoaded {
                queue_url,
                attrs,
                messages,
            } => {
                // Aceptar solo si corresponde a la cola del drill actual.
                if let Level::Detail { queue_url: current } = &self.level
                    && current == queue_url
                {
                    self.attrs = Some(attrs.clone());
                    self.messages = messages.clone();
                    self.loading = false;
                    self.clamp_selection();
                }
            }
            Message::QueuePurged { queue_url } => {
                // El App muestra el info y re-dispara LoadQueueDetail; aquí
                // limpiamos los mensajes y marcamos recarga.
                if let Level::Detail { queue_url: current } = &self.level
                    && current == queue_url
                {
                    self.messages.clear();
                    self.loading = true;
                }
            }
            Message::DlqRedriveStarted { queue_url } => {
                // El App re-dispara LoadQueueDetail; aquí marcamos recarga.
                if let Level::Detail { queue_url: current } = &self.level
                    && current == queue_url
                {
                    self.loading = true;
                }
            }
            Message::Error { .. } => self.loading = false,
            // Mensajes de otras vistas (p. ej. logs): se ignoran.
            _ => {}
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.state.select(Some(0)); // top = mejor match (estilo fzf)
        self.clamp_selection();
    }

    fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Panel del cuerpo del mensaje: ocupa el cuerpo entero.
        if let Some(p) = self.detail_panel.as_mut() {
            p.render(frame, area);
            return;
        }
        if matches!(self.level, Level::Queues) {
            let block = Block::bordered().title(self.queues_title());
            let items: Vec<ListItem> = self
                .filtered_queue_indices()
                .into_iter()
                .map(|i| queue_item(&self.queues[i]))
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

        // Detalle: attributes arriba, peek de mensajes abajo. Una fila más si la cola
        // es un DLQ (muestra "DLQ de" + el hint de redrive).
        let attrs_h = if self.attrs.as_ref().is_some_and(|a| a.is_dlq()) {
            8
        } else {
            7
        };
        let [attrs_area, msgs_area] =
            Layout::vertical([Constraint::Length(attrs_h), Constraint::Min(0)]).areas(area);
        frame.render_widget(self.attrs_paragraph(), attrs_area);

        let block = Block::bordered().title(self.messages_title());
        let items: Vec<ListItem> = self
            .filtered_message_indices()
            .into_iter()
            .map(|i| message_item(&self.messages[i]))
            .collect();
        if items.is_empty() {
            let msg = if self.loading {
                "cargando…"
            } else if self.filter.is_empty() {
                "(cola sin mensajes visibles)"
            } else {
                "(sin coincidencias para el filtro)"
            };
            frame.render_widget(Paragraph::new(msg).block(block), msgs_area);
            return;
        }
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed())
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, msgs_area, &mut self.state);
    }
}

// --- Construcción de filas y helpers ------------------------------------------

fn name_from_url(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

fn empty_msg(loading: bool, filter: &str) -> &'static str {
    if loading {
        "cargando…"
    } else if filter.is_empty() {
        "(sin resultados)"
    } else {
        "(sin coincidencias para el filtro)"
    }
}

fn queue_item(q: &QueueDto) -> ListItem<'static> {
    let mut spans = vec![Span::raw(q.name.clone())];
    if q.is_fifo {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("[fifo]", Style::new().dark_gray()));
    }
    ListItem::new(Line::from(spans))
}

fn message_item(m: &QueueMessageDto) -> ListItem<'static> {
    let when = m
        .sent_ts
        .map(fmt_epoch_millis)
        .unwrap_or_else(|| "—".to_string());
    let recv = m
        .receive_count
        .map(|n| format!("recv {n}"))
        .unwrap_or_default();
    let body = m.body.replace('\n', " ");
    let body = if body.chars().count() > 60 {
        format!("{}…", body.chars().take(60).collect::<String>())
    } else {
        body
    };
    ListItem::new(Line::from(vec![
        Span::raw(body),
        Span::raw("  "),
        Span::styled(format!("{when}  {recv}"), Style::new().dark_gray()),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(name: &str) -> QueueDto {
        QueueDto {
            name: name.to_string(),
            url: format!("https://sqs.us-east-1.amazonaws.com/000000000000/{name}"),
            is_fifo: name.ends_with(".fifo"),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        use ratatui::crossterm::event::KeyModifiers;
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn activate_requests_queues() {
        let mut v = SqsView::new();
        assert!(matches!(v.on_activate().as_slice(), [Action::LoadQueues]));
    }

    #[test]
    fn ingests_queues_via_message() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![
            queue("orders"),
            queue("pay.fifo"),
        ]));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn filter_narrows_queue_list() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![
            queue("orders"),
            queue("orders-dlq"),
            queue("payments"),
        ]));
        v.set_filter("ORDERS"); // case-insensitive
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn enter_drills_into_queue_detail() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![
            queue("orders"),
            queue("payments"),
        ]));
        v.on_key(key(KeyCode::Down)); // selecciona payments
        let actions = v.on_key(key(KeyCode::Enter));
        match actions.as_slice() {
            [
                Action::ClearFilter,
                Action::RecordRecent { label, .. },
                Action::LoadQueueDetail { queue_url },
            ] => {
                assert!(queue_url.ends_with("/payments"));
                assert_eq!(label, "payments", "recuerda la cola por su nombre");
            }
            other => {
                panic!("se esperaba ClearFilter+RecordRecent+LoadQueueDetail, llegó {other:?}")
            }
        }
        assert!(matches!(v.level, Level::Detail { .. }));

        v.on_message(&Message::QueueDetailLoaded {
            queue_url: queue("payments").url,
            attrs: QueueAttrsDto {
                visible: Some(3),
                ..Default::default()
            },
            messages: vec![QueueMessageDto {
                id: "m1".into(),
                body: "hola".into(),
                sent_ts: None,
                receive_count: Some(1),
            }],
        });
        assert_eq!(v.visible_len(), 1);

        v.on_key(key(KeyCode::Esc));
        assert!(matches!(v.level, Level::Queues));
        assert_eq!(v.visible_len(), 2);
    }

    #[test]
    fn favorite_getter_and_open_via_context() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("payments")]));
        // En el nivel de colas el favorito es la cola seleccionada (url + nombre).
        let fav = v.selected_favorite().expect("hay una cola seleccionada");
        assert!(fav.0.ends_with("/payments"), "key = url");
        assert_eq!(fav.1, "payments", "label = nombre");

        // Abrir un favorito via contexto → drillea directo al detalle de esa URL.
        let url = queue("payments").url;
        match v
            .on_context(&ViewContext::Favorite { key: url.clone() })
            .as_slice()
        {
            [Action::LoadQueueDetail { queue_url }] => assert_eq!(*queue_url, url),
            other => panic!("se esperaba LoadQueueDetail, llegó {other:?}"),
        }
        assert!(matches!(v.level, Level::Detail { .. }));
    }

    #[test]
    fn enter_in_detail_expands_message_body_and_esc_closes() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        v.on_key(key(KeyCode::Enter)); // → Detail
        let long = format!("payload {}", "x".repeat(200));
        v.on_message(&Message::QueueDetailLoaded {
            queue_url: queue("orders").url,
            attrs: QueueAttrsDto::default(),
            messages: vec![QueueMessageDto {
                id: "abcdef123456".into(),
                body: long.clone(),
                sent_ts: None,
                receive_count: Some(2),
            }],
        });

        // `enter` expande el cuerpo completo (no drillea, no purga).
        let actions = v.on_key(key(KeyCode::Enter));
        assert!(actions.is_empty());
        assert_eq!(
            v.detail_panel.as_ref().expect("panel").content(),
            long,
            "muestra el cuerpo completo, no los 60 chars de la fila"
        );
        // `y` copia el cuerpo completo.
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert_eq!(*text, long),
            other => panic!("se esperaba copiar el cuerpo, llegó {other:?}"),
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
    fn esc_at_root_emits_back() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        // En la raíz (queues) no hay drill que despojar: esc pide volver al menú.
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
        assert!(
            matches!(v.level, Level::Queues),
            "esc en raíz no cambia nivel"
        );
    }

    #[test]
    fn esc_in_detail_pops_to_queues_without_back() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        v.on_key(key(KeyCode::Enter)); // drill al detalle
        assert!(matches!(v.level, Level::Detail { .. }));
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(
            matches!(actions.as_slice(), [Action::ClearFilter]),
            "esc en el detalle se consume en la vista (limpia el filtro al subir)"
        );
        assert!(matches!(v.level, Level::Queues));
    }

    #[test]
    fn esc_at_root_empty_list_emits_back() {
        // El caso más común del bug original: vista recién activada (sin data) + esc.
        let mut v = SqsView::new();
        let actions = v.on_key(key(KeyCode::Esc));
        assert!(matches!(actions.as_slice(), [Action::Back]));
    }

    #[test]
    fn detail_from_wrong_queue_is_ignored() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        v.on_key(key(KeyCode::Enter)); // drill orders
        v.on_message(&Message::QueueDetailLoaded {
            queue_url: queue("otra").url, // cola equivocada
            attrs: QueueAttrsDto::default(),
            messages: vec![QueueMessageDto {
                id: "x".into(),
                body: "x".into(),
                sent_ts: None,
                receive_count: None,
            }],
        });
        assert_eq!(v.visible_len(), 0, "no se acepta detalle de otra cola");
    }

    #[test]
    fn y_copies_queue_url() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        match v.on_key(key(KeyCode::Char('y'))).as_slice() {
            [Action::CopyToClipboard { text }] => assert!(text.ends_with("/orders")),
            other => panic!("se esperaba CopyToClipboard, llegó {other:?}"),
        }
    }

    #[test]
    fn o_opens_queue_in_console() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        match v.on_key(key(KeyCode::Char('O'))).as_slice() {
            [
                Action::OpenConsole {
                    target: ConsoleTarget::SqsQueue { url },
                },
            ] => assert!(url.ends_with("/orders")),
            other => panic!("se esperaba OpenConsole SqsQueue, llegó {other:?}"),
        }
    }

    #[test]
    fn hints_offer_purge_only_in_detail() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        assert!(
            !v.hints().iter().any(|(k, _)| *k == "p"),
            "en la lista no se ofrece la acción gated `p`"
        );
        v.on_key(key(KeyCode::Enter)); // → Detail
        assert!(
            v.hints().iter().any(|(k, _)| *k == "p"),
            "el detalle de la cola anuncia purgar"
        );
    }

    #[test]
    fn purge_key_emits_purge_action() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        v.on_key(key(KeyCode::Enter)); // drill
        let actions = v.on_key(key(KeyCode::Char('p')));
        match actions.as_slice() {
            [Action::PurgeQueue { queue_url }] => assert!(queue_url.ends_with("/orders")),
            other => panic!("se esperaba PurgeQueue, llegó {other:?}"),
        }
    }

    fn dlq_attrs() -> QueueAttrsDto {
        QueueAttrsDto {
            arn: Some("arn:aws:sqs:us-east-1:000:orders-dlq".into()),
            dlq_sources: vec!["https://sqs.us-east-1.amazonaws.com/000/orders".into()],
            ..Default::default()
        }
    }

    #[test]
    fn redrive_offered_and_emitted_only_for_dlq() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders-dlq")]));
        v.on_key(key(KeyCode::Enter)); // → Detail

        // Cola sin colas origen: NO es un DLQ → no se ofrece `d` ni emite.
        v.on_message(&Message::QueueDetailLoaded {
            queue_url: queue("orders-dlq").url,
            attrs: QueueAttrsDto::default(),
            messages: vec![],
        });
        assert!(!v.hints().iter().any(|(k, _)| *k == "d"));
        assert!(v.on_key(key(KeyCode::Char('d'))).is_empty());

        // Ahora ES un DLQ (tiene colas origen) → se ofrece y emite RedriveDlq.
        v.on_message(&Message::QueueDetailLoaded {
            queue_url: queue("orders-dlq").url,
            attrs: dlq_attrs(),
            messages: vec![],
        });
        assert!(v.hints().iter().any(|(k, _)| *k == "d"));
        match v.on_key(key(KeyCode::Char('d'))).as_slice() {
            [Action::RedriveDlq { queue_url }] => assert!(queue_url.ends_with("/orders-dlq")),
            other => panic!("se esperaba RedriveDlq, llegó {other:?}"),
        }
    }

    #[test]
    fn redrive_not_offered_outside_detail() {
        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders-dlq")]));
        // En la lista (queues) no se ofrece la acción gated `d` ni se emite.
        assert!(!v.hints().iter().any(|(k, _)| *k == "d"));
        assert!(v.on_key(key(KeyCode::Char('d'))).is_empty());
    }

    #[test]
    fn render_detail_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut v = SqsView::new();
        v.on_message(&Message::QueuesLoaded(vec![queue("orders")]));
        v.on_key(key(KeyCode::Enter));
        v.on_message(&Message::QueueDetailLoaded {
            queue_url: queue("orders").url,
            attrs: QueueAttrsDto {
                visible: Some(2),
                in_flight: Some(1),
                ..Default::default()
            },
            messages: vec![QueueMessageDto {
                id: "m1".into(),
                body: "{\"event\":\"x\"}".into(),
                sent_ts: Some(1_700_000_000_000),
                receive_count: Some(1),
            }],
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|f| v.render(f, f.area())).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("attributes"));
        assert!(text.contains("peek"));
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
