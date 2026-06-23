//! Header: indicador de ambiente (`profile · region`) a la derecha y el título de
//! la vista activa a la izquierda. Se dibuja siempre, en la fila superior.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::aws::context::Env;

/// Dibuja la barra superior: `awsdeck · <título>` a la izquierda y el ambiente
/// activo a la derecha, separados por relleno calculado al ancho del área.
/// `auth_warning` añade una pista persistente `[re-auth]` cuando la sesión/credenciales
/// caducaron (el `App` la deriva del último error sin nombrar ningún servicio).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    env: &Env,
    title: &str,
    write_mode: bool,
    auth_warning: bool,
) {
    let auth = if auth_warning { "[re-auth] " } else { "" };
    let badge = if write_mode { "[ESCRITURA] " } else { "" };
    let left = format!(" awsdeck · {title}");
    let right = format!("{env} ");

    let width = area.width as usize;
    let used =
        auth.chars().count() + badge.chars().count() + left.chars().count() + right.chars().count();
    let pad = width.saturating_sub(used);

    let line = Line::from(vec![
        Span::styled(auth, Style::new().red().bold()),
        Span::styled(badge, Style::new().red().bold()),
        Span::styled(left, Style::new().bold()),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::new().cyan().bold()),
    ]);

    let header = Paragraph::new(line).style(Style::new().on_dark_gray());
    frame.render_widget(header, area);
}
