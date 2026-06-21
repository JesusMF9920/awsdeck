//! awsdeck — un TUI tipo k9s para los servicios de AWS que uso a diario.
//!
//! Composition root: arma el registry de vistas, registra las vistas concretas
//! (solo aquí se nombran), construye el `App` y corre el loop. Ver `ROADMAP.md`
//! y `CLAUDE.md` para la arquitectura.

mod action;
mod app;
mod aws;
mod effects;
mod message;
mod tui;
mod ui;
mod views;

use color_eyre::eyre::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Alignment;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

use crate::tui::Tui;

#[tokio::main]
async fn main() -> Result<()> {
    // SMOKE TEST temporal del Tui guard (se reemplaza por el loop `tokio::select!`
    // real en el commit "app loop + ui shell"). Valida el criterio de aceptación:
    // `cargo run` abre el TUI y al salir (o en panic) la terminal queda limpia.
    let mut tui = Tui::init()?;

    loop {
        tui.terminal.draw(|frame| {
            let body = Paragraph::new(vec![
                Line::from("awsdeck".bold()),
                Line::from(""),
                Line::from("TUI guard OK — la terminal se restaura al salir y en panic."),
                Line::from("Presiona q para salir.".dim()),
            ])
            .alignment(Alignment::Center)
            .block(Block::bordered().title(" awsdeck "));
            frame.render_widget(body, frame.area());
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                let quit = matches!(key.code, KeyCode::Char('q'))
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL));
                if quit {
                    break;
                }
            }
        }
    }

    Ok(())
}
