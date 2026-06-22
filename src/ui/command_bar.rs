//! Command bar / footer: la fila inferior. Comparte espacio (estilo vim) entre el
//! input de `:` (comandos) y `/` (filtro), la status bar y la línea de hints.
//! El `App` decide qué estado mostrar; aquí solo se dibuja.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// Qué mostrar en la fila inferior.
pub enum Footer<'a> {
    /// Input activo de comando (`:`) o filtro (`/`). `cursor` es el offset visual.
    Input {
        prefix: char,
        value: &'a str,
        cursor: usize,
    },
    /// Mensaje de estado (info o error).
    Status { error: bool, text: &'a str },
    /// Modo normal: hints de teclado, o el filtro aplicado si lo hay. `view` son las
    /// pistas contextuales de la vista activa (`View::hints`), que se pintan ANTES de
    /// los hints globales. Vacío = footer idéntico al global de siempre.
    Hints {
        filter: &'a str,
        view: Vec<(&'static str, &'static str)>,
    },
}

pub fn render(frame: &mut Frame, area: Rect, footer: Footer<'_>) {
    match footer {
        Footer::Input {
            prefix,
            value,
            cursor,
        } => {
            frame.render_widget(Paragraph::new(format!("{prefix}{value}")), area);
            // Cursor real: después del prefijo (1 col) + offset visual del input.
            let x = area.x.saturating_add(1).saturating_add(cursor as u16);
            let max_x = area.x + area.width.saturating_sub(1);
            frame.set_cursor_position((x.min(max_x), area.y));
        }
        Footer::Status { error, text } => {
            let style = if error {
                Style::new().red().bold()
            } else {
                Style::new().green()
            };
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {text}"), style)),
                area,
            );
        }
        Footer::Hints { filter, view } => {
            let line = if !filter.is_empty() {
                Line::from(vec![
                    label(" filtro: "),
                    Span::styled(filter.to_string(), Style::new().yellow().bold()),
                    label("   (/ editar · esc limpiar)"),
                ])
            } else {
                // Pistas contextuales de la vista (si las hay) y luego las globales.
                // Van primero porque son lo no-obvio; si la fila se desborda, ratatui
                // recorta por la derecha (las globales, que además están en `?`).
                let mut spans = vec![Span::raw(" ")];
                for &(k, desc) in &view {
                    spans.push(key(k));
                    spans.push(label(&format!("{desc}  ")));
                }
                if !view.is_empty() {
                    spans.push(label("· "));
                }
                spans.extend([
                    key(":"),
                    label("cmd  "),
                    key("/"),
                    label("filtro  "),
                    key("enter"),
                    label("drill  "),
                    key("esc"),
                    label("back  "),
                    key("r"),
                    label("refresh  "),
                    key("^e"),
                    label("ambiente  "),
                    key("?"),
                    label("ayuda  "),
                    key("q"),
                    label("salir"),
                ]);
                Line::from(spans)
            };
            frame.render_widget(Paragraph::new(line), area);
        }
    }
}

/// Span amarillo para una tecla (con espacio final).
fn key(k: &str) -> Span<'static> {
    Span::styled(format!("{k} "), Style::new().yellow())
}

/// Span tenue para el texto descriptivo.
fn label(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::new().dark_gray())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn footer_line(footer: Footer, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal.draw(|f| render(f, f.area(), footer)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect()
    }

    #[test]
    fn hints_pinta_contexto_antes_de_globales() {
        let line = footer_line(
            Footer::Hints {
                filter: "",
                view: vec![("t", "logs por tiempo")],
            },
            120,
        );
        assert!(
            line.contains("logs por tiempo"),
            "muestra el hint contextual: {line:?}"
        );
        assert!(line.contains("cmd"), "y los globales: {line:?}");
        assert!(
            line.find("logs por tiempo") < line.find("cmd"),
            "los contextuales van antes que los globales: {line:?}"
        );
    }

    #[test]
    fn hints_sin_contexto_son_solo_globales() {
        // view vacío = footer global de siempre (cero regresión).
        let line = footer_line(
            Footer::Hints {
                filter: "",
                view: vec![],
            },
            120,
        );
        assert!(line.contains("cmd") && line.contains("salir"));
        assert!(
            !line.contains("logs por tiempo"),
            "sin view no hay contextuales: {line:?}"
        );
    }
}
