//! Overlay de ayuda (`?`): tabla de keybindings comunes a todas las vistas,
//! centrada sobre la pantalla.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

const KEYS: &[(&str, &str)] = &[
    (":", "command bar (p. ej. :logs, :sqs)"),
    ("/", "filtrar la lista actual"),
    ("enter", "drill (entrar al detalle)"),
    ("esc", "volver"),
    ("r", "refresh"),
    ("p", "purgar cola SQS — gated por modo escritura"),
    (":write", "alternar modo escritura (acciones mutantes)"),
    ("ctrl-e", "cambiar de ambiente"),
    ("?", "mostrar/ocultar esta ayuda"),
    ("q", "salir"),
];

pub fn render(frame: &mut Frame, area: Rect) {
    let popup = super::popup_area(area, 62, KEYS.len() as u16 + 3);
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
