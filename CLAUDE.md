# CLAUDE.md — awsdeck

**awsdeck** es un TUI en Rust: *"k9s para mi AWS."* Un solo binario, una consola de
terminal para saltar entre los servicios de AWS que uso a diario (CloudWatch Logs, SQS,
Step Functions, EventBridge) con la misma navegación, los mismos keybindings y el
ambiente (cuenta + región) siempre visible y cambiable al instante. Diseño completo en
`ROADMAP.md`.

**Anti-objetivo:** no repetir a `cwtail` (de un solo propósito, sin a dónde crecer). Aquí
`logs` es **solo una vista más** dentro de un shell extensible.

## Principios de diseño (no negociables)

1. **Extensible por defecto.** Agregar un servicio = implementar `View` + registrarlo. El
   core no se entera de qué servicios existen salvo por el registry de vistas.
2. **Async fuera del trait (effects pattern).** Las vistas son síncronas y puras (estado,
   render, teclas). Jamás llaman al SDK. Esto mantiene `View` *object-safe*; **nada de
   `async-trait`**.
3. **El ambiente es estado global de primera clase.** `Env { profile, region }` vive en el
   `App`. Cambiarlo sube un *epoch*, reconstruye clients y refresca la vista activa.
4. **UX consistente = adherencia.** Command bar, filtro, drill/back y ayuda funcionan
   igual en todas las vistas. El header siempre muestra el ambiente activo.
5. **Prod-safe.** Lectura por default. (v0 es solo-lectura; las acciones mutantes llegan en
   fases posteriores detrás de confirm modal + toggle de modo escritura.)

## Flujo de datos (Action → effects → Message)

```
tecla → App.route → View activa.on_key → Vec<Action>
Action → effects.dispatch → tokio::spawn(llamada SDK) → Message (etiquetado con epoch del Env)
Message → mpsc → App.handle_message → (¿epoch vigente?) → View activa.on_message → render
```

- **`View`** — trait síncrono y object-safe. No conoce el SDK ni async.
- **`Action`** (`action.rs`) — intenciones (load/refresh, drill, switch-env, …). Puede tener
  variantes específicas de servicio.
- **`Message`** (`message.rs`) — resultados async (`LogGroupsLoaded`, `LogStreamsLoaded`,
  `Error`, …) + DTOs planos. Puede tener variantes específicas de servicio.
- **`effects.rs`** — el **único** lugar que toca `aws-sdk-*`. Mapea Action → llamada al
  client → Message, y etiqueta cada resultado con el epoch del `Env`.
- **`aws/context.rs`** — `Env` + `AwsContext`/ClientFactory: construye y cachea clients
  tipados por ambiente (lazy/async). Solo vía `aws-config` (profiles de `~/.aws/config`);
  **nunca hardcodear credenciales**.
- **Epoch guard** — cada request en vuelo lleva el epoch del `Env` que lo lanzó; al cambiar
  de ambiente el epoch sube y los `Message` viejos se descartan (nunca pintar datos de la
  cuenta anterior).

## Layout de módulos

```
src/
  main.rs          composition root: arma el registry (registra vistas concretas) y corre el loop
  tui.rs           guard de terminal: raw mode + alt screen, restore en Drop + panic hook
  app.rs           Env+epoch, modos de input, vista activa, routing, status bar (agnóstico de servicio)
  action.rs        enum Action (intents)
  message.rs       enum Message (results) + DTOs (LogGroup, LogStream)
  effects.rs       dispatcher: Action -> task de tokio -> Message  (la frontera con el SDK)
  aws/context.rs   Env + AwsContext/ClientFactory (clients cacheados por ambiente)
  views/
    mod.rs         trait View + Registry genérico (no nombra ningún servicio concreto)
    logs.rs        CloudWatch log groups -> streams (drill, filtro)
  ui/
    header.rs      indicador de ambiente (profile · region) + breadcrumbs
    command_bar.rs `:` comandos y `/` filtro (tui-input)
    help.rs        overlay de ayuda
```

**Core agnóstico:** `app.rs` y `views/mod.rs` no nombran ningún servicio; las vistas
concretas se cablean en `main.rs`; `effects.rs` es la frontera deliberada con el SDK.

## Keybindings (iguales en todas las vistas)

`:` command bar · `/` filtro · `enter` drill · `esc` back · `r` refresh ·
`ctrl-e` cambiar ambiente · `?` ayuda · `q` salir. (`y` copiar ARN/URL — más adelante.)

## Stack

tokio · ratatui + crossterm (feature `event-stream`) · color-eyre · tui-input ·
aws-config + aws-sdk-cloudwatchlogs. **Sin `async-trait`.**

## Estado

v0 en progreso: shell + vista `logs` (mock → SDK real). v1 `sqs`, v2 `sfn`, v3 `events`:
no iniciadas.
