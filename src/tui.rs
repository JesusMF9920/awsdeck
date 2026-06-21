//! Guard de terminal: entra a raw mode + alternate screen y restaura SIEMPRE.
//!
//! - `Drop` restaura en el path normal y en early-return por `?`.
//! - Un panic hook (encadenado *antes* del de color-eyre) restaura la terminal
//!   primero, para que el reporte bonito de color-eyre se imprima sobre una
//!   terminal sana en vez de quedar comida por el alternate screen / raw mode.
//!
//! Por eso NO usamos `ratatui::init()`: instala su propio panic hook y nos
//! quitaría el control del orden `color-eyre install -> restore -> reporte`.

use std::io::stdout;
use std::panic;

use color_eyre::eyre::Result;
use ratatui::DefaultTerminal;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// RAII guard de la terminal. Mientras este valor viva, estamos en raw mode +
/// alternate screen.
pub struct Tui {
    pub terminal: DefaultTerminal,
}

impl Tui {
    /// Instala color-eyre, encadena el panic hook con la restauración de la
    /// terminal y entra a raw mode + alternate screen. Llamar una sola vez.
    pub fn init() -> Result<Self> {
        // 1. color-eyre primero: su hook queda como renderer base del reporte.
        color_eyre::install()?;

        // 2. Encadenar la restauración ANTES del hook previo (el de color-eyre),
        //    para que la terminal esté sana cuando se imprima el reporte.
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = restore(); // best-effort: en pleno panic ignoramos errores.
            previous(info);
        }));

        // 3. Entrar a raw mode + alternate screen (sin captura de mouse en v0,
        //    para no romper la selección de texto del usuario).
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen)?;

        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        Ok(Self { terminal })
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore();
    }
}

/// Restaura la terminal: sale del alternate screen y desactiva raw mode.
/// Best-effort e idempotente: se invoca tanto desde `Drop` como desde el panic
/// hook, y llamarla dos veces no hace daño.
pub fn restore() -> std::io::Result<()> {
    execute!(stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}
