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

- **`View`** — trait síncrono y object-safe. No conoce el SDK ni async. Hooks agnósticos
  para extender sin tocar el core: `on_command` (comandos `:` no-core) y `hints` (pistas de
  teclado contextuales que el footer anuncia).
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
    lambda.rs      funciones -> config + env vars (drill; `l` cross-link a logs; solo lectura)
  ui/
    header.rs      indicador de ambiente (profile · region) + breadcrumbs
    command_bar.rs `:` comandos y `/` filtro (tui-input)
    help.rs        overlay de ayuda
```

**Core agnóstico:** `app.rs` y `views/mod.rs` no nombran ningún servicio; las vistas
concretas se cablean en `main.rs`; `effects.rs` es la frontera deliberada con el SDK.

## Keybindings (iguales en todas las vistas)

`:` command bar · `/` filtro/buscar (fuzzy con ranking; en `logs` groups y tail consulta al server
por subcadena; con `↑/↓` navegas sin salir; `enter` entra directo, sin doble enter) · `enter` drill ·
`esc` back de dos etapas (con filtro lo limpia;
si no, un nivel; desde la raíz, al menú) · `r` refresh · `y` copiar ARN/URL/línea · `O` abrir en
la consola AWS · `ctrl-e` cambiar ambiente (profiles) · `:region <código>` cambiar solo la región ·
`?` ayuda · `q` salir. `enter` también **expande** el contenido completo (línea de log, cuerpo de
mensaje `sqs`, `input` de target `events`, **input/output de un estado `sfn`**) en un panel
scrolleable; `o` **carga más** (tail: ventana ·
Events: líneas viejas · `sfn`: ejecuciones o **history del detalle**). En `logs`: `t` tail del group,
`w/W` ventana, `f` tail en
vivo. En `events` detalle: `P` expande el `event_pattern`. En `sfn`: `:status failed|all` filtra
ejecuciones por estado y `l` (detalle) abre los logs de la Lambda del estado. `*` **marca/quita
favorito** del recurso seleccionado del nivel raíz (★ en el menú; los recientes se trackean solos al
drillear). Acciones mutantes gated
por modo escritura (`:write`)
+ confirm: `p` purgar cola (`sqs`), `d` redrive de DLQ (`sqs`, sobre un dead-letter), `R` redrive
ejecución (`sfn`), `S` enviar evento a un bus (`events`, abre un **form editable** con
source/detail-type/detail/`time`/`resources`). Las teclas
propias de cada vista se **anuncian** en el footer según el contexto
(`View::hints`): no hay que memorizar la tabla ni abrir `?`.

`esc` es navegación uniforme de **dos etapas** (estilo k9s): el `App` lo intercepta en
`on_normal_key` y, si hay filtro aplicado, lo limpia y se queda en la vista (1a etapa, la vista
no ve ese esc); si no hay filtro, lo reenvía a la vista, que despoja un nivel de drill y, en la
raíz, emite `Action::Back` (intención core agnóstica) que el `App` mapea a "volver al menú". La
vista nunca nombra al menú.

## Stack

tokio · ratatui + crossterm (feature `event-stream`) · color-eyre · tui-input · serde_json ·
serde + toml (config) + toml_edit (`:set` preservando comentarios) · arboard (clipboard) · open
(abrir navegador) · aws-config +
aws-sdk-cloudwatchlogs / aws-sdk-sqs / aws-sdk-sfn / aws-sdk-eventbridge / aws-sdk-lambda.
**Sin `async-trait`.**

## Estado

**v0 + v1 + v2 + v3 completos.** Shell + vistas `logs` (`aws-sdk-cloudwatchlogs`), `sqs`
(`aws-sdk-sqs`), `sfn` (`aws-sdk-sfn`), `events` (`aws-sdk-eventbridge`) y `lambda` (`aws-sdk-lambda`)
contra el SDK real, con `Backend::Mock` (`AWSDECK_MOCK=1`) para tests/demo offline. Header con
`profile · region`, command bar (`:logs`/`:sqs`/`:sfn`/`:events`/`:lambda`), filtro (`/`), drill/back,
picker de profiles (`ctrl-e`) con
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
  últimas 200, *newest abajo*; sigue `nextBackwardToken` porque la API puede devolver una página
  vacía con eventos); `esc` vuelve a streams (siguen en cache). **`Tail`** = **logs del group por
  rango de tiempo** (tecla `t`, *sibling* de streams; `filter_log_events` sobre **todos** los
  streams): `esc` vuelve a groups.
  - **Rango configurable de dos formas**: presets con `w`/`W` (15m/1h/6h/24h/3d/7d) y command bar
    `:since 2d` / `:from 2026-06-19[T17:00] [to …]` (UTC), vía el hook agnóstico `View::on_command`
    (el `App` reenvía comandos no-core a la vista). `LogWindow { Last | Range }` plano; el reloj vive
    solo en `effects`. **Completitud**: `effects` auto-pagina la ventana (`MAX_TAIL_PAGES`) y `o`
    carga la siguiente página (append). **Staleness por `generation`** (sube en consulta fresca:
    ventana/patrón/drill; se conserva en load-more) en vez del eco de query.
  - **Expandir una línea**: `enter` sobre un evento abre un panel (interno a la vista, sin overlay)
    con el mensaje **completo** (wrap + scroll `j/k`,`g/G`,`PgUp/Dn`; JSON pretty si parsea); `esc`
    cierra. `clip_message` conserva saltos (cap 16KB); la fila de la lista los colapsa al render.
  - `/` filtra **local** (substring) en `Events` y **server-side** (`filter_pattern`) en `Tail`; color
    por severidad (ERROR rojo, WARN amarillo); señal `· parcial`. Lectura pura: **sin gate** (las
    nuevas `Message` no cambian listados → no re-dispatch). `util::parse_duration`/`parse_datetime`
    (UTC, sin crate de fechas). Mock + SDK real.
- **Descubribilidad de teclas por vista (`View::hints`)**: hook agnóstico del trait — cada vista
  declara sus teclas contextuales como pares `(tecla, qué hace)` según su estado (nivel de drill,
  panel abierto). El `App` se las pide a la vista activa y el footer (`Footer::Hints`) las pinta
  **antes** de los hints globales (lo no-obvio primero; si la fila se desborda, ratatui recorta los
  globales por la derecha, que igual están en `?`). El core nunca las interpreta; con `view` vacío
  el footer queda byte-idéntico al global de siempre. `logs` anuncia `t logs por tiempo` en
  groups/streams (reforzado en el `body_title`: `· t: todos los streams por tiempo`), `w`/`o`/`:since`
  en el tail y `esc` en el detalle de línea; `sqs`/`sfn`/`events` anuncian sus teclas gated
  (`p`/`R`/`S`, `R` solo si la ejecución es redrivable). Hace **descubrible** el tail por tiempo de
  `logs` (antes solo vivía en `?` con texto críptico, ya reescrito).
- **Búsqueda de groups: híbrida (rápida a escala)**: con miles de groups **no se cargan todos**
  (eso bloqueaba segundos). `fetch_log_groups` trae **una página** (≤50, 1 round-trip); `/` busca
  **server-side por subcadena** (`logGroupNamePattern`, infix → `CreateOrder` trae
  `…-CreateOrderV3` sin el prefijo) y el `fuzzy_score` local rankea/refina esos resultados
  (case-insensitive). `last_query` + guard "latest wins" en `LogGroupsLoaded`; `back()` recarga la
  1ª página al volver de una búsqueda.
  - **Tolerancia de casing (fan-out)**: `logGroupNamePattern` es substring *case-sensitive*, así
    que con búsqueda `fetch_log_groups` hace **fan-out**: dispara ≤5 variantes de casing de la query
    (`util::case_variants`: as-typed, lower, UPPER, primera-letra, Title-por-segmento) en paralelo
    (`join_all`), mergea dedup por nombre y OR-ea el `more`. Así `createOrder` encuentra
    `…CreateOrderV3` sin acertar la primera mayúscula. **Limitación**: NO reconstruye CamelCase
    interno desde todo-minúsculas (`createorder` ≠ `CreateOrder`). El mock espeja la misma cobertura.
- **Performance del tail (rangos amplios)**: `logs` cachea los índices filtrados (`filtered`) y
  precomputa por evento lowercase/preview/severidad (`event_rows`), en sync vía
  `set_events`/`extend_events` (únicos puntos de mutación) + `recompute_filtered` (solo al cambiar
  filtro/lista/nivel). Antes recomputaba O(n) con `to_lowercase` por tecla y reconstruía la lista por
  frame sobre hasta 10k líneas; ahora navega fluido.
- **Un solo `enter` desde el filtro** (`app.rs on_filter_key`): `Enter` commitea el filtro y reenvía
  el Enter a la vista (drill) en una pulsación, espejando el reenvío de flechas; el drill emite
  `ClearFilter` (no se arrastra al hijo). Agnóstico.
- **Copiar (`y`) y abrir en consola (`O`)**: `Action::CopyToClipboard{text}` (agnóstica, la maneja el
  `App` con `arboard`) y `Action::OpenConsole{target: ConsoleTarget}` (service-shaped en `action.rs`;
  `effects` arma la URL con la región del `ctx` y abre el navegador con `open`). Cada vista decide qué
  identificador copiar/abrir; hints anuncian `y`/`O`.
- **Tail en vivo (`f`, `tail -f`)**: `View::on_tick` (default no-op) + un `interval` (~3s) en el loop
  del `App` (solo tickea con vista activa en modo normal, sin overlays). `logs` togglea `tail_live`
  con `f`; en cada tick re-consulta la ventana (salta al fondo). Título `[LIVE]`.
- **Config en disco** (`config.rs`): `~/.config/awsdeck/config.toml` (respeta `XDG_CONFIG_HOME`,
  **load-only**, hand-editado) con `default_profile`/`default_region` (fallbacks de `Env` tras el
  entorno) y `default_tail_window` (preset del tail, vía `LogsView::with_default_window`). Si no
  existe/no parsea, defaults.
- **Cross-link `sfn` → logs de la Lambda (`l`)**: en el detalle de una ejecución, `l` sobre un estado
  de tipo Lambda task salta a `logs` y abre el **tail de su log group** acotado a la ventana del
  estado. Mecanismo **agnóstico**: `effects::parse_history` cuelga el ARN de la Lambda de cada estado
  (`StateSpanDto::resource_arn`: integración directa vía `LambdaFunctionScheduled.resource` u
  optimizada `arn:aws:states:::lambda:invoke` vía `TaskScheduled` + `FunctionName` de los parameters);
  `util::lambda_log_group_from_arn` → `/aws/lambda/<fn>`; la vista emite
  `Action::ActivateViewWithContext { id: "logs", context: ViewContext::LogGroupTail { group, window } }`
  y el `App` activa la vista destino llamando `View::on_context` (en vez de `on_activate`) **sin
  inspeccionar** el `ViewContext` (espeja `on_message`/`ConsoleTarget`: el core nunca nombra
  servicios). `logs::on_context` abre el tail directo (`open_group_tail`). Lectura → sin gate; el hint
  `l` solo aparece sobre un estado con Lambda.
- **Robustez contra AWS real (P0)**: lo que hacía frustrante el uso diario contra una cuenta real.
  - **Errores tipados y accionables**: `Message::Error { kind: ErrorKind, detail }` (antes `String`).
    `ErrorKind` (`Auth`/`AccessDenied`/`Throttle`/`Network`/`Other`) **no nombra servicios** → el core
    ramifica sin romper el agnosticismo. `effects::sdk_error` recorre `e.chain()` (el `SdkError`
    esconde el code/message reales en `source()`, no en `{e}`), clasifica y arma un detalle con
    **hint** (Auth → "corre `aws sso login`"). Reemplaza los 15 sitios `format!("op: {e}")`.
  - **Status pegajoso + recuperación**: las teclas de navegación ya no borran un error (`on_key` solo
    limpia el info); se descarta con `esc` (etapa 0, antes del esc de filtro/back), con data fresca
    (`on_envelope`) o al cambiar de ambiente. El header enciende `[re-auth]` ante `ErrorKind::Auth`
    (`App::error_kind`, derivado sin nombrar servicios).
  - **Cuenta confirmada por STS**: `Action::VerifyIdentity` → `Message::IdentityLoaded { account_id }`
    (`get_caller_identity`; mock canned). El header muestra `profile · region · <cuenta>` (prod-safe:
    la cuenta real, no solo el nombre del profile). `App::identity_pending` lo dispara desde el **loop**
    (no `switch_env`) para que los cambios de ambiente queden sincrónicos (sin `tokio::spawn`); el epoch
    guard descarta stale; un fallo de STS clasifica `Auth` (= SSO caducado) y enciende `[re-auth]`.
  - **`:region <código>`** (core agnóstico en `run_command`): cambia solo la región del ambiente actual
    reusando `switch_env` (epoch + clients + recarga); cumple "región cambiable al instante".
  - **Retry adaptativo + timeouts** en el `SdkConfig` (`aws/context.rs`): mitiga throttling del fan-out;
    un endpoint colgado falla como `Network` (reintentable con `r`) en vez de congelar la TUI.
  - **Nudge** al arrancar sin profiles en `~/.aws/config` (antes caía a default en silencio).
- **Lectura y navegación a fondo (P1)**: cerrar los dead-ends de lectura/paginación.
  - **Panel de detalle reusable** (`ui::detail::DetailPanel`): extraído de `logs`, lo usan también
    `events` (input de target con `enter`, `event_pattern` con `P`) y `sqs` (cuerpo de mensaje con
    `enter`). Snapshot del texto al abrir (una recarga detrás no lo invalida), scroll `j/k/g/G`, JSON
    pretty, `y` copia el crudo, `esc` cierra (sin subir de nivel). Cada vista guarda un
    `Option<DetailPanel>` y le delega teclas/render/hints mientras está abierto.
  - **Load-more sin dejar nada inalcanzable**: `sfn` pagina ejecuciones con `o`
    (`Message::ExecutionsLoaded` pasa de `more` a `{ next_token, append }`); `logs` Events trae
    **líneas más viejas** con `o` (`fetch_log_events` devuelve el `next_backward_token`;
    `prepend_events` las antepone y conserva la línea leída). Espeja el load-more del tail.
  - **Filtro de ejecuciones por estado** (`sfn`): `:status failed|all|…` vía `View::on_command` →
    `list_executions().status_filter` server-side (más allá del top-50). Título `· solo FAILED`.
  - **Tail en vivo no arrebata la selección** (`logs`): `on_tick` marca su consulta como `live_refresh`;
    la respuesta respeta la posición del usuario salvo que ya estuviera al fondo (una carga manual sí
    salta al fondo). Antes cada tick (~3s) lo expulsaba al final y leer en vivo era inviable.
  - **Bug**: `parse_datetime` rechaza días fuera del mes (round-trip de `civil_from_days`).
- **Backlog de features cerrado (P2)**: lo que faltaba para que cada vista sea "completa".
  - **`sfn`: input/output por estado** en el timeline: `parse_history` extrae el `input`
    (`StateEntered`) y `output` (`StateExited`) de cada estado a `StateSpanDto` (pretty + truncado);
    `enter` sobre un estado abre el `DetailPanel` con ambos. Antes solo se veía el I/O global.
  - **Paginación acotada + `· parcial`**: `fetch_log_streams` dejó de drenar el paginador sin tope
    (`MAX_STREAM_PAGES`, `LogStreamsLoaded.more`, campo propio `streams_partial`); `get_execution_history`
    dejó de ser una sola llamada de 1000 (`MAX_HISTORY_PAGES` ≈ 10k, `ExecutionDetailLoaded.history_more`,
    `history_partial`). Ambos señalan `· parcial` (espeja `fetch_state_machines`).
  - **`sqs`: redrive de DLQ** (`StartMessageMoveTask`): segunda mutante de sqs. Detección **nativa** —
    `ListDeadLetterSourceQueues` (≥1 cola origen ⇒ ES un DLQ, sin heurística de nombres) → `dlq_sources`
    / `is_dlq()`. `d` emite `Action::RedriveDlq`, reusa el gate; `effects` resuelve el `source_arn`
    (GetQueueAttributes) y dispara `StartMessageMoveTask` (sin destino → vuelven al origen).
  - **`events`: `SendEvent` editable** (`ui::form::Form`, genérico): `S` abre un form multi-campo
    (source/detail-type/detail JSON con defaults); valida el JSON local y emite el payload (gated; el
    confirm lo muestra). Nuevo hook agnóstico **`View::wants_raw_input`** → el `App` reenvía teclas
    crudas a la vista activa (teclear `:`/`{`/`q` en el JSON sin que el core las intercepte).
  - **Config persistente** (`config.rs`): nuevo `State { last_profile, last_region }` que la app
    escribe sola al salir en `state.toml` (aparte del `config.toml` hand-editado, **no-destructivo**).
    `initial_env` cae: entorno > `default_*` del config > último usado > defaults (`pick_env`, pura).
- **Backlog P3 (más usabilidad diaria)**:
  - **`events`: `SendEvent` con `time`/`resources`**: el form gana dos campos opcionales (vacío =
    omitir). `time` reusa `util::parse_datetime` (UTC, sin `chrono`); `resources` = ARNs separados por
    coma. `Action::SendEvent` += `time: Option<i64>` + `resources: Vec<String>`; `send_event_real`
    arma el `PutEventsRequestEntry` condicional (`.time(DateTime::from_millis)`/`.set_resources`); el
    confirm los muestra (prod-safe). Mock los ignora.
  - **`sfn`: load-more del history** (`o` en Detail): `Action::LoadMoreExecutionHistory { page_budget }`
    re-pide describe+history con un presupuesto de páginas mayor y **re-parsea todo** (effects es
    stateless y `parse_history` empareja entry/exit con una pila por nombre → solo un prefijo
    cronológico contiguo es correcto; acumular páginas sueltas rompería el borde). Reusa
    `ExecutionDetailLoaded`; `fetch_execution_detail` toma `page_budget`. La vista sube `history_pages`
    por `HISTORY_PAGE_STEP` y **preserva la selección por nombre** (ya no salta al `failed_state` en el
    load-more). Mock: ejecución `big` con history contiguo que crece con el budget.
  - **Favoritos + recientes** (acceso desde el **menú principal**, recientes **auto-trackeados**):
    máximo agnosticismo — el core trata `view_id`/`key`/`label` como strings opacos y la recencia se
    modela por **posición en un `Vec`** (frente = más reciente, sin reloj). Hook agnóstico
    **`View::selected_favorite() -> Option<(key, label)>`** (default `None`): el `App` lo usa para la
    tecla **`*`** (marca/desmarca) y le pone el `view_id`. **`Action::RecordRecent { key, label }`**
    (core): la vista la emite al drillear a un recurso raíz; el `App` la guarda con el id activo.
    Abrir un favorito reusa el handoff: nueva variante **`ViewContext::Favorite { key }`** →
    `ActivateViewWithContext` → `on_context` de la vista destino (drill directo). Persistido en
    `state.toml` **por ambiente** (`State.environments: Vec<EnvHistory{profile,region,favorites}>`,
    `[[environments]]`; `toggle_favorite`/`record_recent`/`favorites_for`/`prune` toman `(profile,
    región)` y operan sobre el bucket del ambiente; `CAP=50` recientes por bucket; `#[serde(default)]`
    + shim `migrate_legacy` = compat con el `state.toml` plano viejo, que migra solo al último ambiente
    usado). El `App` guarda un `store: State` (inyectado con `load_state`, reescrito al salir). El
    **menú** lista, bajo las herramientas, **★ favoritos** y **recientes** (`MenuRow::{Tool,Header,
    Favorite}`; la navegación salta los headers); `enter` sobre uno lo sube al frente y lo abre por
    contexto. Hint `* favorito` cuando la vista expone un recurso. Cada vista implementa el getter +
    `on_context(Favorite)` + `RecordRecent` al drillear (`logs` group→tail, `sqs` cola→detalle, `sfn`
    máquina→ejecuciones —EXPRESS abierto así muestra el error del SDK—, `events` bus→rules).
  - **Historial por ambiente**: los favoritos/recientes son **por `(profile, región)`** — cada
    ambiente ve recursos distintos, así que tiene su propio historial aislado. `menu_rows` lee el
    bucket del ambiente activo (`favorites_for`), así que `ctrl-e`/`:region` muestran el set correcto
    en el siguiente render; `switch_env` re-ancla la selección del menú (`reanchor_menu_selection`) por
    si apuntaba a un favorito del ambiente anterior. La clave NO usa el `account_id` (async vía STS):
    profile+región es síncrono y siempre conocido. Core agnóstico: el `App` pasa `profile`/`región`
    (campos que ya posee), nunca inspecciona `view_id`/`key`; las vistas no cambian.
- **Backlog de usabilidad (P4)**:
  - **Persistencia instantánea**: `state.toml` ya no se escribe solo al salir; `App::persist_store`
    (stampa el ambiente + `State::save`) corre tras cada cambio del store (`*`, `RecordRecent`, abrir un
    favorito), así un crash no pierde lo de la sesión. En tests el write a disco se compila fuera
    (`#[cfg(not(test))]`) para no tocar el `state.toml` real.
  - **Favoritos en niveles profundos**: `events` marca una rule y `sfn` una ejecución (no solo el
    recurso raíz). La `key` de `ViewContext::Favorite` sigue opaca: cada vista la codifica compuesta
    `padre⟂hijo` (separador `\u{1f}`) en `selected_favorite` y la decodifica en `on_context` para abrir
    directo el `Detail` (`LoadRuleDetail` / `LoadExecutionDetail`). Sin auto-recientes en profundo (son
    explícitos con `*`); sin caveat EXPRESS (a las ejecuciones solo se llega en STANDARD). logs/sqs y el
    core sin cambios.
  - **Presets de evento**: `config.toml` gana `[[event_presets]]` (`Config.event_presets`, primer dato
    no-escalar); `S` abre un chooser (`(en blanco)` + presets) que prellena el form
    (`open_send_form_with`); el chooser reusa `wants_raw_input`. Sin presets → form canned directo.
    `EventsView::with_presets` (cableado en `main`).
  - **`:set <clave> <valor>`**: persiste un default en `config.toml` (`default_profile`/`default_region`/
    `default_tail_window`) **preservando comentarios** (dep `toml_edit`; `Config::apply_setting` pura +
    `write_setting` IO). Solo persiste (efectivo al reabrir); el ambiente vivo lo cambia `:region`. Core
    agnóstico (toca claves de config, no servicios).
- **Vista `lambda`** (`aws-sdk-lambda`): primera vista nueva del backlog. Drill de 2 niveles que
  espeja a `sqs`: funciones (`list_functions`) → config (`get_function`: runtime/handler/memoria/
  timeout/code size/role/tracing/DLQ/layers) + lista navegable de **env vars** (keys + values; `enter`
  expande el valor completo en el `DetailPanel`). **`l`** salta a los logs `/aws/lambda/<fn>` reusando
  el cross-link de `sfn` (`lambda_log_group_from_arn` + `ViewContext::LogGroupTail`). Solo lectura, sin
  gate. Cableado en las fronteras (`aws/context.lambda()`, `action`/`message`/`effects` + mock,
  `main`); DTOs planos (runtime aplanado a String). **Invoke gated queda como fast-follow.**
  Carga **en streaming** (`stream_functions` manda una `Message::FunctionsLoaded { append, more,
  partial }` por página): la 1ª llega tras un solo round-trip → la vista es usable de inmediato y el
  resto se anexa de fondo (`more` mantiene `· cargando…`). Esto resuelve el cuelgue contra cuentas
  grandes (`list_functions` devuelve ≤50/página y no admite filtro server-side por nombre, así que
  antes bloqueaba decenas de round-trips). Tope `MAX_FUNCTION_PAGES=40` (no-bloqueante; si corta,
  `partial` → `· parcial`); la búsqueda es fuzzy local sobre lo cargado.

291 tests sin red (routing, epoch guard, gate de mutaciones —purge, redrive, send y **redrive de DLQ**—, búsqueda de
groups server-side por subcadena + fuzzy local + fan-out de casing ("createOrder" → "CreateOrderV3") +
"latest wins", menú, drill x3 en `sfn`/`events` y
eventos/tail de `logs`, back→menú de dos etapas,
navegación en filtro + `enter` que drillea, preservación de selección, `ClearFilter` al cambiar de
nivel, señal `· parcial`, rango de tiempo del tail —`w`/`o`/`:since`/`:from-to`, staleness por gen,
append—, cache de filtrado invariante, copiar (`y`) y abrir consola (`O`) por vista + URLs de consola,
tail en vivo (`f`/`on_tick`/`[LIVE]`), config (`Config::parse`), expandir línea (JSON pretty), parsers
de fecha/duración, guard EXPRESS, `parse_history` —incluye resource_arn de Lambda—, cross-link
`sfn`→logs (`on_context`/`ActivateViewWithContext`), `lambda_log_group_from_arn`, hints por vista +
footer, render con `TestBackend`, **errores tipados** (`ErrorKind::classify`/`hint`), **status
pegajoso** (sobrevive navegación, se limpia con data fresca, `esc` lo descarta), **`:region`**
(cambia región conservando profile), **STS** (mock `VerifyIdentity`, `IdentityLoaded` puebla la
cuenta, `switch_env` la resetea, header pinta cuenta + `[re-auth]`), `parse_datetime` rechaza días
fuera del mes, **panel de detalle** (`DetailPanel`: cierra con esc/enter, `content()` crudo, JSON
pretty), **expandir en `events`/`sqs`** (input/patrón/cuerpo), **tail en vivo conserva la posición**
salvo al fondo, **load-more** (`sfn` ejecuciones append + `:status` server-side; `logs` Events antepone
líneas viejas conservando la selección), **input/output por estado** (`sfn`: enter abre el panel),
**paginación** (`streams`/`history` señalan `· parcial`), **redrive de DLQ** (`is_dlq` por
`ListDeadLetterSourceQueues`, `d` gated, mock), **form de envío** (`ui::form`: tab/enter/esc, valida
JSON, `wants_raw_input` reenvía teclas crudas), **config persistente** (`State` round-trip,
`pick_env` precedencia entorno > config > último > default), **SendEvent time/resources** (submit
parsea/valida time + resources, confirm los muestra), **load-more del history `sfn`** (`o` sube el
budget, mock `big` crece contiguo, selección preservada por nombre), **favoritos/recientes**
(`toggle`/`record_recent`/`prune` LRU + round-trip + compat v1, `*` marca/quita, `RecordRecent` con id
activo, menú lista fav+recientes y `enter` abre por `ViewContext::Favorite`, getter + `on_context` por
vista), **historial por ambiente** (aislamiento por `(profile, región)`, `prune` por bucket, lectura
no crea bucket, migración del listado plano legacy al último ambiente / a defaults, el menú refleja
solo el ambiente activo, `switch_env` re-ancla la selección), **persistencia instantánea** (`*` stampa
el ambiente en el store), **favoritos profundos** (events rule / sfn ejecución: la key compuesta abre el
`Detail`), **presets de evento** (`S` abre el chooser, prellena el form; sin presets → canned), **`:set`**
(`apply_setting` conserva comentarios y otras claves, agrega clave nueva, round-trip por `Config::parse`,
rechaza clave/TOML inválido), **vista `lambda`** (drill funciones→config + env vars, `enter` expande el
valor, `l` cross-link a logs, favorito por ARN, copiar/consola, render, streaming por páginas
—`append`/`more`/`partial`— con `· cargando…`/`· parcial`)). `AWSDECK_MOCK=1 cargo run` lo
abre sin credenciales.

Pendiente: **Lambda Invoke gated** (`i` + form de payload + respuesta); presets built-in / "guardar
evento actual como preset"; favoritos de streams de logs (efímeros); abrir un favorito EXPRESS de `sfn`
sin el error del SDK; `:set` que también cambie el ambiente vivo. Backlog de vistas: DynamoDB, ECS, RDS, S3…
