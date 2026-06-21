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

`:` command bar · `/` filtro (con `↑/↓` navegas los resultados sin salir) · `enter` drill ·
`esc` back (un nivel; desde la raíz de la vista, al menú) · `r` refresh ·
`ctrl-e` cambiar ambiente · `?` ayuda · `q` salir. (`y` copiar ARN/URL — más adelante.)

`esc` es navegación uniforme: despoja un nivel de drill y, en la raíz, la vista emite
`Action::Back` (intención core agnóstica) que el `App` mapea a "volver al menú". La vista
nunca nombra al menú.

## Stack

tokio · ratatui + crossterm (feature `event-stream`) · color-eyre · tui-input ·
aws-config + aws-sdk-cloudwatchlogs. **Sin `async-trait`.**

## Estado

**v0 + v1 completos.** Shell + vistas `logs` (`aws-sdk-cloudwatchlogs`) y `sqs`
(`aws-sdk-sqs`) contra el SDK real, con `Backend::Mock` (`AWSDECK_MOCK=1`) para tests/demo
offline. Header con `profile · region`, command bar (`:logs`/`:sqs`), filtro (`/`),
drill/back, picker de profiles (`ctrl-e`) con epoch guard, selección de ambiente al iniciar si
no hay `AWS_PROFILE` (`start_with_env_picker`), ayuda (`?`), status bar de errores.

**v1 `sqs`:** lista colas (badge `[fifo]`), drill a attributes (visible/in-flight/delayed/DLQ)
+ *peek* de mensajes (receive sin borrar, best-effort). Primera acción mutante **`PurgeQueue`**
detrás del **gate prod-safe**: modo escritura (`:write`, badge rojo `[ESCRITURA]`) + confirm
modal (`ui/confirm.rs`); el gate vive en `App::dispatch` (`is_mutating` → `request_confirm`) y
es reusable para v2/v3. `switch_env` resetea el modo escritura.

**Pulido (UX a escala):**
- **Menú principal** como pantalla de inicio (`Screen::{Menu,View}` en `App`): lista las
  herramientas (`Registry::metas` → `id` + `View::description`), navegación vim, `enter` abre,
  `:menu`/backspace vuelve. Nuevas vistas aparecen solas. Tras el picker de ambiente se aterriza
  en el menú (ya no auto-activa logs).
- **Búsqueda fuzzy** (`util::fuzzy_score` + `util::ranked`): subsecuencia con ranking en `logs` y
  `sqs` ("ordapi" encuentra "orders-api"); reemplaza el `contains`.
- **Logs a escala**: no carga todos los groups. `LoadLogGroups { query }` trae una página
  acotada (`describe_log_groups().limit(50)`); con query usa `log_group_name_pattern` (substring
  server-side). `View::search` + debounce ~280ms en el loop (`sleep_until`): cada tecla re-rankea
  local y, al parar, consulta al server. `last_query` descarta respuestas viejas (latest wins).
- **`esc` vuelve al menú** desde la raíz de una vista (antes era no-op): la vista emite
  `Action::Back` y el `App` hace `go_home()`. Drill back interno sigue consumiéndose en la vista.
- **Navegar dentro del filtro**: en `Mode::Filter`, `↑/↓` se reenvían a la vista activa (mueven
  su selección sobre la lista ya filtrada) sin salir del filtro ni editar el texto (estilo fzf);
  el resto de teclas siguen editando el `tui-input`.
- **La recarga async no pisa la navegación** (`logs`): `on_message(LogGroupsLoaded)` preserva la
  selección **por nombre** (`restore_selection`) en vez de forzar `select(Some(0))`. Como
  `set_filter` ya pone la selección en el mejor match al teclear, esto conserva ese baseline
  cuando no navegaste y respeta tu posición (flechas en filtro) cuando sí; cae al tope si el item
  desapareció. Aplica también al `r` refresh. (SQS ya solo hacía `clamp_selection`.)

61 tests sin red (routing, epoch guard, gate de mutaciones, fuzzy, menú, búsqueda/staleness,
drill, back→menú, navegación en filtro, preservación de selección, parsers, render con
`TestBackend`). `AWSDECK_MOCK=1 cargo run` lo abre sin credenciales.

Pendiente: v2 `sfn`, v3 `events` (no iniciadas), eventos de log (3er nivel en `logs`), `y`
(copiar ARN), abrir en consola (`o`), config en disco.
