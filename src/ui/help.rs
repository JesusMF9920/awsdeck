//! Overlay de ayuda (`?`): tabla de keybindings comunes a todas las vistas,
//! centrada sobre la pantalla.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

const KEYS: &[(&str, &str)] = &[
    (
        ":",
        "command bar (p. ej. :logs, :sqs, :sfn, :events, :lambda)",
    ),
    (
        "/",
        "buscar (fuzzy; en logs tolera mayús/minús; ↑/↓ navega sin salir)",
    ),
    (
        "enter",
        "drill · expandir (log / cuerpo msg sqs / input target events / in-out de estado sfn)",
    ),
    (
        "esc",
        "descarta el error; con filtro lo limpia; si no, volver (raíz → menú)",
    ),
    (
        ":menu / bksp",
        "volver al menú principal (lista herramientas + ★ favoritos + recientes)",
    ),
    ("r", "refresh"),
    ("y", "copiar ARN/URL/línea del item al portapapeles"),
    ("O", "abrir el recurso en la consola web de AWS"),
    (
        "t",
        "logs: ver TODOS los streams del group juntos, por rango de tiempo",
    ),
    ("w / W", "logs: ciclar la ventana de tiempo (15m…7d)"),
    (
        "o",
        "cargar más (tail: ventana · Events: líneas · sfn: ejecuciones / history del detalle)",
    ),
    (
        "*",
        "marcar/quitar favorito del recurso (★ en el menú · recientes auto · por ambiente)",
    ),
    (
        "f",
        "logs: seguir el tail en vivo (tail -f, no te arrastra al fondo)",
    ),
    (
        ":since/:from",
        "logs: rango — :since 2d · :from 2026-06-19 [to …] (UTC)",
    ),
    (
        "P",
        "events: expandir el event_pattern completo (scroll + copia)",
    ),
    (
        ":status",
        "sfn: filtrar ejecuciones por estado (:status failed / all)",
    ),
    (
        "l",
        "abrir los logs de la Lambda (sfn: del estado · lambda: de la función)",
    ),
    ("p", "purgar cola SQS — gated por modo escritura"),
    ("d", "redrive DLQ (sqs, sobre un dead-letter) — gated"),
    ("R", "redrive ejecución sfn — gated por modo escritura"),
    (
        "S",
        "enviar evento (events): chooser de presets (config.toml) + form editable — gated",
    ),
    (":write", "alternar modo escritura (acciones mutantes)"),
    ("ctrl-e", "cambiar de ambiente (picker de profiles)"),
    (
        ":region",
        "cambiar SOLO la región del ambiente actual (p. ej. :region eu-west-1)",
    ),
    (
        ":set",
        "persistir un default en config.toml (default_profile/region/tail_window)",
    ),
    ("?", "mostrar/ocultar esta ayuda"),
    ("q", "salir"),
];

pub fn render(frame: &mut Frame, area: Rect) {
    let popup = super::popup_area(area, 76, KEYS.len() as u16 + 3);
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
