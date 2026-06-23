//! Panel de detalle scrolleable reusable: muestra un texto **completo** (con wrap y
//! scroll, JSON pretty si parsea) ocupando el cuerpo de la vista en vez de la lista.
//! Lo usan `logs` (línea de log), `events` (event_pattern / input de un target) y `sqs`
//! (cuerpo de un mensaje) para que un contenido largo no se quede truncado en la fila.
//!
//! La vista guarda un `Option<DetailPanel>`; mientras está `Some`, le reenvía las teclas
//! (scroll/cerrar) y lo pinta. El texto se **snapshotea** al abrir, así una recarga de la
//! lista detrás no lo invalida. Copiar (`y`) lo decide la vista con `content()`.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Paragraph, Wrap};

/// Panel de detalle: un rótulo contextual + el texto crudo + el scroll vertical.
pub struct DetailPanel {
    /// Rótulo contextual (p. ej. `12:00:00 · stream`, `target orders`, `msg abc123`).
    title: String,
    /// Texto crudo (lo que se copia con `y`); el render lo pretty-printea si es JSON.
    raw: String,
    scroll: u16,
}

impl DetailPanel {
    pub fn new(title: impl Into<String>, raw: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            raw: raw.into(),
            scroll: 0,
        }
    }

    /// Texto crudo del panel, para copiar al portapapeles (`y`).
    pub fn content(&self) -> &str {
        &self.raw
    }

    /// Procesa una tecla mientras el panel está abierto. Devuelve `true` si hay que
    /// **cerrarlo** (`esc`/`enter`); las demás teclas scrollean y devuelven `false`.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll = u16::MAX, // clamp al render
            KeyCode::Esc | KeyCode::Enter => return true,
            _ => {}
        }
        false
    }

    /// Hints contextuales del panel (la vista los antepone a los suyos).
    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("j/k", "scroll"), ("y", "copiar"), ("esc", "cerrar")]
    }

    /// Pinta el panel: cabecera + contenido con wrap y scroll (JSON pretty si parsea).
    /// Ocupa el `area` entero (el cuerpo de la vista).
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let body = pretty_or_raw(&self.raw);
        let total = body.lines().count() as u16;
        // Clampa el scroll (soporta `G` = u16::MAX) al final del contenido.
        self.scroll = self.scroll.min(total.saturating_sub(1));
        let title = format!(" {} · esc cierra · j/k scroll ", self.title);
        let para = Paragraph::new(body)
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(para, area);
    }
}

/// Pretty-print si el texto es JSON válido; si no, el texto tal cual. Para el panel de
/// detalle, donde sí queremos saltos de línea e indentación.
pub fn pretty_or_raw(msg: &str) -> String {
    serde_json::from_str::<serde_json::Value>(msg.trim())
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_and_enter_close_other_keys_scroll() {
        let mut p = DetailPanel::new("t", "raw");
        assert!(!p.on_key(k(KeyCode::Char('j'))), "j scrollea, no cierra");
        assert!(!p.on_key(k(KeyCode::Char('G'))), "G no cierra");
        assert!(p.on_key(k(KeyCode::Esc)), "esc cierra");
        assert!(p.on_key(k(KeyCode::Enter)), "enter cierra");
    }

    #[test]
    fn content_returns_raw_for_copy() {
        let p = DetailPanel::new("t", r#"{"a":1}"#);
        assert_eq!(p.content(), r#"{"a":1}"#, "copia el crudo, no el pretty");
    }

    #[test]
    fn pretty_or_raw_prettifies_only_json() {
        assert!(
            pretty_or_raw(r#"{"a":1}"#).contains('\n'),
            "JSON → multilínea"
        );
        assert_eq!(pretty_or_raw("texto plano"), "texto plano");
    }
}
