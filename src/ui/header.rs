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
/// caducaron (el `App` la deriva del último error sin nombrar ningún servicio). `account`
/// es el id de cuenta confirmado por STS, que se muestra junto al ambiente (prod-safe).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    env: &Env,
    title: &str,
    write_mode: bool,
    auth_warning: bool,
    account: Option<&str>,
) {
    let auth = if auth_warning { "[re-auth] " } else { "" };
    let badge = if write_mode { "[ESCRITURA] " } else { "" };
    let left = format!(" awsdeck · {title}");
    let right = match account {
        Some(acct) => format!("{env} · {acct} "),
        None => format!("{env} "),
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn header_line(
        env: &Env,
        write_mode: bool,
        auth_warning: bool,
        account: Option<&str>,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 1)).unwrap();
        terminal
            .draw(|f| render(f, f.area(), env, "logs", write_mode, auth_warning, account))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect()
    }

    #[test]
    fn shows_confirmed_account_next_to_env() {
        let env = Env::new("prod", "eu-west-1");
        let line = header_line(&env, false, false, Some("123456789012"));
        assert!(line.contains("prod · eu-west-1"), "el ambiente: {line:?}");
        assert!(
            line.contains("123456789012"),
            "la cuenta confirmada: {line:?}"
        );
        assert!(!line.contains("[re-auth]"), "sin error de auth: {line:?}");
    }

    #[test]
    fn shows_reauth_badge_only_on_auth_warning() {
        let env = Env::new("prod", "eu-west-1");
        assert!(!header_line(&env, false, false, None).contains("[re-auth]"));
        assert!(
            header_line(&env, false, true, None).contains("[re-auth]"),
            "ante un error de auth, pista persistente de re-auth"
        );
    }
}
