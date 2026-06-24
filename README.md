# awsdeck

**k9s, pero para mi AWS.** Un TUI en Rust: un solo binario que abre una consola de terminal
para saltar entre los servicios de AWS que uso a diario —CloudWatch Logs hoy; SQS, Step
Functions y EventBridge en camino— con la misma navegación, los mismos keybindings y el
ambiente (cuenta + región) siempre visible y cambiable al instante.

> Estado: **v0 + v1 + v2 + v3** — el shell extensible + las vistas `logs` (CloudWatch), `sqs` (colas,
> peek, purge gated), `sfn` (Step Functions: ejecuciones, timeline, redrive gated) y `events`
> (EventBridge: buses, rules, patrón + targets, send gated).
> Diseño completo en [`ROADMAP.md`](ROADMAP.md); notas de arquitectura en [`CLAUDE.md`](CLAUDE.md).

## Requisitos

- **Rust** reciente (edición 2024 → toolchain 1.85+). Instala con [rustup](https://rustup.rs).
- Para datos reales: **credenciales AWS** en `~/.aws/config` (profiles o SSO). Si usas SSO,
  corre `aws sso login` antes. Es **solo lectura**.

## Correr

```bash
# Contra tu AWS real (usa el profile/region activos)
cargo run

# Demo/QA con datos falsos en memoria — sin red ni credenciales
AWSDECK_MOCK=1 cargo run
```

**Al iniciar**, si **no** fijaste `AWS_PROFILE`, aparece un selector con los profiles de
`~/.aws/config` para elegir con cuál trabajar (preselecciona el default; `enter` confirma,
`esc` usa el default). Si fijaste `AWS_PROFILE=algún-profile`, arranca directo a esa cuenta sin
preguntar. La región sale de `AWS_REGION` o del profile (default `us-east-1`).

Cambia de ambiente en vivo con `ctrl-e` (profiles) o `:region <código>` (solo la región, mismo
profile). El header confirma la **cuenta real** vía STS (`profile · region · 123456789012`), no
solo el nombre del profile. Si las credenciales caducaron o falta un permiso, el error es
**accionable y pegajoso**: la status bar dice qué hacer (`sesión caducada — corre aws sso login…`)
y enciende un `[re-auth]` en el header; no se borra al navegar (se descarta con `esc` o al llegar
data fresca). Nunca crashea.

## Keybindings

| Tecla | Acción |
|-------|--------|
| `:` | command bar (saltar de herramienta, p. ej. `:logs`, `:sqs`, `:sfn`, `:events`) |
| `/` | buscar (en `logs` consulta al server por subcadena + fuzzy local, tolerante a mayús/minús; `↑`/`↓` navegan sin salir; `enter` entra directo) |
| `enter` | drill al detalle (en `logs`: group → stream → **eventos**); **expande** completo: una línea de log, el cuerpo de un mensaje (`sqs`), el `input` de un target (`events`) o el **input/output de un estado** (`sfn`) en un panel scrolleable (JSON pretty, `y` copia) |
| `esc` | con filtro aplicado lo limpia (1er `esc`); si no, vuelve un nivel (drill back; en la raíz, al menú) |
| `:menu` · `backspace` | volver al menú principal (lista las herramientas + **★ favoritos** + **recientes**; `enter` salta directo al recurso) |
| `j` / `k` · `↑` / `↓` · `g` / `G` | navegar (y scrollear el panel de detalle) |
| `r` | refrescar |
| `y` | copiar el ARN/URL/línea del item seleccionado al portapapeles |
| `O` | abrir el recurso seleccionado en la consola web de AWS |
| `t` | **logs del group** (`logs`): todos sus streams **por rango de tiempo** |
| `w` / `W` | `logs`: ciclar la **ventana de tiempo** (15m · 1h · 6h · 24h · 3d · 7d) |
| `o` | **cargar más** — tail: paginar la ventana · Events: líneas más viejas del stream · `sfn`: más ejecuciones, o más **history** en el detalle |
| `*` | marcar/quitar **favorito** del recurso seleccionado (★ en el menú; recientes auto al drillear; historial **por ambiente**; también niveles profundos: una rule de `events` o una ejecución de `sfn`) |
| `f` | `logs`: **tail en vivo** (`tail -f`) — auto-refresca **sin arrastrarte al fondo** si estás leyendo arriba |
| `:since` · `:from`/`to` | `logs`: rango — `:since 2d` · `:from 2026-06-19 [to 2026-06-20]` (UTC) |
| `P` | `events` (detalle): expandir el **event_pattern** completo (scroll + copia) |
| `:status` | `sfn`: filtrar ejecuciones por estado server-side (`:status failed` · `:status all`) |
| `l` | `sfn` (detalle): abrir los **logs de la Lambda** del estado seleccionado (cross-link a `logs`) |
| `p` | purgar cola SQS (gated: modo escritura + confirm) |
| `d` | `sqs` (detalle de un **DLQ**): redrive — reenvía los mensajes a sus colas origen (gated) |
| `R` | redrive ejecución `sfn` fallida (gated: modo escritura + confirm) |
| `S` | `events`: si hay **presets** en `config.toml`, un chooser; luego un **form editable** (source/detail-type/detail JSON + `time`/`resources`) que publica el evento (gated) |
| `:write` | alternar modo escritura (habilita acciones mutantes) |
| `ctrl-e` | cambiar de ambiente (picker de profiles) |
| `:region` | cambiar **solo la región** del ambiente actual (mismo profile), p. ej. `:region eu-west-1` |
| `:set` | persistir un default en `config.toml` (`default_profile`/`default_region`/`default_tail_window`; conserva comentarios) |
| `?` | ayuda |
| `q` | salir |

> Las teclas específicas de cada vista (`t`/`w`/`o`/`f` en `logs`, `y`/`O`/`*`, `p`/`d`/`R`/`S` gated)
> se **anuncian solas** en el footer según dónde estés, y `logs` además recuerda `t` en el título del
> group: no hace falta memorizar esta tabla ni abrir `?`.

## Cómo probar los cambios

```bash
AWSDECK_MOCK=1 cargo run    # ver el TUI con datos, sin tocar AWS
cargo test                  # 251 tests, sin red
cargo clippy --all-targets  # lint
cargo fmt --check           # formato
```

Recorrido rápido (con `AWSDECK_MOCK=1 cargo run`):

1. Arranca en el **menú principal**; `j`/`k` + `enter` para abrir una herramienta (`logs`, `sqs`).
   `:menu` o `backspace` vuelven al menú.
2. En `logs`/`sqs`, `/` **busca fuzzy** (p. ej. `ordapi` encuentra `orders-api`) y dentro del
   filtro `↑`/`↓` navegan los resultados sin tener que salir; `enter` hace **drill** al detalle.
   `esc` es de **dos etapas** (estilo k9s): con un filtro aplicado lo limpia primero; el siguiente
   `esc` regresa un nivel (y desde la raíz de la vista, al menú). En `logs`, `enter` en un stream
   abre sus **líneas** (`get_log_events`, newest abajo, `ERROR` en rojo) y `t` sobre un group abre
   los **logs del group por rango de tiempo** (todos sus streams): `w`/`W` ciclan la ventana
   (15m…7d), `:since 2d` / `:from … [to …]` la fijan (UTC), `o` carga más, `f` sigue en vivo (sin
   arrastrarte al fondo) y `/` filtra server-side. En un stream, `o` trae **líneas más viejas**.
   `enter` sobre una **línea** la **expande** completa (wrap + scroll, JSON pretty, `y` copia); `esc` cierra.
   En `sqs`, `enter` sobre un mensaje del peek expande su **cuerpo completo** (no los 60 chars de la fila).
3. En `sfn`, `enter` entra a una state machine → sus **ejecuciones con status coloreado** y duración;
   `o` trae **más ejecuciones** (ninguna queda inalcanzable) y `:status failed` filtra server-side.
   `enter` en una FAILED → detalle con input/output, error/cause y el **timeline de estados** (el que
   reventó va resaltado y preseleccionado). Sobre un estado de tipo Lambda, `l` salta a **`logs`** y
   abre el tail de la Lambda en la ventana del estado. En una máquina `[express]` se muestra una nota
   (sus ejecuciones viven en CloudWatch Logs). Con `:write`, `R` hace **redrive** de una ejecución
   fallida (confirm modal).
4. En `events`, `enter` entra a un event bus → sus **rules** con estado `[enabled]`/`[disabled]`;
   `enter` en una rule → detalle partido **meta / patrón / targets**. `enter` sobre un target expande
   su `input` completo y `P` el **event_pattern** completo (panel scrolleable, `y` copia); los targets
   se filtran con `/`. Con `:write`, `S` sobre un bus **envía un evento de prueba** (confirm modal).
5. `ctrl-e` abre el picker; elige otro profile → el ambiente y la lista cambian.
6. `?` muestra la ayuda; `q` sale y la terminal queda limpia.

**Epoch guard:** al cambiar de ambiente con un request en vuelo, nunca se pintan datos de la
cuenta anterior (probado en `app::tests::epoch_guard_discards_stale_and_accepts_fresh`).

**Escala (logs):** con miles de log groups **no se cargan todos** (eso bloqueaba segundos). Se trae
**una página** (≤50, 1 round-trip) y `/` **busca server-side por subcadena** (`logGroupNamePattern`,
infix, debounced ~280ms): teclear `CreateOrder` trae `…-CreateOrderV3` sin escribir el prefijo. Como
ese patrón es *case-sensitive*, la búsqueda hace **fan-out** de ≤5 variantes de casing en paralelo
(`createOrder` también encuentra `…CreateOrderV3`; no reconstruye CamelCase desde todo-minúsculas). El
`fuzzy` local rankea/refina lo devuelto; el título marca `· parcial` si hay más en el server. Los
**logs del group** se traen por **rango de tiempo** (`w`/`:since`/`:from-to`), se paginan en demanda
(`o`) y siguen en vivo con `f` (`tail -f`); el reloj vive solo en `effects`. El tail no recomputa el
filtro por tecla ni reconstruye la lista por frame (cache + display precomputado), así que un rango
amplio (p. ej. 14 días) navega fluido.

**Escala (sfn):** las state machines se **paginan** (se traen todas, alcanzables por el fuzzy);
las ejecuciones muestran las 50 más recientes y marcan `· parcial (recientes)` si hay más. El
filtro **no se arrastra** al drillear (estilo k9s: cada nivel arranca limpio), y `/` filtra en
los 3 niveles, incluido el timeline del detalle (por nombre de estado).

## Arquitectura (resumen)

```
tecla → App.route → View activa.on_key → Vec<Action>
Action → effects.dispatch → tokio::spawn(SDK) → Message (con epoch del Env)
Message → mpsc → App (¿epoch vigente?) → View.on_message → render
```

- **`View`**: trait síncrono y *object-safe*, sin `async-trait`. Las vistas son puras y **no**
  importan `aws-sdk-*`; reciben datos por `on_message` (DTOs planos) → testeables sin red.
- **`effects.rs`**: única frontera con el SDK (`Backend::{Mock, Real}`).
- **Core agnóstico**: `app.rs` y `views/mod.rs` no nombran servicios; las vistas concretas se
  registran en `main.rs`. Agregar un servicio = una `views/foo.rs` + variantes en
  `action`/`message` + un brazo en `effects` + una línea en `main`.
- **Ambiente con epoch**: cambiar de cuenta/región sube un epoch y descarta respuestas stale.

Más detalle en [`CLAUDE.md`](CLAUDE.md).

## Roadmap

- **v0** ✅ shell + `logs` (CloudWatch): groups (búsqueda server-side + fuzzy local) → streams → **eventos**
  (`get_log_events`) + **logs del group por rango de tiempo** (`filter_log_events`, `t`;
  `w`/`:since`/`:from-to`, paginación `o`, **tail en vivo** `f`) + **expandir una línea** (`enter`,
  JSON pretty).
- **v1** ✅ `sqs` — colas, attributes, *peek*, `PurgeQueue` (gated por modo escritura + confirm).
- **v2** ✅ `sfn` — state machines, ejecuciones (status coloreado), timeline de estados con duración,
  `Redrive` (gated), **cross-link `l` → logs de la Lambda** de una ejecución.
- **v3** ✅ `events` — event buses, rules (estado coloreado), detalle con patrón + targets,
  `SendEvent` (gated).
- **Transversal** ✅ copiar ARN/URL (`y`), abrir en consola AWS (`O`), config en disco
  (`~/.config/awsdeck/config.toml`), búsqueda de groups por subcadena sin prefijo y **tolerante a
  casing** (fan-out server-side + fuzzy local, sin cargar todo), un solo `enter` desde el filtro,
  **handoff entre vistas con contexto** (`ActivateViewWithContext`/`View::on_context`).
- **Robustez AWS real (P0)** ✅ errores del SDK **tipados** (`ErrorKind`) con la causa real
  (SSO/credenciales caducadas, AccessDenied, throttling) y **pista accionable pegajosa** + `[re-auth]`
  en el header; **cuenta confirmada por STS** en el header (prod-safe); **`:region`** para cambiar de
  región sin tocar `~/.aws/config`; **retry adaptativo + timeouts** en el `SdkConfig`.
- **Lectura y navegación a fondo (P1)** ✅ **panel de detalle reusable** (`ui::detail`) para expandir
  contenido completo (línea de log, cuerpo de mensaje `sqs`, `input`/`event_pattern` de `events`) con
  scroll + JSON pretty + copia; **load-more** sin dejar nada inalcanzable (ejecuciones `sfn`, líneas
  viejas por stream en `logs`); **filtro de ejecuciones por estado** (`:status`); y el **tail en vivo
  ya no arrastra la selección** al fondo mientras lees.
- **Backlog de features (P2)** ✅ **input/output por estado** en el timeline de `sfn` (`enter`);
  **paginación acotada** de log streams y del history de `sfn` con señal `· parcial`; **redrive de
  DLQ** en `sqs` (`d`, detección nativa por `ListDeadLetterSourceQueues`, gated); **`SendEvent` con
  payload editable** (`events`, `S` abre un form multi-campo que valida el JSON; nuevo hook agnóstico
  `View::wants_raw_input`); **config persistente** (recuerda el último ambiente en `state.toml`, sin
  tocar el `config.toml` hand-editado).
- **Más usabilidad diaria (P3)** ✅ **`SendEvent` con `time`/`resources`** (campos opcionales del form,
  fechas UTC sin `chrono`); **load-more del history de `sfn`** (`o` en el detalle: re-fetch + re-parse
  con presupuesto creciente, conservando la selección); **favoritos + recientes** desde el menú
  principal (`*` marca un recurso, los recientes se trackean solos al drillear; agnóstico vía
  `View::selected_favorite` + `ViewContext::Favorite`; persistidos en `state.toml` **por ambiente**:
  cada `(profile, región)` tiene su propio historial, así que cambiar de ambiente muestra el set
  correcto; un `state.toml` plano viejo migra solo).
- **Más usabilidad diaria (P4)** ✅ **persistencia instantánea** (los favoritos/recientes se escriben
  tras cada cambio, no solo al salir); **favoritos en niveles profundos** (`*` sobre una rule de
  `events` o una ejecución de `sfn`, no solo el recurso raíz; key compuesta opaca que abre el detalle
  directo); **presets de evento** (`[[event_presets]]` en `config.toml`: `S` ofrece un chooser que
  prellena el form); **`:set <clave> <valor>`** (persiste un default en `config.toml` preservando
  comentarios, vía `toml_edit`).

Backlog: presets built-in / guardar el evento actual como preset; favoritos de streams de logs
(efímeros); más vistas (Lambda, DynamoDB, ECS…).

## Stack

`tokio` · `ratatui` + `crossterm` · `color-eyre` · `tui-input` · `aws-config` +
`aws-sdk-cloudwatchlogs` / `aws-sdk-sqs` / `aws-sdk-sfn` / `aws-sdk-eventbridge` · `serde_json`.
