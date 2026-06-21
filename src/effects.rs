//! `effects` — el dispatcher y la ÚNICA frontera con el SDK. Recibe un `Action`,
//! hace `tokio::spawn` de una task contra el client correcto y manda un `Message`
//! de vuelta por el canal mpsc, etiquetado con el epoch del `Env`.
//!
//! Se llena en el commit "effects dispatcher (mock)".
