//! `App` — estado global (Env activo + epoch), modos de input, vista activa,
//! routing de teclas, status bar y el loop `tokio::select!` (teclado + canal de
//! mensajes). Agnóstico de servicio: solo conoce vistas a través del registry.
//!
//! Se llena en el commit "app loop + ui shell".
