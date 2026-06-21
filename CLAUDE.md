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
  message.rs       enum Message (results) + DTOs (LogGroup, Queue, StateMachine, Execution…)
  effects.rs       dispatcher: Action -> task de tokio -> Message  (la frontera con el SDK)
  aws/context.rs   Env + AwsContext/ClientFactory (clients cacheados por ambiente)
  views/
    mod.rs         trait View + Registry genérico (no nombra ningún servicio concreto)
    logs.rs        CloudWatch log groups -> streams -> eventos; tail del group (drill, filtro)
    sqs.rs         colas -> attributes + peek (drill, purge gated)
    sfn.rs         state machines -> ejecuciones -> detalle/timeline (drill x3, redrive gated)
    events.rs      event buses -> rules -> detalle (patrón + targets) (drill x3, send gated)
  ui/
    header.rs      indicador de ambiente (profile · region) + breadcrumbs
    command_bar.rs `:` comandos y `/` filtro (tui-input)
    help.rs        overlay de ayuda
```

**Core agnóstico:** `app.rs` y `views/mod.rs` no nombran ningún servicio; las vistas
concretas se cablean en `main.rs`; `effects.rs` es la frontera deliberada con el SDK.

## Keybindings (iguales en todas las vistas)

`:` command bar · `/` filtro (con `↑/↓` navegas los resultados sin salir) · `enter` drill ·
`esc` back de dos etapas (con filtro lo limpia; si no, un nivel; desde la raíz, al menú) ·
`r` refresh · `ctrl-e` cambiar ambiente · `?` ayuda · `q` salir. Acciones mutantes gated por
modo escritura (`:write`) + confirm: `p` purgar cola (`sqs`), `R` redrive ejecución (`sfn`),
`S` enviar evento de prueba a un bus (`events`). (`y` copiar ARN/URL — más adelante.)

`esc` es navegación uniforme de **dos etapas** (estilo k9s): el `App` lo intercepta en
`on_normal_key` y, si hay filtro aplicado, lo limpia y se queda en la vista (1a etapa, la vista
no ve ese esc); si no hay filtro, lo reenvía a la vista, que despoja un nivel de drill y, en la
raíz, emite `Action::Back` (intención core agnóstica) que el `App` mapea a "volver al menú". La
vista nunca nombra al menú.

## Stack

tokio · ratatui + crossterm (feature `event-stream`) · color-eyre · tui-input · serde_json ·
aws-config + aws-sdk-cloudwatchlogs / aws-sdk-sqs / aws-sdk-sfn / aws-sdk-eventbridge. **Sin
`async-trait`.**

## Estado

**v0 + v1 + v2 + v3 completos.** Shell + vistas `logs` (`aws-sdk-cloudwatchlogs`), `sqs`
(`aws-sdk-sqs`), `sfn` (`aws-sdk-sfn`) y `events` (`aws-sdk-eventbridge`) contra el SDK real, con
`Backend::Mock` (`AWSDECK_MOCK=1`) para tests/demo offline. Header con `profile · region`, command
bar (`:logs`/`:sqs`/`:sfn`/`:events`), filtro (`/`), drill/back, picker de profiles (`ctrl-e`) con
epoch guard, selección de ambiente al iniciar si no hay `AWS_PROFILE` (`start_with_env_picker`),
ayuda (`?`), status bar de errores.

**v1 `sqs`:** lista colas (badge `[fifo]`), drill a attributes (visible/in-flight/delayed/DLQ)
+ *peek* de mensajes (receive sin borrar, best-effort). Primera acción mutante **`PurgeQueue`**
detrás del **gate prod-safe**: modo escritura (`:write`, badge rojo `[ESCRITURA]`) + confirm
modal (`ui/confirm.rs`); el gate vive en `App::dispatch` (`is_mutating` → `request_confirm`) y
es reusable para v2/v3. `switch_env` resetea el modo escritura.

**v2 `sfn`:** primera vista de **3 niveles** (`Level::{Machines,Executions,Detail}`, cada uno
carga los identificadores que `back()` reconstruye y `on_message` valida). L1 `list_state_machines`
(badge tipo + fecha). L2 `list_executions` con status coloreado + duración; **guard EXPRESS** (la
vista no emite `LoadExecutions` para máquinas EXPRESS → muestra nota, evita el error del SDK). L3
`describe_execution` + `get_execution_history` combinados en un Message: input/output (pretty,
truncado a 4KB), error/cause y **timeline de estados con duración** (`effects::parse_history`
empareja StateEntered/StateExited por nombre, pura/testeada), resaltando y preseleccionando el
estado que reventó. **`RedriveExecution`** (`R`) reusa el mismo gate prod-safe; la vista solo la
ofrece si `ExecStatus::is_redrivable()`. DTOs con enums propios planos (`MachineType`/`ExecStatus`,
no los `#[non_exhaustive]` del SDK); `DateTime → millis` vía `.to_millis().ok()` en effects.

**v3 `events`:** segunda vista de **3 niveles** (`Level::{Buses,Rules,Detail}`), espeja `sfn`. L1
`list_event_buses` (paginado). L2 `list_rules` con badge de estado (`[enabled]`/`[disabled]`
coloreado) + descripción. L3 combina `describe_rule` + `list_targets_by_rule` en un Message: render
partido **meta / patrón / targets** — el `event_pattern` (pretty, truncado) queda inspeccionable y
los **targets** son la lista navegable/filtrable. `/` filtra en los 3 niveles (buses/rules/targets);
`ClearFilter` al cambiar de nivel; señal `· parcial`. **`SendEvent`** (`S` sobre el bus) publica un
evento de prueba **canned** (`source=awsdeck.manual`) reusando el gate prod-safe; en éxito muestra
"evento enviado", y un `put_events` con `failed_entry_count>0` se traduce a error en la status bar.
`RuleState` enum propio plano (`_ => Enabled`); sin timestamps (EventBridge no los expone en
list/describe).

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
- **`esc` de dos etapas (estilo k9s)**: el `App` intercepta `esc` en `on_normal_key`; con filtro
  aplicado lo limpia (`clear_filter` + `fire_search_now`, recarga sin filtro en logs) y se queda
  en la vista; sin filtro lo reenvía a la vista (drill back / `Action::Back` → menú).
- **Navegar dentro del filtro**: en `Mode::Filter`, `↑/↓` se reenvían a la vista activa (mueven
  su selección sobre la lista ya filtrada) sin salir del filtro ni editar el texto (estilo fzf);
  el resto de teclas siguen editando el `tui-input`.
- **La recarga async no pisa la navegación** (`logs`): `on_message(LogGroupsLoaded)` preserva la
  selección **por nombre** (`restore_selection`) en vez de forzar `select(Some(0))`. Como
  `set_filter` ya pone la selección en el mejor match al teclear, esto conserva ese baseline
  cuando no navegaste y respeta tu posición (flechas en filtro) cuando sí; cae al tope si el item
  desapareció. Aplica también al `r` refresh. (SQS ya solo hacía `clamp_selection`.)
- **Listas `sfn` sin truncado silencioso**: las máquinas se **paginan** (`fetch_state_machines`
  sigue `next_token`, tope `MAX_MACHINE_PAGES=20` → todas alcanzables por el fuzzy); las
  ejecuciones traen las 50 más recientes y marcan `· parcial (recientes)` si hay `next_token`.
  `Message::{StateMachinesLoaded,ExecutionsLoaded}` llevan `more`; el título muestra `· parcial`
  (espeja el patrón de `logs`).
- **El filtro no se arrastra entre niveles** (`Action::ClearFilter`): intención core agnóstica
  (como `Action::Back`) que la vista emite al drillear/back; el `App` —dueño del filtro— la
  ejecuta (`clear_filter` + `fire_search_now`, recarga sin filtro en server-side, no-op en
  client-side). Cablada en `logs`/`sqs`/`sfn`; `logs` además recarga la página completa al volver
  a groups si veníamos de una búsqueda server-side (el cache estaba acotado a esa query).
- **`/` filtra en los 3 niveles de `sfn`**: el timeline de `Detail` también se filtra por nombre
  de estado (`filtered_history_indices`), útil en histories largos (Map/Parallel); la
  preselección del estado fallido se mapea a su posición en la lista visible.
- **Contenido de los logs (`logs` cierra el ciclo de `cwtail`)**: el `Level` de `logs` crece a 4.
  **`Events`** (3er nivel): `enter` en un stream → sus líneas más recientes (`get_log_events`, las
  últimas 200, *newest abajo*); `esc` vuelve a streams (siguen en cache, no recarga). **`Tail`**
  (tecla `t` sobre un group, *sibling* de streams): `filter_log_events` sobre **todos** los streams
  del group en la última hora; `esc` vuelve a groups. `/` filtra **local** (substring) en `Events`
  y **server-side** (`filter_pattern`, reusa `search`/`last_query` latest-wins) en `Tail`; color por
  severidad (ERROR rojo, WARN amarillo); señal `· parcial`. DTO `LogEventDto` plano (`ts`/`message`/
  `stream`); `stream: Some` solo en el tail. Lectura pura: **sin gate, sin tocar `app.rs`** (las
  nuevas `Message` no cambian listados → no re-dispatch). Mock + SDK real; `effects` acota la ventana
  del tail con `SystemTime` (la vista nunca ve relojes) y recorta líneas (`clip_message`, 2KB).

134 tests sin red (routing, epoch guard, gate de mutaciones —purge, redrive y send—, fuzzy, menú,
búsqueda/staleness, drill x3 en `sfn`/`events` y eventos/tail de `logs`, back→menú de dos etapas,
navegación en filtro, preservación de selección, `ClearFilter` al cambiar de nivel, señal `· parcial`,
filtro del timeline/targets/eventos, guard EXPRESS, `parse_history`, parsers, render con
`TestBackend`). `AWSDECK_MOCK=1 cargo run` lo abre sin credenciales.

Pendiente: `SendEvent` con payload editable (form multi-campo; v3 envía un evento canned), tail en
vivo (`tail -f`) y ver una línea completa expandida en `logs` (hoy es carga puntual + `r`),
input/output por estado en el timeline de `sfn`, `y` (copiar ARN), abrir en consola (`o`), config en
disco. Backlog de vistas: Lambda, DynamoDB, ECS…
