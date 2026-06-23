//! `App` — estado global (Env activo + epoch), modos de input, vista activa,
//! routing de teclas, status bar y el loop `tokio::select!` (teclado + canal de
//! mensajes). **Agnóstico de servicio:** solo conoce vistas por el registry y
//! reenvía las `Action` de efecto a `effects` sin inspeccionarlas.

use std::time::Duration;

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
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use tokio::sync::mpsc;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::action::{Action, ViewContext};
use crate::aws::context::{Env, ProfileEntry, list_profiles};
use crate::effects::Effects;
use crate::message::{Envelope, ErrorKind, Message};
use crate::ui::{command_bar, confirm, header, help, picker};
use crate::views::Registry;

/// Modo de input del `App`: dónde van las teclas.
enum Mode {
    Normal,
    Command,
    Filter,
}

/// Pantalla activa: el menú principal de herramientas, o una vista concreta.
enum Screen {
    Menu,
    View,
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

/// Modal de confirmación para una acción mutante (gate prod-safe). Lleva la
/// `Action` a despachar si el usuario confirma.
struct Confirm {
    title: String,
    body: String,
    action: Action,
}

pub struct App {
    env: Env,
    epoch: u64,
    registry: Registry,
    effects: Effects,
    rx: mpsc::Receiver<Envelope>,

    /// Pantalla activa (menú principal o vista). Arranca en el menú.
    screen: Screen,
    /// Selección del menú principal (índice sobre `registry.metas()`).
    menu: ListState,
    mode: Mode,
    /// Buffer de edición compartido por los modos `:` y `/`.
    input: Input,
    /// Filtro aplicado a la vista activa (espejo, para mostrarlo en modo normal).
    filter: String,
    status: Option<StatusLine>,
    /// Clase del último error mostrado (lo puebla `Message::Error`). Habilita una
    /// pista persistente de recuperación en el header (p. ej. `[re-auth]` ante una
    /// sesión SSO caducada) sin que el core nombre ningún servicio. `None` = no hay
    /// error vigente; se limpia al llegar data fresca o al cambiar de ambiente.
    error_kind: Option<ErrorKind>,
    show_help: bool,
    picker: Option<Picker>,
    /// Modal de confirmación de una acción mutante (gate prod-safe).
    confirm: Option<Confirm>,
    /// Modo escritura: las acciones mutantes solo proceden si está ON.
    write_mode: bool,
    /// `true` mientras el picker de arranque espera que el usuario elija ambiente;
    /// difiere la carga inicial para no pintar datos del default.
    awaiting_startup_env: bool,
    /// Deadline para disparar la búsqueda server-side (debounce del filtro `/`).
    search_deadline: Option<tokio::time::Instant>,
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
            screen: Screen::Menu,
            menu: ListState::default().with_selected(Some(0)),
            mode: Mode::Normal,
            input: Input::default(),
            filter: String::new(),
            status: None,
            error_kind: None,
            show_help: false,
            picker: None,
            confirm: None,
            write_mode: false,
            awaiting_startup_env: false,
            search_deadline: None,
            should_quit: false,
        }
    }

    /// Corre el loop principal hasta que el usuario sale.
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        // Arranca en el menú principal (Screen::Menu por defecto). El picker de
        // ambiente, si lo hay, se dibuja encima hasta que el usuario elija.
        let mut events = EventStream::new();
        // Tick periódico para refrescos en vivo (p. ej. tail -f de logs). Marca la
        // cadencia; cada vista decide en `on_tick` si refresca. Skip si nos atrasamos.
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !self.should_quit {
            terminal.draw(|frame| self.render(frame))?;

            // Debounce del filtro: la búsqueda server-side dispara al vencer el
            // deadline (o nunca, si no hay uno). Se reconstruye cada iteración, así
            // que cada tecla "resetea" el temporizador.
            let deadline = self.search_deadline;
            let debounce = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(debounce);

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
                () = &mut debounce => {
                    self.search_deadline = None;
                    self.dispatch_active_search();
                }
                _ = tick.tick() => self.on_tick(),
            }
        }
        Ok(())
    }

    /// Tick periódico: si hay una vista activa en modo normal (sin overlays), le
    /// pregunta si quiere refrescar (tail -f). Agnóstico: no sabe qué vista es.
    fn on_tick(&mut self) {
        if !matches!(self.screen, Screen::View)
            || !matches!(self.mode, Mode::Normal)
            || self.confirm.is_some()
            || self.picker.is_some()
            || self.show_help
        {
            return;
        }
        let actions = match self.registry.active_mut() {
            Some(view) => view.on_tick(),
            None => Vec::new(),
        };
        self.dispatch_all(actions);
    }

    // --- Routing --------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        // El info transitorio se limpia con cualquier tecla; un **error sí persiste**
        // (hay que poder leerlo y guía la recuperación). Se descarta con `esc`, con
        // data fresca (`on_envelope`) o al cambiar de ambiente.
        if !self.status.as_ref().is_some_and(|s| s.error) {
            self.status = None;
        }
        // Los overlays interceptan primero; el confirm tiene máxima precedencia.
        if self.confirm.is_some() {
            self.on_confirm_key(key);
            return;
        }
        if self.picker.is_some() {
            self.on_picker_key(key);
            return;
        }
        if self.show_help {
            self.show_help = false;
            return;
        }
        // Command/Filter son independientes de la pantalla (editan el input line).
        match self.mode {
            Mode::Command => return self.on_command_key(key),
            Mode::Filter => return self.on_filter_key(key),
            Mode::Normal => {}
        }
        // Modo normal: enruta según la pantalla activa.
        match self.screen {
            Screen::Menu => self.on_menu_key(key),
            Screen::View => self.on_normal_key(key),
        }
    }

    fn on_menu_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.dispatch(Action::Quit),
            KeyCode::Char(':') => self.enter_command_mode(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('e') if ctrl => self.open_picker(),
            KeyCode::Char('j') | KeyCode::Down => self.menu_move(1),
            KeyCode::Char('k') | KeyCode::Up => self.menu_move(-1),
            KeyCode::Char('g') | KeyCode::Home => self.menu_select_edge(false),
            KeyCode::Char('G') | KeyCode::End => self.menu_select_edge(true),
            KeyCode::Enter => self.menu_activate(),
            KeyCode::Esc if self.status.as_ref().is_some_and(|s| s.error) => self.clear_error(),
            _ => {}
        }
    }

    fn menu_len(&self) -> usize {
        self.registry.metas().len()
    }

    fn menu_move(&mut self, delta: i32) {
        let len = self.menu_len();
        if len == 0 {
            return;
        }
        let cur = self.menu.selected().unwrap_or(0) as i32;
        self.menu
            .select(Some((cur + delta).clamp(0, len as i32 - 1) as usize));
    }

    fn menu_select_edge(&mut self, last: bool) {
        let len = self.menu_len();
        if len > 0 {
            self.menu.select(Some(if last { len - 1 } else { 0 }));
        }
    }

    fn menu_activate(&mut self) {
        let metas = self.registry.metas();
        if let Some((id, _)) = self.menu.selected().and_then(|sel| metas.get(sel)) {
            self.dispatch(Action::ActivateView(id.to_string()));
        }
    }

    /// Vuelve al menú principal (desde `:menu` o backspace).
    fn go_home(&mut self) {
        self.screen = Screen::Menu;
        self.clear_filter();
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.dispatch(Action::Quit),
            KeyCode::Char(':') => self.enter_command_mode(),
            KeyCode::Char('/') => self.enter_filter_mode(),
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('e') if ctrl => self.open_picker(),
            KeyCode::Backspace => self.go_home(),
            // Etapa 0 de `esc`: si hay un error pegajoso en pantalla, el primer `esc`
            // solo lo descarta (y se queda donde estás). La vista no ve este esc.
            KeyCode::Esc if self.status.as_ref().is_some_and(|s| s.error) => {
                self.clear_error();
            }
            // Primera etapa (estilo k9s): `esc` con filtro aplicado lo limpia y se
            // queda en la vista; un segundo `esc` (ya sin filtro) deja que la vista
            // suba de nivel y, desde la raíz, vuelva al menú. La vista no ve este esc.
            KeyCode::Esc if !self.filter.is_empty() => {
                self.clear_filter();
                self.fire_search_now(); // recargar sin filtro (server-side en logs)
                self.set_info("filtro limpiado");
            }
            // Resto (incl. `esc` sin filtro): lo maneja la vista activa.
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
            // esc limpia el filtro, recarga la primera página y vuelve a normal.
            KeyCode::Esc => {
                self.input.reset();
                self.apply_filter();
                self.fire_search_now();
                self.mode = Mode::Normal;
            }
            // enter commitea el filtro y, en la MISMA pulsación, reenvía el Enter a la
            // vista para hacer drill (un solo enter, no dos). El drill de la vista emite
            // `ClearFilter`, así que el filtro no se arrastra al nivel hijo; en una hoja,
            // Enter abre el detalle. Mismo patrón de reenvío que las flechas.
            KeyCode::Enter => {
                self.fire_search_now();
                self.mode = Mode::Normal;
                let actions = match self.registry.active_mut() {
                    Some(view) => view.on_key(key),
                    None => Vec::new(),
                };
                self.dispatch_all(actions);
            }
            // Flechas: navegar los resultados sin salir del filtro (estilo fzf).
            // Se reenvían a la vista (mueve su selección sobre la lista filtrada);
            // no tocan el input de texto ni reprograman la búsqueda server-side.
            KeyCode::Up | KeyCode::Down => {
                let actions = match self.registry.active_mut() {
                    Some(view) => view.on_key(key),
                    None => Vec::new(),
                };
                self.dispatch_all(actions);
            }
            _ => {
                self.input.handle_event(&Event::Key(key));
                self.apply_filter(); // fuzzy local instantáneo sobre lo ya cargado
                // Programar la búsqueda server-side ~280ms tras dejar de escribir.
                self.search_deadline =
                    Some(tokio::time::Instant::now() + Duration::from_millis(280));
            }
        }
    }

    /// Despacha una `Action`: las core las maneja el `App`; el resto las reenvía a
    /// `effects` con el epoch actual (sin nombrar servicios concretos).
    fn dispatch(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::ActivateView(id) => self.activate_view(&id),
            // Handoff entre vistas: activa `id` y le entrega el `context` opaco.
            Action::ActivateViewWithContext { id, context } => {
                self.activate_view_with_context(&id, context)
            }
            // `esc` en la raíz de una vista: volver al menú principal.
            Action::Back => self.go_home(),
            // La vista cambió de nivel de drill: el filtro del nivel anterior no
            // debe arrastrarse. `fire_search_now` recarga la página sin filtro en
            // vistas server-side (logs, guardado por nivel); en client-side es no-op.
            Action::ClearFilter => {
                if !self.filter.is_empty() {
                    self.clear_filter();
                    self.fire_search_now();
                }
            }
            Action::SwitchEnv(env) => self.switch_env(env),
            // Copiar al portapapeles: local (no es efecto del SDK).
            Action::CopyToClipboard { text } => self.copy_to_clipboard(text),
            // Gate prod-safe: las mutantes pasan por modo escritura + confirm.
            mutating if is_mutating(&mutating) => self.request_confirm(mutating),
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
            self.screen = Screen::View;
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

    /// Activación con contexto (handoff). Espeja `activate_view` pero, en vez de
    /// `on_activate`, entrega el `context` a la vista destino vía `on_context` (que lo
    /// interpreta para arrancar en un estado específico). El `App` no inspecciona el
    /// `ViewContext`: lo pasa opaco.
    fn activate_view_with_context(&mut self, id: &str, context: ViewContext) {
        if self.registry.activate(id) {
            self.screen = Screen::View;
            self.clear_filter();
            let actions = match self.registry.active_mut() {
                Some(view) => view.on_context(&context),
                None => Vec::new(),
            };
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
        self.write_mode = false; // re-armar la seguridad al cambiar de cuenta
        self.clear_filter();
        // Solo recargar si estamos dentro de una vista; en el menú no hay qué cargar.
        if matches!(self.screen, Screen::View) {
            let actions = self.on_activate_active();
            self.dispatch_all(actions);
        }
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
            // Sin profiles caemos al default; avísalo (si no, el primer load falla con
            // un error opaco y el usuario no sabe que falta configurar `~/.aws/config`).
            self.set_info(format!(
                "sin profiles en ~/.aws/config — usando {} · ctrl-e para cambiar",
                self.env
            ));
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
                // Cancelar: cerrar el picker. En el de arranque, quedarse en el
                // ambiente por defecto (se aterriza en el menú principal).
                self.picker = None;
                self.awaiting_startup_env = false;
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

    // --- Gate de mutaciones (modo escritura + confirm) ------------------------

    /// Sin modo escritura, bloquea con error; con modo escritura, abre el confirm.
    fn request_confirm(&mut self, action: Action) {
        if !self.write_mode {
            self.set_error("acción mutante bloqueada: activa modo escritura con :write");
            return;
        }
        let confirm = match &action {
            Action::PurgeQueue { queue_url } => {
                let name = queue_url.rsplit('/').next().unwrap_or(queue_url);
                Confirm {
                    title: " purgar cola — irreversible ".to_string(),
                    body: format!("se borrarán TODOS los mensajes de:\n{name}"),
                    action,
                }
            }
            Action::RedriveExecution { execution_arn } => {
                let name = execution_arn.rsplit(':').next().unwrap_or(execution_arn);
                Confirm {
                    title: " redrive ejecución ".to_string(),
                    body: format!("se relanzará desde el último estado fallido:\n{name}"),
                    action,
                }
            }
            Action::SendEvent { event_bus_name } => Confirm {
                title: " enviar evento de prueba ".to_string(),
                body: format!("se publicará un evento de prueba en el bus:\n{event_bus_name}"),
                action,
            },
            // `is_mutating` ya filtró; cualquier otra no debería llegar aquí.
            _ => return,
        };
        self.confirm = Some(confirm);
    }

    fn on_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            // Ya confirmado: va directo a effects (no se re-gatea).
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Some(confirm) = self.confirm.take() {
                    self.effects.dispatch(confirm.action, self.epoch);
                    self.set_info("acción enviada");
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => self.confirm = None,
            _ => {}
        }
    }

    fn on_envelope(&mut self, envelope: Envelope) {
        // EPOCH GUARD: descartar respuestas del ambiente anterior.
        if envelope.epoch != self.epoch {
            return;
        }
        // Data o confirmación fresca: descarta cualquier error pegajoso anterior (el
        // ambiente respondió, la pista de recuperación ya no aplica).
        if !matches!(envelope.message, Message::Error { .. }) {
            self.clear_error();
        }
        match &envelope.message {
            Message::Error { detail, kind } => {
                self.set_error(detail.clone());
                self.error_kind = Some(*kind);
            }
            Message::QueuePurged { .. } => self.set_info("cola purgada — refrescando…"),
            Message::ExecutionRedriven { .. } => self.set_info("redrive enviado — refrescando…"),
            Message::EventSent { event_bus_name } => {
                self.set_info(format!("evento enviado a {event_bus_name}"))
            }
            _ => {}
        }
        if let Some(view) = self.registry.active_mut() {
            view.on_message(&envelope.message);
        }
        // Tras una mutación confirmada, refrescar el detalle (el estado del lado del
        // servidor no se actualiza al instante).
        match envelope.message {
            Message::QueuePurged { queue_url } => {
                self.dispatch(Action::LoadQueueDetail { queue_url })
            }
            Message::ExecutionRedriven { execution_arn } => {
                self.dispatch(Action::LoadExecutionDetail { execution_arn })
            }
            _ => {}
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
            "w" | "write" => self.toggle_write_mode(),
            "menu" | "home" => self.go_home(),
            // `:region <código>` — cambia SOLO la región del ambiente actual (mismo
            // profile), reusando toda la maquinaria de `switch_env` (epoch + clients +
            // recarga). Core agnóstico: no nombra ningún servicio. Cumple la promesa de
            // "ambiente (cuenta + región) cambiable al instante" sin editar ~/.aws/config.
            cmd if cmd == "region" || cmd.starts_with("region ") => {
                let code = cmd.strip_prefix("region").unwrap_or("").trim();
                if code.is_empty() {
                    self.set_error("uso: :region <código>  (p. ej. :region eu-west-1)");
                } else {
                    self.dispatch(Action::SwitchEnv(Env::new(self.env.profile.clone(), code)));
                }
            }
            // No es core: ofrécelo a la vista activa (p. ej. `logs` con `:since 2d`).
            // Si la vista no lo reclama (Vec vacío), trátalo como id de vista.
            other => {
                let actions = match self.registry.active_mut() {
                    Some(view) => view.on_command(other),
                    None => Vec::new(),
                };
                if actions.is_empty() {
                    self.dispatch(Action::ActivateView(other.to_string()));
                } else {
                    self.dispatch_all(actions);
                }
            }
        }
    }

    fn toggle_write_mode(&mut self) {
        self.write_mode = !self.write_mode;
        let state = if self.write_mode { "ON" } else { "OFF" };
        self.set_info(format!("modo escritura: {state}"));
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

    /// Dispara la búsqueda server-side de inmediato (cancela el debounce).
    fn fire_search_now(&mut self) {
        self.search_deadline = None;
        self.dispatch_active_search();
    }

    /// Pide a la vista activa su búsqueda server-side con el filtro actual y
    /// despacha el resultado (vacío para vistas client-side, p. ej. sqs).
    fn dispatch_active_search(&mut self) {
        let query = self.filter.clone();
        let actions = match self.registry.active_mut() {
            Some(view) => view.search(&query),
            None => Vec::new(),
        };
        self.dispatch_all(actions);
    }

    /// Copia `text` al portapapeles del sistema (arboard) y avisa en la status bar.
    /// Síncrono (no es I/O de red); si el portapapeles no está disponible, lo reporta.
    fn copy_to_clipboard(&mut self, text: String) {
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.clone())) {
            Ok(()) => self.set_info(format!("copiado: {text}")),
            Err(e) => self.set_error(format!("no se pudo copiar: {e}")),
        }
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusLine {
            error: true,
            text: text.into(),
        });
        // Por defecto un error no es específicamente de auth; el path tipado
        // (`Message::Error`) sobreescribe `error_kind` con la clase real.
        self.error_kind = Some(ErrorKind::Other);
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusLine {
            error: false,
            text: text.into(),
        });
        self.error_kind = None;
    }

    /// Descarta el error pegajoso vigente (si lo hay) y su pista de recuperación.
    fn clear_error(&mut self) {
        if self.status.as_ref().is_some_and(|s| s.error) {
            self.status = None;
        }
        self.error_kind = None;
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

        let title = match self.screen {
            Screen::Menu => "menú".to_string(),
            Screen::View => self
                .registry
                .active()
                .map(|v| v.title())
                .unwrap_or_else(|| "—".to_string()),
        };
        let auth_warning = self.error_kind == Some(ErrorKind::Auth);
        header::render(
            frame,
            header_area,
            &self.env,
            &title,
            self.write_mode,
            auth_warning,
        );

        match self.screen {
            Screen::Menu => self.render_menu(frame, body_area),
            Screen::View => match self.registry.active_mut() {
                Some(view) => view.render(frame, body_area),
                None => render_placeholder(frame, body_area),
            },
        }

        command_bar::render(frame, footer_area, self.footer_state());

        // Overlays por precedencia: confirm > picker > help.
        if let Some(c) = &self.confirm {
            confirm::render(frame, full, &c.title, &c.body);
        } else if let Some(p) = &mut self.picker {
            picker::render(frame, full, &p.profiles, &mut p.state);
        } else if self.show_help {
            help::render(frame, full);
        }
    }

    fn render_menu(&mut self, frame: &mut Frame, area: Rect) {
        let metas = self.registry.metas();
        let block = Block::bordered().title(" herramientas · enter para abrir ");
        if metas.is_empty() {
            frame.render_widget(
                Paragraph::new("(sin herramientas registradas)").block(block),
                area,
            );
            return;
        }
        let items: Vec<ListItem> = metas
            .iter()
            .map(|(id, desc)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {id:<10}"), Style::new().bold()),
                    Span::styled((*desc).to_string(), Style::new().dark_gray()),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().reversed())
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, area, &mut self.menu);
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
                None => {
                    // Pistas contextuales de la vista activa (solo en una vista, no
                    // en el menú). El core no las interpreta: las reenvía al footer.
                    let view = match self.screen {
                        Screen::View => self
                            .registry
                            .active()
                            .map(|v| v.hints())
                            .unwrap_or_default(),
                        Screen::Menu => Vec::new(),
                    };
                    command_bar::Footer::Hints {
                        filter: &self.filter,
                        view,
                    }
                }
            },
        }
    }
}

/// `true` si la acción es mutante y debe pasar por el gate (modo escritura + confirm).
fn is_mutating(action: &Action) -> bool {
    matches!(
        action,
        Action::PurgeQueue { .. } | Action::RedriveExecution { .. } | Action::SendEvent { .. }
    )
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

    /// Vista falsa que cuenta cuántas veces se activó (carga) y cuántas teclas
    /// recibió en `on_key`. Devuelve `vec![]` para no disparar efectos (que
    /// necesitarían un runtime de tokio).
    struct CountingView {
        activations: Rc<Cell<u32>>,
        keys: Rc<Cell<u32>>,
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
            self.keys.set(self.keys.get() + 1);
            Vec::new()
        }
        fn on_message(&mut self, _message: &Message) {}
        fn set_filter(&mut self, _filter: &str) {}
        fn render(&mut self, _frame: &mut Frame, _area: Rect) {}
    }

    /// `App` con una `CountingView` registrada; devuelve los contadores de
    /// activaciones y de teclas recibidas por la vista.
    fn app_with_counting_view() -> (App, Rc<Cell<u32>>, Rc<Cell<u32>>) {
        let activations = Rc::new(Cell::new(0));
        let keys = Rc::new(Cell::new(0));
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("default", "us-east-1");
        let effects = Effects::new_mock(tx, env.clone());
        let mut registry = Registry::new();
        registry.register(Box::new(CountingView {
            activations: activations.clone(),
            keys: keys.clone(),
        }));
        (App::new(env, registry, effects, rx), activations, keys)
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

    /// Vista que distingue si fue activada normal (`on_activate`) o con contexto
    /// (`on_context`), para verificar el handoff `ActivateViewWithContext`.
    struct ContextView {
        activated: Rc<Cell<u32>>,
        contexts: Rc<Cell<u32>>,
    }
    impl View for ContextView {
        fn id(&self) -> &'static str {
            "logs"
        }
        fn title(&self) -> String {
            "logs".to_string()
        }
        fn on_activate(&mut self) -> Vec<Action> {
            self.activated.set(self.activated.get() + 1);
            Vec::new()
        }
        fn on_context(&mut self, _context: &ViewContext) -> Vec<Action> {
            self.contexts.set(self.contexts.get() + 1);
            Vec::new()
        }
        fn on_key(&mut self, _key: KeyEvent) -> Vec<Action> {
            Vec::new()
        }
        fn on_message(&mut self, _message: &Message) {}
        fn set_filter(&mut self, _filter: &str) {}
        fn render(&mut self, _frame: &mut Frame, _area: Rect) {}
    }

    #[test]
    fn activate_with_context_calls_on_context_not_on_activate() {
        let activated = Rc::new(Cell::new(0));
        let contexts = Rc::new(Cell::new(0));
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("default", "us-east-1");
        let effects = Effects::new_mock(tx, env.clone());
        let mut registry = Registry::new();
        registry.register(Box::new(ContextView {
            activated: activated.clone(),
            contexts: contexts.clone(),
        }));
        let mut app = App::new(env, registry, effects, rx);

        app.dispatch(Action::ActivateViewWithContext {
            id: "logs".to_string(),
            context: ViewContext::LogGroupTail {
                group: "/aws/lambda/Fn".to_string(),
                window: crate::message::LogWindow::Last(3_600_000),
            },
        });

        assert!(matches!(app.screen, Screen::View), "la vista queda activa");
        assert_eq!(contexts.get(), 1, "se entregó el contexto vía on_context");
        assert_eq!(activated.get(), 0, "no se llamó on_activate en el handoff");
    }

    /// Vista que reclama el comando `die` (emite `Quit`) y ningún otro.
    struct CommandView;
    impl View for CommandView {
        fn id(&self) -> &'static str {
            "logs"
        }
        fn title(&self) -> String {
            "logs".to_string()
        }
        fn on_activate(&mut self) -> Vec<Action> {
            Vec::new()
        }
        fn on_key(&mut self, _key: KeyEvent) -> Vec<Action> {
            Vec::new()
        }
        fn on_message(&mut self, _message: &Message) {}
        fn set_filter(&mut self, _filter: &str) {}
        fn on_command(&mut self, cmd: &str) -> Vec<Action> {
            if cmd == "die" {
                vec![Action::Quit]
            } else {
                Vec::new()
            }
        }
        fn render(&mut self, _frame: &mut Frame, _area: Rect) {}
    }

    fn app_with_command_view() -> App {
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("default", "us-east-1");
        let effects = Effects::new_mock(tx, env.clone());
        let mut registry = Registry::new();
        registry.register(Box::new(CommandView));
        App::new(env, registry, effects, rx)
    }

    #[test]
    fn command_routes_to_active_view() {
        let mut app = app_with_command_view();
        // La vista reclama `:die` → su acción (Quit) se despacha.
        type_command(&mut app, "die");
        assert!(app.should_quit, "el comando de la vista se despachó");
    }

    #[test]
    fn command_not_claimed_by_view_falls_back_to_activate() {
        let mut app = app_with_command_view();
        // `:nope` no lo reclama la vista → cae a ActivateView (id inexistente) → error.
        type_command(&mut app, "nope");
        let status = app.status.as_ref().expect("status");
        assert!(status.error && status.text.contains("desconocido"));
    }

    /// Vista que simula un cambio de nivel: cualquier tecla emite `ClearFilter`
    /// (como hacen las vistas reales al drillear/back).
    struct DrillingView;
    impl View for DrillingView {
        fn id(&self) -> &'static str {
            "logs"
        }
        fn title(&self) -> String {
            "logs".to_string()
        }
        fn on_activate(&mut self) -> Vec<Action> {
            Vec::new()
        }
        fn on_key(&mut self, _key: KeyEvent) -> Vec<Action> {
            vec![Action::ClearFilter]
        }
        fn on_message(&mut self, _message: &Message) {}
        fn set_filter(&mut self, _filter: &str) {}
        fn render(&mut self, _frame: &mut Frame, _area: Rect) {}
    }

    #[test]
    fn clear_filter_action_empties_filter() {
        let mut app = test_app();
        app.filter = "order".to_string();
        app.dispatch(Action::ClearFilter);
        assert!(app.filter.is_empty(), "ClearFilter vacía el filtro del App");
    }

    #[test]
    fn drill_within_view_clears_app_filter() {
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("default", "us-east-1");
        let effects = Effects::new_mock(tx, env.clone());
        let mut registry = Registry::new();
        registry.register(Box::new(DrillingView));
        let mut app = App::new(env, registry, effects, rx);
        type_command(&mut app, "logs"); // activa la vista
        app.filter = "order".to_string();
        // Tecla normal que la vista convierte en cambio de nivel (emite ClearFilter).
        app.on_key(ch('x'));
        assert!(
            app.filter.is_empty(),
            "drillear limpia el filtro heredado del nivel anterior"
        );
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
    fn startup_picker_esc_lands_on_menu() {
        let (mut app, activations, _keys) = app_with_counting_view();
        app.picker = Some(Picker::new(vec![profile("dev", None)]));
        app.awaiting_startup_env = true;

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.picker.is_none());
        assert!(!app.awaiting_startup_env);
        assert!(matches!(app.screen, Screen::Menu), "aterriza en el menú");
        assert_eq!(app.epoch, 0, "esc no cambia de ambiente");
        assert_eq!(activations.get(), 0, "el menú no activa ninguna vista");
    }

    #[test]
    fn startup_picker_enter_switches_env_lands_on_menu() {
        let (mut app, activations, _keys) = app_with_counting_view();
        app.picker = Some(Picker::new(vec![profile("prod", Some("eu-west-1"))]));
        app.awaiting_startup_env = true;

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.picker.is_none());
        assert!(!app.awaiting_startup_env);
        assert_eq!(app.epoch, 1);
        assert_eq!(app.env, Env::new("prod", "eu-west-1"));
        assert!(
            matches!(app.screen, Screen::Menu),
            "tras elegir ambiente, al menú"
        );
        assert_eq!(activations.get(), 0, "el menú no activa ninguna vista");
    }

    #[test]
    fn starts_on_menu() {
        let app = test_app();
        assert!(matches!(app.screen, Screen::Menu));
    }

    #[test]
    fn menu_enter_activates_selected_view() {
        let (mut app, activations, _keys) = app_with_counting_view();
        assert!(matches!(app.screen, Screen::Menu));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.screen, Screen::View));
        assert_eq!(activations.get(), 1, "enter en el menú activa la vista");
    }

    #[test]
    fn menu_command_returns_home() {
        let (mut app, _activations, _keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // entra a la vista
        assert!(matches!(app.screen, Screen::View));
        type_command(&mut app, "menu");
        assert!(matches!(app.screen, Screen::Menu));
    }

    #[test]
    fn back_action_returns_to_menu() {
        let (mut app, _activations, _keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // entra a la vista
        assert!(matches!(app.screen, Screen::View));
        // La vista, en su raíz, emite Back al recibir esc; el App vuelve al menú.
        app.dispatch(Action::Back);
        assert!(matches!(app.screen, Screen::Menu), "Back vuelve al menú");
    }

    #[test]
    fn filter_mode_arrows_navigate_view_without_leaving() {
        let (mut app, _activations, keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // entra a la vista
        app.on_key(ch('/')); // abre el filtro
        assert!(matches!(app.mode, Mode::Filter));
        let before = keys.get();

        // Flecha abajo: navega la lista; NO sale del filtro ni edita el texto.
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        assert!(
            matches!(app.mode, Mode::Filter),
            "navegar no sale del filtro"
        );
        assert_eq!(keys.get(), before + 1, "la flecha llega a la vista activa");
        // Las otras dos garantías del fix: la flecha no reprograma el debounce de
        // la búsqueda server-side ni edita el texto del filtro.
        assert!(
            app.search_deadline.is_none(),
            "la flecha no reprograma la búsqueda server-side"
        );
        assert_eq!(
            app.input.value(),
            "",
            "la flecha no edita el texto del filtro"
        );
    }

    #[test]
    fn filter_enter_commits_and_forwards_to_view() {
        let (mut app, _activations, keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // menú → vista
        app.on_key(ch('/')); // abre el filtro
        assert!(matches!(app.mode, Mode::Filter));
        let before = keys.get();

        // Un solo Enter: commitea el filtro Y se reenvía a la vista (drill en una pulsación).
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.mode, Mode::Normal), "enter commitea el filtro");
        assert!(matches!(app.screen, Screen::View), "sigue en la vista");
        assert_eq!(
            keys.get(),
            before + 1,
            "el enter llega a la vista (no hace falta un segundo enter)"
        );
    }

    #[test]
    fn esc_two_stage_clears_filter_first_then_forwards_to_view() {
        let (mut app, _activations, keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // entra a la vista
        // Aplica un filtro vía el flujo real (/, teclear, enter → vuelve a Normal).
        app.on_key(ch('/'));
        app.on_key(ch('x'));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.filter, "x");
        let keys_before = keys.get();

        // 1a etapa: esc con filtro lo limpia y se queda en la vista, SIN reenviar
        // la tecla a la vista.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.filter, "", "el primer esc limpia el filtro");
        assert!(matches!(app.screen, Screen::View), "y se queda en la vista");
        assert_eq!(
            keys.get(),
            keys_before,
            "la 1a etapa no reenvía esc a la vista"
        );

        // 2a etapa: esc sin filtro se reenvía a la vista (que haría back/menú).
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(keys.get(), keys_before + 1, "el segundo esc va a la vista");
    }

    #[test]
    fn sticky_error_survives_navigation_and_clears_on_fresh_data() {
        let (mut app, _act, _keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // entra a la vista
        // Error tipado de auth (epoch vigente 0).
        app.on_envelope(Envelope::new(
            0,
            Message::Error {
                kind: ErrorKind::Auth,
                detail: "list_queues: sesión caducada".into(),
            },
        ));
        assert!(app.status.as_ref().is_some_and(|s| s.error));
        assert_eq!(app.error_kind, Some(ErrorKind::Auth));

        // Una tecla de navegación NO borra el error (debe poder leerse).
        app.on_key(ch('j'));
        assert!(
            app.status.as_ref().is_some_and(|s| s.error),
            "el error persiste tras navegar"
        );
        assert_eq!(
            app.error_kind,
            Some(ErrorKind::Auth),
            "y la pista de re-auth del header también"
        );

        // Data fresca del ambiente lo descarta (ya respondió; el hint no aplica).
        app.on_envelope(Envelope::new(0, Message::QueuesLoaded(vec![])));
        assert!(app.status.is_none(), "data fresca limpia el error pegajoso");
        assert_eq!(app.error_kind, None);
    }

    #[test]
    fn esc_dismisses_sticky_error_without_forwarding() {
        let (mut app, _act, keys) = app_with_counting_view();
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // entra a la vista
        app.on_envelope(Envelope::new(0, Message::err("boom")));
        let keys_before = keys.get();

        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.status.is_none(), "el primer esc descarta el error");
        assert_eq!(
            keys.get(),
            keys_before,
            "y NO reenvía ese esc a la vista (etapa 0)"
        );
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
    fn region_command_switches_region_keeping_profile() {
        let mut app = test_app(); // dev · us-east-1
        type_command(&mut app, "region eu-west-1");
        assert_eq!(app.epoch, 1, "reusa switch_env (sube epoch)");
        assert_eq!(
            app.env,
            Env::new("dev", "eu-west-1"),
            "mismo profile, otra región"
        );
    }

    #[test]
    fn region_command_without_arg_is_a_usage_error() {
        let mut app = test_app();
        type_command(&mut app, "region");
        assert_eq!(app.epoch, 0, "no cambia de ambiente");
        assert!(app.status.as_ref().is_some_and(|s| s.error), "muestra uso");
    }

    #[test]
    fn epoch_guard_discards_stale_and_accepts_fresh() {
        let mut app = test_app();
        app.dispatch(Action::SwitchEnv(Env::new("prod", "eu-west-1"))); // epoch -> 1
        app.status = None;

        // Respuesta con epoch viejo (0): se descarta, no pinta nada.
        app.on_envelope(Envelope::new(0, Message::err("cuenta anterior")));
        assert!(
            app.status.is_none(),
            "el envelope stale no debe pintar nada"
        );

        // Respuesta con el epoch vigente (1): sí se muestra.
        app.on_envelope(Envelope::new(1, Message::err("error real")));
        let status = app.status.as_ref().expect("el envelope vigente sí pinta");
        assert!(status.error && status.text.contains("error real"));
    }

    // --- Gate de mutaciones (modo escritura + confirm) ------------------------

    fn test_app_mock() -> App {
        let (tx, rx) = mpsc::channel(8);
        let env = Env::new("dev", "us-east-1");
        let effects = Effects::new_mock(tx, env.clone());
        App::new(env, Registry::new(), effects, rx)
    }

    fn purge(url: &str) -> Action {
        Action::PurgeQueue {
            queue_url: url.to_string(),
        }
    }

    #[test]
    fn purge_blocked_without_write_mode() {
        let mut app = test_app();
        assert!(!app.write_mode);
        app.dispatch(purge("https://sqs/000/orders"));
        assert!(app.confirm.is_none(), "sin modo escritura no abre confirm");
        let status = app.status.as_ref().expect("status de bloqueo");
        assert!(status.error && status.text.contains("escritura"));
    }

    #[test]
    fn purge_with_write_mode_opens_confirm() {
        let mut app = test_app();
        app.write_mode = true;
        app.dispatch(purge("https://sqs/000/orders"));
        assert!(app.confirm.is_some(), "con modo escritura abre el confirm");
    }

    #[test]
    fn confirm_n_cancels_without_dispatch() {
        let mut app = test_app();
        app.write_mode = true;
        app.dispatch(purge("https://sqs/000/orders"));
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.confirm.is_none(), "n cancela el confirm");
    }

    #[test]
    fn confirm_intercepts_keys_before_normal() {
        let mut app = test_app();
        app.write_mode = true;
        app.dispatch(purge("https://sqs/000/orders"));
        app.on_key(ch('q')); // 'q' normalmente saldría
        assert!(
            !app.should_quit,
            "el confirm intercepta antes del routing normal"
        );
        assert!(app.confirm.is_some());
    }

    #[tokio::test]
    async fn confirm_y_dispatches_to_effects() {
        let mut app = test_app_mock();
        app.write_mode = true;
        app.dispatch(purge(
            "https://sqs.us-east-1.amazonaws.com/000000000000/orders",
        ));
        assert!(app.confirm.is_some());

        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.confirm.is_none(), "y confirma y cierra el modal");

        // El purge (mock) responde QueuePurged por el canal.
        let envelope = app.rx.recv().await.expect("debe llegar un envelope");
        assert!(matches!(envelope.message, Message::QueuePurged { .. }));
    }

    #[test]
    fn write_command_toggles_mode() {
        let mut app = test_app();
        assert!(!app.write_mode);
        type_command(&mut app, "write");
        assert!(app.write_mode);
        type_command(&mut app, "w");
        assert!(!app.write_mode);
    }

    #[test]
    fn switch_env_resets_write_mode() {
        let mut app = test_app();
        app.write_mode = true;
        app.dispatch(Action::SwitchEnv(Env::new("prod", "eu-west-1")));
        assert!(!app.write_mode, "cambiar de ambiente re-arma la seguridad");
    }

    // --- Gate de redrive (sfn) reusa el mismo gate genérico -------------------

    fn redrive(arn: &str) -> Action {
        Action::RedriveExecution {
            execution_arn: arn.to_string(),
        }
    }

    #[test]
    fn redrive_blocked_without_write_mode() {
        let mut app = test_app();
        app.dispatch(redrive("arn:…:execution:m1:exec-fail"));
        assert!(app.confirm.is_none(), "sin modo escritura no abre confirm");
        let status = app.status.as_ref().expect("status de bloqueo");
        assert!(status.error && status.text.contains("escritura"));
    }

    #[test]
    fn redrive_with_write_mode_opens_confirm() {
        let mut app = test_app();
        app.write_mode = true;
        app.dispatch(redrive("arn:…:execution:m1:exec-fail"));
        assert!(app.confirm.is_some(), "con modo escritura abre el confirm");
    }

    #[tokio::test]
    async fn confirm_y_dispatches_redrive_to_effects() {
        let mut app = test_app_mock();
        app.write_mode = true;
        app.dispatch(redrive(
            "arn:aws:states:us-east-1:000:execution:m1:exec-fail",
        ));
        assert!(app.confirm.is_some());

        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.confirm.is_none(), "y confirma y cierra el modal");

        let envelope = app.rx.recv().await.expect("debe llegar un envelope");
        assert!(matches!(
            envelope.message,
            Message::ExecutionRedriven { .. }
        ));
    }

    #[tokio::test]
    async fn redriven_envelope_triggers_detail_reload() {
        let mut app = test_app_mock();
        let arn = "arn:aws:states:us-east-1:000:execution:m1:exec-fail".to_string();
        app.on_envelope(Envelope::new(
            0,
            Message::ExecutionRedriven {
                execution_arn: arn.clone(),
            },
        ));
        // El App re-dispara LoadExecutionDetail (mock) → llega su respuesta.
        let envelope = app.rx.recv().await.expect("debe llegar un envelope");
        assert!(matches!(
            envelope.message,
            Message::ExecutionDetailLoaded { execution_arn, .. } if execution_arn == arn
        ));
    }

    // --- Gate de SendEvent (events) reusa el mismo gate genérico --------------

    fn send_event(bus: &str) -> Action {
        Action::SendEvent {
            event_bus_name: bus.to_string(),
        }
    }

    #[test]
    fn send_event_blocked_without_write_mode() {
        let mut app = test_app();
        app.dispatch(send_event("default"));
        assert!(app.confirm.is_none(), "sin modo escritura no abre confirm");
        let status = app.status.as_ref().expect("status de bloqueo");
        assert!(status.error && status.text.contains("escritura"));
    }

    #[test]
    fn send_event_with_write_mode_opens_confirm() {
        let mut app = test_app();
        app.write_mode = true;
        app.dispatch(send_event("default"));
        assert!(app.confirm.is_some(), "con modo escritura abre el confirm");
    }

    #[tokio::test]
    async fn confirm_y_dispatches_send_event_to_effects() {
        let mut app = test_app_mock();
        app.write_mode = true;
        app.dispatch(send_event("default"));
        assert!(app.confirm.is_some());

        app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.confirm.is_none(), "y confirma y cierra el modal");

        let envelope = app.rx.recv().await.expect("debe llegar un envelope");
        assert!(matches!(envelope.message, Message::EventSent { .. }));
    }

    #[test]
    fn event_sent_envelope_shows_info_without_reload() {
        let mut app = test_app_mock();
        app.on_envelope(Envelope::new(
            0,
            Message::EventSent {
                event_bus_name: "default".into(),
            },
        ));
        let status = app.status.as_ref().expect("status info");
        assert!(!status.error && status.text.contains("default"));
        // PutEvents no cambia ningún listado → no se re-dispara nada.
        assert!(app.rx.try_recv().is_err(), "EventSent no dispara recargas");
    }
}
