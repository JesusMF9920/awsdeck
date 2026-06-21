//! `App` — estado global (Env activo + epoch), modos de input, vista activa,
//! routing de teclas, status bar y el loop `tokio::select!` (teclado + canal de
//! mensajes). **Agnóstico de servicio:** solo conoce vistas por el registry y
//! reenvía las `Action` de efecto a `effects` sin inspeccionarlas.

use color_eyre::eyre::Result;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, ListState, Paragraph};
use tokio::sync::mpsc;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::action::Action;
use crate::aws::context::{Env, ProfileEntry, list_profiles};
use crate::effects::Effects;
use crate::message::{Envelope, Message};
use crate::ui::{command_bar, header, help, picker};
use crate::views::Registry;

/// Modo de input del `App`: dónde van las teclas.
enum Mode {
    Normal,
    Command,
    Filter,
}

/// Línea de estado transitoria (errores e info), mostrada en el footer.
struct StatusLine {
    error: bool,
    text: String,
}

/// Estado del picker de ambientes (overlay de `ctrl-e`).
struct Picker {
    profiles: Vec<ProfileEntry>,
    state: ListState,
}

impl Picker {
    fn new(profiles: Vec<ProfileEntry>) -> Self {
        let mut state = ListState::default();
        if !profiles.is_empty() {
            state.select(Some(0));
        }
        Self { profiles, state }
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.profiles.len();
        if len == 0 {
            return;
        }
        let cur = self.state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, len as i32 - 1) as usize;
        self.state.select(Some(next));
    }

    /// Preselecciona el profile con este nombre; si no existe, deja la selección.
    fn preselect(&mut self, name: &str) {
        if let Some(idx) = self.profiles.iter().position(|p| p.name == name) {
            self.state.select(Some(idx));
        }
    }

    /// `Env` del profile seleccionado: usa su región declarada o, si no tiene,
    /// conserva la región actual.
    fn selected_env(&self, current: &Env) -> Option<Env> {
        let profile = self.profiles.get(self.state.selected()?)?;
        let region = profile
            .region
            .clone()
            .unwrap_or_else(|| current.region.clone());
        Some(Env::new(profile.name.clone(), region))
    }
}

pub struct App {
    env: Env,
    epoch: u64,
    registry: Registry,
    effects: Effects,
    rx: mpsc::Receiver<Envelope>,

    mode: Mode,
    /// Buffer de edición compartido por los modos `:` y `/`.
    input: Input,
    /// Filtro aplicado a la vista activa (espejo, para mostrarlo en modo normal).
    filter: String,
    status: Option<StatusLine>,
    show_help: bool,
    picker: Option<Picker>,
    /// `true` mientras el picker de arranque espera que el usuario elija ambiente;
    /// difiere la carga inicial para no pintar datos del default.
    awaiting_startup_env: bool,
    should_quit: bool,
}

impl App {
    pub fn new(
        env: Env,
        registry: Registry,
        effects: Effects,
        rx: mpsc::Receiver<Envelope>,
    ) -> Self {
        Self {
            env,
            epoch: 0,
            registry,
            effects,
            rx,
            mode: Mode::Normal,
            input: Input::default(),
            filter: String::new(),
            status: None,
            show_help: false,
            picker: None,
            awaiting_startup_env: false,
            should_quit: false,
        }
    }

    /// Corre el loop principal hasta que el usuario sale.
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        // Con el picker de arranque abierto, esperar a que el usuario elija antes
        // de cargar (no pintar datos del ambiente por defecto).
        if !self.awaiting_startup_env {
            self.activate_initial();
        }

        let mut events = EventStream::new();
        while !self.should_quit {
            terminal.draw(|frame| self.render(frame))?;

            tokio::select! {
                maybe_event = events.next() => match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        self.on_key(key);
                    }
                    Some(Ok(_)) => {}                       // resize / mouse / etc.
                    Some(Err(e)) => self.set_error(format!("input: {e}")),
                    None => break,                          // stream cerrado
                },
                Some(envelope) = self.rx.recv() => self.on_envelope(envelope),
            }
        }
        Ok(())
    }

    // --- Activación / routing -------------------------------------------------

    /// Activa la primera vista registrada (si hay) y dispara su carga inicial.
    fn activate_initial(&mut self) {
        if !self.registry.is_empty() {
            let actions = self.on_activate_active();
            self.dispatch_all(actions);
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Cualquier tecla limpia el estado transitorio.
        self.status = None;
        // Los overlays interceptan primero.
        if self.picker.is_some() {
            self.on_picker_key(key);
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        match self.mode {
            Mode::Normal => self.on_normal_key(key),
            Mode::Command => self.on_command_key(key),
            Mode::Filter => self.on_filter_key(key),
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.dispatch(Action::Quit),
            KeyCode::Char(':') => self.enter_command_mode(),
            KeyCode::Char('/') => self.enter_filter_mode(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('e') if ctrl => self.open_picker(),
            // Resto: lo maneja la vista activa.
            _ => {
                let actions = match self.registry.active_mut() {
                    Some(view) => view.on_key(key),
                    None => Vec::new(),
                };
                self.dispatch_all(actions);
            }
        }
    }

    fn on_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.leave_input_mode(),
            KeyCode::Enter => {
                let cmd = self.input.value().trim().to_string();
                self.leave_input_mode();
                self.run_command(&cmd);
            }
            _ => {
                self.input.handle_event(&Event::Key(key));
            }
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            // esc limpia el filtro y vuelve a normal.
            KeyCode::Esc => {
                self.input.reset();
                self.apply_filter();
                self.mode = Mode::Normal;
            }
            // enter confirma: mantiene el filtro aplicado, vuelve a normal.
            KeyCode::Enter => self.mode = Mode::Normal,
            _ => {
                self.input.handle_event(&Event::Key(key));
                self.apply_filter();
            }
        }
    }

    /// Despacha una `Action`: las core las maneja el `App`; el resto las reenvía a
    /// `effects` con el epoch actual (sin nombrar servicios concretos).
    fn dispatch(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::ActivateView(id) => self.activate_view(&id),
            Action::SwitchEnv(env) => self.switch_env(env),
            effect => self.effects.dispatch(effect, self.epoch),
        }
    }

    fn dispatch_all(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.dispatch(action);
        }
    }

    fn activate_view(&mut self, id: &str) {
        if self.registry.activate(id) {
            self.clear_filter();
            let actions = self.on_activate_active();
            self.dispatch_all(actions);
        } else {
            let available = self.registry.ids().join(", ");
            self.set_error(format!(
                "comando desconocido: {id} (disponibles: {available})"
            ));
        }
    }

    /// Cambia de ambiente: sube el epoch (invalida respuestas en vuelo),
    /// reconstruye la fuente de datos y recarga la vista activa.
    fn switch_env(&mut self, env: Env) {
        self.epoch += 1;
        self.env = env.clone();
        self.effects.set_env(env);
        self.clear_filter();
        let actions = self.on_activate_active();
        self.dispatch_all(actions);
        self.set_info(format!("ambiente: {}", self.env));
    }

    // --- Picker de ambientes (ctrl-e) -----------------------------------------

    fn open_picker(&mut self) {
        let profiles = list_profiles();
        if profiles.is_empty() {
            self.set_error("no se encontraron profiles en ~/.aws/config");
        } else {
            self.picker = Some(Picker::new(profiles));
        }
    }

    /// Abre el picker al arrancar (si hay profiles), preseleccionando el ambiente
    /// actual, y difiere la carga inicial hasta que el usuario elija (enter) o
    /// cancele (esc). Sin profiles no hace nada: `run` cargará con el default.
    pub fn start_with_env_picker(&mut self) {
        let profiles = list_profiles();
        if profiles.is_empty() {
            return;
        }
        let mut picker = Picker::new(profiles);
        picker.preselect(&self.env.profile);
        self.picker = Some(picker);
        self.awaiting_startup_env = true;
    }

    fn on_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                // En el picker de arranque, cancelar = usar el ambiente por defecto.
                if self.awaiting_startup_env {
                    self.awaiting_startup_env = false;
                    self.activate_initial();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(p) = self.picker.as_mut() {
                    p.move_selection(1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(p) = self.picker.as_mut() {
                    p.move_selection(-1);
                }
            }
            KeyCode::Enter => {
                // Calcular el Env antes de mutar `self.picker` (evita borrow doble).
                let env = self.picker.as_ref().and_then(|p| p.selected_env(&self.env));
                if let Some(env) = env {
                    self.picker = None;
                    self.awaiting_startup_env = false;
                    self.dispatch(Action::SwitchEnv(env));
                }
            }
            _ => {}
        }
    }

    fn on_envelope(&mut self, envelope: Envelope) {
        // EPOCH GUARD: descartar respuestas del ambiente anterior.
        if envelope.epoch != self.epoch {
            return;
        }
        if let Message::Error(e) = &envelope.message {
            self.set_error(e.clone());
        }
        if let Some(view) = self.registry.active_mut() {
            view.on_message(&envelope.message);
        }
    }

    // --- Helpers de modo / filtro / estado ------------------------------------

    fn on_activate_active(&mut self) -> Vec<Action> {
        match self.registry.active_mut() {
            Some(view) => view.on_activate(),
            None => Vec::new(),
        }
    }

    fn run_command(&mut self, cmd: &str) {
        match cmd {
            "" => {}
            "q" | "quit" => self.dispatch(Action::Quit),
            id => self.dispatch(Action::ActivateView(id.to_string())),
        }
    }

    fn enter_command_mode(&mut self) {
        self.mode = Mode::Command;
        self.input.reset();
    }

    fn enter_filter_mode(&mut self) {
        self.mode = Mode::Filter;
        // Arrancar el editor desde el filtro ya aplicado.
        self.input = Input::new(self.filter.clone());
    }

    fn leave_input_mode(&mut self) {
        self.input.reset();
        self.mode = Mode::Normal;
    }

    fn apply_filter(&mut self) {
        self.filter = self.input.value().to_string();
        if let Some(view) = self.registry.active_mut() {
            view.set_filter(&self.filter);
        }
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.input.reset();
        if let Some(view) = self.registry.active_mut() {
            view.set_filter("");
        }
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusLine {
            error: true,
            text: text.into(),
        });
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusLine {
            error: false,
            text: text.into(),
        });
    }

    // --- Render ---------------------------------------------------------------

    fn render(&mut self, frame: &mut Frame) {
        let full = frame.area();
        let [header_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(full);

        let title = self
            .registry
            .active()
            .map(|v| v.title())
            .unwrap_or_else(|| "—".to_string());
        header::render(frame, header_area, &self.env, &title);

        match self.registry.active_mut() {
            Some(view) => view.render(frame, body_area),
            None => render_placeholder(frame, body_area),
        }

        command_bar::render(frame, footer_area, self.footer_state());

        // Overlays: el picker tiene precedencia sobre la ayuda.
        if let Some(p) = &mut self.picker {
            picker::render(frame, full, &p.profiles, &mut p.state);
        } else if self.show_help {
            help::render(frame, full);
        }
    }

    fn footer_state(&self) -> command_bar::Footer<'_> {
        match self.mode {
            Mode::Command => command_bar::Footer::Input {
                prefix: ':',
                value: self.input.value(),
                cursor: self.input.visual_cursor(),
            },
            Mode::Filter => command_bar::Footer::Input {
                prefix: '/',
                value: self.input.value(),
                cursor: self.input.visual_cursor(),
            },
            Mode::Normal => match &self.status {
                Some(status) => command_bar::Footer::Status {
                    error: status.error,
                    text: &status.text,
                },
                None => command_bar::Footer::Hints {
                    filter: &self.filter,
                },
            },
        }
    }
}

/// Cuerpo cuando no hay vista activa (registry vacío): guía al usuario a `:logs`.
fn render_placeholder(frame: &mut Frame, area: Rect) {
    let body = Paragraph::new(vec![
        Line::from("Sin vista activa.".bold()),
        Line::from(""),
        Line::from(vec![
            Span::raw("Escribe "),
            Span::styled(":logs", Style::new().yellow().bold()),
            Span::raw(" para abrir CloudWatch Logs."),
        ]),
        Line::from(vec![
            Span::raw("Pulsa "),
            Span::styled("?", Style::new().yellow().bold()),
            Span::raw(" para ayuda, "),
            Span::styled("q", Style::new().yellow().bold()),
            Span::raw(" para salir."),
        ]),
    ])
    .alignment(Alignment::Center)
    .block(Block::bordered());
    frame.render_widget(body, area);
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use crate::message::Message;
    use crate::views::View;

    fn test_app() -> App {
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("dev", "us-east-1");
        let effects = Effects::new(tx, env.clone());
        App::new(env, Registry::new(), effects, rx)
    }

    /// Vista falsa que cuenta cuántas veces se activó (carga). Devuelve `vec![]`
    /// para no disparar efectos (que necesitarían un runtime de tokio).
    struct CountingView {
        activations: Rc<Cell<u32>>,
    }

    impl View for CountingView {
        fn id(&self) -> &'static str {
            "logs"
        }
        fn title(&self) -> String {
            "logs".to_string()
        }
        fn on_activate(&mut self) -> Vec<Action> {
            self.activations.set(self.activations.get() + 1);
            Vec::new()
        }
        fn on_key(&mut self, _key: KeyEvent) -> Vec<Action> {
            Vec::new()
        }
        fn on_message(&mut self, _message: &Message) {}
        fn set_filter(&mut self, _filter: &str) {}
        fn render(&mut self, _frame: &mut Frame, _area: Rect) {}
    }

    /// `App` con una `CountingView` registrada; devuelve también el contador de
    /// activaciones para verificar que la carga se disparó.
    fn app_with_counting_view() -> (App, Rc<Cell<u32>>) {
        let counter = Rc::new(Cell::new(0));
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("default", "us-east-1");
        let effects = Effects::new_mock(tx, env.clone());
        let mut registry = Registry::new();
        registry.register(Box::new(CountingView {
            activations: counter.clone(),
        }));
        (App::new(env, registry, effects, rx), counter)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_command(app: &mut App, cmd: &str) {
        app.on_key(ch(':'));
        for c in cmd.chars() {
            app.on_key(ch(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn q_quits() {
        let mut app = test_app();
        app.on_key(ch('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn command_quit_quits() {
        let mut app = test_app();
        type_command(&mut app, "q");
        assert!(app.should_quit);
    }

    #[test]
    fn unknown_command_sets_error() {
        let mut app = test_app();
        type_command(&mut app, "nope");
        let status = app.status.as_ref().expect("debe haber status");
        assert!(status.error);
        assert!(status.text.contains("desconocido"));
    }

    fn profile(name: &str, region: Option<&str>) -> ProfileEntry {
        ProfileEntry {
            name: name.to_string(),
            region: region.map(str::to_string),
        }
    }

    #[test]
    fn picker_selected_env_uses_region_or_falls_back() {
        let mut p = Picker::new(vec![
            profile("prod", Some("eu-west-1")),
            profile("dev", None),
        ]);
        let current = Env::new("default", "us-east-1");
        assert_eq!(
            p.selected_env(&current),
            Some(Env::new("prod", "eu-west-1"))
        );
        p.move_selection(1);
        assert_eq!(p.selected_env(&current), Some(Env::new("dev", "us-east-1")));
    }

    #[test]
    fn picker_enter_switches_env_and_bumps_epoch() {
        let mut app = test_app();
        app.picker = Some(Picker::new(vec![profile("prod", Some("eu-west-1"))]));
        assert_eq!(app.epoch, 0);

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.picker.is_none(), "el picker se cierra al elegir");
        assert_eq!(app.epoch, 1, "el switch sube el epoch");
        assert_eq!(app.env, Env::new("prod", "eu-west-1"));
    }

    #[test]
    fn picker_esc_closes_without_switching() {
        let mut app = test_app();
        app.picker = Some(Picker::new(vec![profile("prod", None)]));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.picker.is_none());
        assert_eq!(app.epoch, 0, "esc no cambia de ambiente");
    }

    #[test]
    fn preselect_picks_matching_profile_else_keeps_first() {
        let cur = Env::new("x", "us-east-1");
        let mut p = Picker::new(vec![
            profile("dev", None),
            profile("prod", Some("eu-west-1")),
            profile("stage", None),
        ]);
        p.preselect("prod");
        assert_eq!(p.selected_env(&cur), Some(Env::new("prod", "eu-west-1")));
        // Nombre inexistente: no cambia la selección.
        p.preselect("nope");
        assert_eq!(p.selected_env(&cur), Some(Env::new("prod", "eu-west-1")));
    }

    #[test]
    fn startup_picker_esc_loads_default() {
        let (mut app, activations) = app_with_counting_view();
        app.picker = Some(Picker::new(vec![profile("dev", None)]));
        app.awaiting_startup_env = true;

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.picker.is_none());
        assert!(!app.awaiting_startup_env);
        assert_eq!(app.epoch, 0, "esc no cambia de ambiente");
        assert_eq!(
            activations.get(),
            1,
            "esc en el picker de arranque carga el ambiente por defecto"
        );
    }

    #[test]
    fn startup_picker_enter_switches_and_loads() {
        let (mut app, activations) = app_with_counting_view();
        app.picker = Some(Picker::new(vec![profile("prod", Some("eu-west-1"))]));
        app.awaiting_startup_env = true;

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.picker.is_none());
        assert!(!app.awaiting_startup_env);
        assert_eq!(app.epoch, 1);
        assert_eq!(app.env, Env::new("prod", "eu-west-1"));
        assert_eq!(activations.get(), 1, "elegir un profile recarga la vista");
    }

    #[test]
    fn switch_env_bumps_epoch_and_updates_env() {
        let mut app = test_app();
        assert_eq!(app.epoch, 0);
        app.dispatch(Action::SwitchEnv(Env::new("prod", "eu-west-1")));
        assert_eq!(app.epoch, 1);
        assert_eq!(app.env, Env::new("prod", "eu-west-1"));
    }

    #[test]
    fn epoch_guard_discards_stale_and_accepts_fresh() {
        let mut app = test_app();
        app.dispatch(Action::SwitchEnv(Env::new("prod", "eu-west-1"))); // epoch -> 1
        app.status = None;

        // Respuesta con epoch viejo (0): se descarta, no pinta nada.
        app.on_envelope(Envelope::new(0, Message::Error("cuenta anterior".into())));
        assert!(
            app.status.is_none(),
            "el envelope stale no debe pintar nada"
        );

        // Respuesta con el epoch vigente (1): sí se muestra.
        app.on_envelope(Envelope::new(1, Message::Error("error real".into())));
        let status = app.status.as_ref().expect("el envelope vigente sí pinta");
        assert!(status.error && status.text.contains("error real"));
    }
}
