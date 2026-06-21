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
    /// Modo normal: hints de teclado, o el filtro aplicado si lo hay.
    Hints { filter: &'a str },
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
        Footer::Hints { filter } => {
            let line = if filter.is_empty() {
                Line::from(vec![
                    key(" :"),
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
                ])
            } else {
                Line::from(vec![
                    label(" filtro: "),
                    Span::styled(filter.to_string(), Style::new().yellow().bold()),
                    label("   (/ editar · esc limpiar)"),
                ])
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
