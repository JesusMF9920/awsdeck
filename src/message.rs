//! `Message` — resultados que regresan del mundo async, más los DTOs planos que
//! viajan a las vistas (sin tipos del SDK). Viajan envueltos en `Envelope { epoch }`
//! para el descarte de respuestas stale al cambiar de ambiente.
//!
//! Se llena en el commit "action/message/env".
