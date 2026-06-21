//! Guard de terminal: entra a raw mode + alternate screen y restaura SIEMPRE
//! (Drop guard para el path normal/`?` + panic hook para que la terminal quede
//! limpia incluso en panic, antes de que color-eyre imprima su reporte).
//!
//! Se llena en el commit "tui guard".
