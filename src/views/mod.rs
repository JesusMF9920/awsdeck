//! El contrato de plugin `View` (síncrono y object-safe) + el `Registry` genérico
//! que conecta vistas por su `id()`. Nada aquí nombra un servicio concreto: las
//! vistas se registran desde `main.rs` (composition root).

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;

use crate::action::Action;
use crate::message::Message;

pub mod logs;
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

    /// Reacciona a una tecla (en modo normal) y devuelve acciones a despachar.
    /// La navegación interna (drill/back, mover selección) se resuelve aquí.
    fn on_key(&mut self, key: KeyEvent) -> Vec<Action>;

    /// Ingiere un resultado async. La vista ignora los `Message` que no le
    /// corresponden. Es el punto de inyección para tests sin red.
    fn on_message(&mut self, message: &Message);

    /// Aplica el filtro actual (el texto del modo `/`). La vista decide cómo
    /// filtra su lista y re-clampa la selección.
    fn set_filter(&mut self, filter: &str);

    /// Dibuja la vista dentro de `area`.
    fn render(&mut self, frame: &mut Frame, area: Rect);

    /// Descripción de una línea para el menú principal. Default vacío.
    fn description(&self) -> &'static str {
        ""
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
