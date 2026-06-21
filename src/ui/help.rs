//! Overlay de ayuda (`?`): tabla de keybindings comunes a todas las vistas,
//! centrada sobre la pantalla.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

const KEYS: &[(&str, &str)] = &[
    (":", "command bar (saltar de servicio, p. ej. :logs)"),
    ("/", "filtrar la lista actual"),
    ("enter", "drill (entrar al detalle)"),
    ("esc", "volver"),
    ("r", "refresh"),
    ("ctrl-e", "cambiar de ambiente"),
    ("?", "mostrar/ocultar esta ayuda"),
    ("q", "salir"),
];

pub fn render(frame: &mut Frame, area: Rect) {
    let popup = popup_area(area, 54, KEYS.len() as u16 + 3);
    frame.render_widget(Clear, popup);

    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(format!(" {k:>8}  "), Style::new().yellow().bold()),
                Span::raw(*desc),
            ])
        })
        .collect();

    let body = Paragraph::new(lines)
        .block(Block::bordered().title(" ayuda — awsdeck (? o esc para cerrar) "));
    frame.render_widget(body, popup);
}

/// Rectángulo centrado de `width` x `height` dentro de `area`.
fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(v);
    h
}
