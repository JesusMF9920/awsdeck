//! Componentes de UI transversales y consistentes en todas las vistas: header
//! (ambiente + breadcrumbs), command bar (`:` y `/`), overlay de ayuda y picker
//! de ambientes.

use ratatui::layout::{Constraint, Flex, Layout, Rect};

pub mod command_bar;
pub mod confirm;
pub mod detail;
pub mod header;
pub mod help;
pub mod picker;

/// Rectángulo centrado de `width` x `height` dentro de `area`, para overlays.
pub fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let [v] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [h] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(v);
    h
}
