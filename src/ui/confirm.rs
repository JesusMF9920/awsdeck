//! Modal de confirmación para acciones mutantes (gate prod-safe). El `App` posee
//! el estado (`Confirm`); aquí solo se dibuja. Borde rojo para señalar peligro.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

pub fn render(frame: &mut Frame, area: Rect, title: &str, body: &str) {
    let popup = super::popup_area(area, 60, 8);
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line> = body.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "y/enter confirmar · n/esc cancelar",
        Style::new().dark_gray(),
    )));

    let body = Paragraph::new(lines).block(
        Block::bordered()
            .border_style(Style::new().red())
            .title(title.to_string()),
    );
    frame.render_widget(body, popup);
}
