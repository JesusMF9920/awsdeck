//! El contrato de plugin `View` (síncrono y object-safe) + el `Registry` genérico
//! que conecta vistas por su `id()`. Nada aquí nombra un servicio concreto: las
//! vistas se registran desde `main.rs` (composition root).

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;

use crate::action::{Action, ViewContext};
use crate::message::Message;

pub mod events;
pub mod logs;
pub mod sfn;
pub mod sqs;

/// Contrato que implementa cada vista. **Síncrono y object-safe** (`Box<dyn View>`):
/// sin async, sin SDK. Las vistas son puras —mantienen estado, dibujan y traducen
/// teclas a `Action`s— y reciben datos del mundo async vía `on_message`.
pub trait View {
    /// Alias de comando, estable y único (p. ej. `"logs"`). Lo usa el command bar
    /// (`:logs`) y el registry para resolver la vista.
    fn id(&self) -> &'static str;

    /// Título para el header / breadcrumbs. Puede cambiar según el drill
    /// (p. ej. `"logs"` → `"logs / mi-log-group"`).
    fn title(&self) -> String;

    /// Acciones a emitir cuando la vista se vuelve activa (típicamente cargar su
    /// data inicial). El `App` las despacha igual que las de `on_key`.
    fn on_activate(&mut self) -> Vec<Action>;

    /// Activación **con contexto** (handoff desde otra vista vía
    /// [`Action::ActivateViewWithContext`]). El `App` la llama en vez de `on_activate`
    /// y pasa el `context` opaco; la vista lo interpreta para arrancar en un estado
    /// específico (p. ej. `logs` abre el tail de un group). Default: ignora el contexto
    /// y cae a `on_activate` (una vista que no entiende el contexto se activa normal).
    fn on_context(&mut self, context: &ViewContext) -> Vec<Action> {
        let _ = context;
        self.on_activate()
    }

    /// Reacciona a una tecla (en modo normal) y devuelve acciones a despachar.
    /// La navegación interna (drill/back, mover selección) se resuelve aquí.
    ///
    /// **Contrato de `esc` (navegación uniforme, principio #4):** al recibir
    /// `KeyCode::Esc` la vista debe despojar un nivel de drill si lo hay; y en su
    /// nivel raíz, donde `esc` ya no tiene nada que despojar, **debe emitir
    /// [`Action::Back`]** para que el `App` vuelva al menú principal. Una vista que
    /// lo omita deja `esc` muerto en su raíz (no vuelve al menú). El `App` no puede
    /// forzarlo —`on_key` devuelve un `Vec<Action>` opaco—, así que es responsabilidad
    /// de cada vista. Ver `logs::LogsView::back` / `sqs::SqsView::back` como referencia.
    fn on_key(&mut self, key: KeyEvent) -> Vec<Action>;

    /// Ingiere un resultado async. La vista ignora los `Message` que no le
    /// corresponden. Es el punto de inyección para tests sin red.
    fn on_message(&mut self, message: &Message);

    /// Aplica el filtro actual (el texto del modo `/`). La vista decide cómo
    /// filtra su lista y re-clampa la selección.
    fn set_filter(&mut self, filter: &str);

    /// Búsqueda server-side: el `App` la llama (con debounce) cuando cambia el
    /// filtro, para vistas que cargan datos acotados (p. ej. logs a escala).
    /// Default vacío: las vistas client-side no consultan al servidor.
    fn search(&mut self, query: &str) -> Vec<Action> {
        let _ = query;
        Vec::new()
    }

    /// Comando del command bar (`:foo bar`) que el `App` no reconoció como core.
    /// El `App` —agnóstico— no lo parsea: lo reenvía a la vista activa, que decide
    /// (p. ej. `logs` interpreta `:since 2d`). Devolver `Vec` vacío = "no es mío";
    /// el `App` cae entonces a tratarlo como `id` de vista (`ActivateView`).
    fn on_command(&mut self, cmd: &str) -> Vec<Action> {
        let _ = cmd;
        Vec::new()
    }

    /// Tick periódico del `App` (cadencia fija). La vista decide si quiere refrescar
    /// (p. ej. el tail en vivo de `logs` re-consulta); default no-op. El `App` solo
    /// tickea cuando hay una vista activa en modo normal, sin overlays.
    fn on_tick(&mut self) -> Vec<Action> {
        Vec::new()
    }

    /// `true` si la vista está capturando **entrada de texto cruda** (p. ej. un form
    /// abierto donde se teclea JSON). Cuando lo es, el `App` le reenvía TODAS las teclas
    /// sin interceptar `:`/`/`/`q`/`esc`/etc. Agnóstico: el core no sabe POR QUÉ, solo
    /// deja de robar teclas. Default `false` (modo normal). Ver `events::EventsView`.
    fn wants_raw_input(&self) -> bool {
        false
    }

    /// Dibuja la vista dentro de `area`.
    fn render(&mut self, frame: &mut Frame, area: Rect);

    /// Descripción de una línea para el menú principal. Default vacío.
    fn description(&self) -> &'static str {
        ""
    }

    /// Pistas de teclado **contextuales** que la vista quiere anunciar según su
    /// estado actual (p. ej. el nivel de drill): pares `(tecla, qué hace)`. El `App`
    /// las pinta en el footer ANTES de los hints globales —agnóstico: no las
    /// interpreta, solo las muestra—. Es el canal para hacer descubribles las teclas
    /// específicas de cada vista (`t`/`w` en logs, `p`/`R`/`S` gated, …) sin que el
    /// core las conozca. Default vacío: la vista no contribuye ninguna. Mantenerlas
    /// a 1–3 por estado para no saturar la única fila del footer.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}

/// Registro de vistas. Genérico: no conoce ningún servicio. La primera vista
/// registrada queda activa por defecto.
pub struct Registry {
    views: Vec<Box<dyn View>>,
    active: usize,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            views: Vec::new(),
            active: 0,
        }
    }

    /// Registra una vista concreta (llamado solo desde el composition root).
    pub fn register(&mut self, view: Box<dyn View>) {
        self.views.push(view);
    }

    /// Referencia a la vista activa, o `None` si el registry está vacío (p. ej.
    /// antes de registrar cualquier vista). Sin panics.
    pub fn active(&self) -> Option<&dyn View> {
        self.views.get(self.active).map(|v| v.as_ref())
    }

    /// Referencia mutable a la vista activa, o `None` si no hay ninguna.
    /// El object lifetime es `'static` (las vistas no toman prestado nada); la
    /// del préstamo `&mut` se elide a la de `&mut self`.
    pub fn active_mut(&mut self) -> Option<&mut (dyn View + 'static)> {
        self.views.get_mut(self.active).map(|v| v.as_mut())
    }

    /// Activa la vista con este `id`. Devuelve `true` si existía.
    pub fn activate(&mut self, id: &str) -> bool {
        match self.views.iter().position(|v| v.id() == id) {
            Some(idx) => {
                self.active = idx;
                true
            }
            None => false,
        }
    }

    /// Ids registrados (para validar comandos y, a futuro, autocompletar).
    pub fn ids(&self) -> Vec<&'static str> {
        self.views.iter().map(|v| v.id()).collect()
    }

    /// `(id, description)` de cada vista, en orden de registro. Lo usa el menú
    /// principal; el core sigue sin nombrar servicios.
    pub fn metas(&self) -> Vec<(&'static str, &'static str)> {
        self.views
            .iter()
            .map(|v| (v.id(), v.description()))
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
