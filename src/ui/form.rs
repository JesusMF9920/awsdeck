//! Form modal genérico de varios campos de texto (reusa `tui-input`). Agnóstico:
//! una vista lo arma con rótulos + valores iniciales, recibe el `FormOutcome` de cada
//! tecla y lee `values()` al enviar. No conoce ningún servicio.
//!
//! Ruteo de teclas: mientras el form está abierto, la vista declara `wants_raw_input()`
//! y el `App` le reenvía TODAS las teclas crudas (incluidas `:`/`/`/`q`), así se puede
//! teclear JSON sin que el core las intercepte. `tab`/`↓` y `shift-tab`/`↑` cambian de
//! campo; `enter` envía; `esc` cancela; el resto edita el campo con foco.

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

/// Ancho fijo del rótulo (para alinear los campos y ubicar el cursor).
const LABEL_W: usize = 13;

/// Qué hacer tras una tecla del form.
#[derive(Debug, PartialEq, Eq)]
pub enum FormOutcome {
    /// Sigue editando (cambio de foco o edición de texto).
    Editing,
    /// El usuario pidió enviar (`enter`): la vista lee `values()` y valida.
    Submit,
    /// El usuario canceló (`esc`): la vista cierra el form.
    Cancel,
}

struct FormField {
    label: String,
    input: Input,
}

pub struct Form {
    title: String,
    fields: Vec<FormField>,
    focus: usize,
    error: Option<String>,
}

impl Form {
    /// Arma el form con un título y pares `(rótulo, valor inicial)`.
    pub fn new(
        title: impl Into<String>,
        fields: Vec<(impl Into<String>, impl Into<String>)>,
    ) -> Self {
        let fields = fields
            .into_iter()
            .map(|(label, value)| FormField {
                label: label.into(),
                input: Input::new(value.into()),
            })
            .collect();
        Self {
            title: title.into(),
            fields,
            focus: 0,
            error: None,
        }
    }

    /// Procesa una tecla y devuelve qué hacer. Editar limpia el error anterior.
    pub fn on_key(&mut self, key: KeyEvent) -> FormOutcome {
        let n = self.fields.len().max(1);
        match key.code {
            KeyCode::Esc => FormOutcome::Cancel,
            KeyCode::Enter => FormOutcome::Submit,
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % n;
                FormOutcome::Editing
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + n - 1) % n;
                FormOutcome::Editing
            }
            _ => {
                if let Some(f) = self.fields.get_mut(self.focus) {
                    f.input.handle_event(&Event::Key(key));
                }
                self.error = None;
                FormOutcome::Editing
            }
        }
    }

    /// Valores actuales de los campos, en orden.
    pub fn values(&self) -> Vec<String> {
        self.fields
            .iter()
            .map(|f| f.input.value().to_string())
            .collect()
    }

    /// Muestra un error de validación bajo los campos (se borra al editar).
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.error = Some(msg.into());
    }

    pub fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("tab", "campo"), ("enter", "enviar"), ("esc", "cancelar")]
    }

    /// Pinta el form como popup centrado. Coloca el cursor en el campo con foco.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let height = self.fields.len() as u16 + 4; // borde(2) + campos + error/hint(2)
        let popup = super::popup_area(area, 72, height);
        frame.render_widget(Clear, popup);

        let block = Block::bordered().title(format!(" {} ", self.title));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let mut lines: Vec<Line> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let label = format!("{:>LABEL_W$}: ", f.label);
                let label_style = if i == self.focus {
                    Style::new().yellow().bold()
                } else {
                    Style::new().dark_gray()
                };
                Line::from(vec![
                    Span::styled(label, label_style),
                    Span::raw(f.input.value().to_string()),
                ])
            })
            .collect();
        lines.push(Line::from(""));
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                err.clone(),
                Style::new().red().bold(),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // Cursor en el campo con foco: tras el rótulo de ancho fijo + ": ".
        if let Some(f) = self.fields.get(self.focus) {
            let prefix = (LABEL_W + 2) as u16;
            let x = inner
                .x
                .saturating_add(prefix)
                .saturating_add(f.input.visual_cursor() as u16);
            let max_x = inner.x + inner.width.saturating_sub(1);
            frame.set_cursor_position((x.min(max_x), inner.y + self.focus as u16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn form() -> Form {
        Form::new("enviar", vec![("source", "src"), ("detail", "{}")])
    }

    #[test]
    fn tab_cycles_focus_and_typing_edits_focused_field() {
        let mut f = form();
        assert_eq!(f.values(), vec!["src", "{}"]);
        // Editar el primer campo (foco 0).
        assert_eq!(f.on_key(ch('X')), FormOutcome::Editing);
        assert_eq!(f.values()[0], "srcX");
        // Tab → foco al segundo campo; editar lo cambia a él.
        assert_eq!(f.on_key(k(KeyCode::Tab)), FormOutcome::Editing);
        f.on_key(ch('!'));
        assert_eq!(f.values()[1], "{}!");
        assert_eq!(f.values()[0], "srcX", "el primer campo no cambió");
    }

    #[test]
    fn enter_submits_and_esc_cancels() {
        let mut f = form();
        assert_eq!(f.on_key(k(KeyCode::Enter)), FormOutcome::Submit);
        assert_eq!(f.on_key(k(KeyCode::Esc)), FormOutcome::Cancel);
    }

    #[test]
    fn set_error_clears_on_edit() {
        let mut f = form();
        f.set_error("detail no es JSON");
        assert!(f.error.is_some());
        f.on_key(ch('x'));
        assert!(f.error.is_none(), "editar limpia el error");
    }

    #[test]
    fn render_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let f = form();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|frame| f.render(frame, frame.area())).unwrap();
    }
}
