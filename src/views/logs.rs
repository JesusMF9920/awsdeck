//! Vista `logs`: CloudWatch Log Groups -> drill a Log Streams. Pura y síncrona:
//! mantiene estado (groups/streams), filtra, hace drill/back y dibuja. NUNCA
//! importa `aws-sdk-*`; recibe datos vía `on_message` (DTOs planos).
//!
//! Se llena en el commit "logs view (mock)".
