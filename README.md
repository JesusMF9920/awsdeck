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

Cambia de ambiente en vivo con `ctrl-e`. Si un profile no tiene credenciales válidas, el error
se muestra en la **status bar** (no crashea).

## Keybindings

| Tecla | Acción |
|-------|--------|
| `:` | command bar (saltar de herramienta, p. ej. `:logs`, `:sqs`, `:sfn`, `:events`) |
| `/` | buscar (fuzzy; en `logs` consulta al servidor; `↑`/`↓` navegan los resultados sin salir) |
| `enter` | drill al detalle (en `logs`: group → stream → **eventos**); sobre una **línea**, la **expande** completa |
| `esc` | con filtro aplicado lo limpia (1er `esc`); si no, vuelve un nivel (drill back; en la raíz, al menú) |
| `:menu` · `backspace` | volver al menú principal |
| `j` / `k` · `↑` / `↓` · `g` / `G` | navegar (y scrollear el panel de detalle) |
| `r` | refrescar |
| `t` | **logs del group** (`logs`): todos sus streams **por rango de tiempo** (`/` filtra server-side) |
| `w` / `W` | `logs`: ciclar la **ventana de tiempo** (15m · 1h · 6h · 24h · 3d · 7d) |
| `o` | `logs`: **cargar más** líneas (paginación de la ventana) |
| `:since` · `:from`/`to` | `logs`: rango — `:since 2d` · `:from 2026-06-19 [to 2026-06-20]` (UTC) |
| `p` | purgar cola SQS (gated: modo escritura + confirm) |
| `R` | redrive ejecución `sfn` fallida (gated: modo escritura + confirm) |
| `S` | enviar evento de prueba a un bus `events` (gated: modo escritura + confirm) |
| `:write` | alternar modo escritura (habilita acciones mutantes) |
| `ctrl-e` | cambiar de ambiente (picker de profiles) |
| `?` | ayuda |
| `q` | salir |

> Las teclas específicas de cada vista (`t`/`w`/`o` en `logs`, `p`/`R`/`S` gated) se **anuncian
> solas** en el footer según dónde estés, y `logs` además recuerda `t` en el título del group:
> no hace falta memorizar esta tabla ni abrir `?`.

## Cómo probar los cambios

```bash
AWSDECK_MOCK=1 cargo run    # ver el TUI con datos, sin tocar AWS
cargo test                  # 157 tests, sin red
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
   (15m…7d), `:since 2d` / `:from … [to …]` la fijan (UTC), `o` carga más y `/` filtra server-side.
   `enter` sobre una **línea** la **expande** completa (wrap + scroll, JSON pretty); `esc` cierra.
3. En `sfn`, `enter` entra a una state machine → sus **ejecuciones con status coloreado** y duración;
   `enter` en una FAILED → detalle con input/output, error/cause y el **timeline de estados** (el que
   reventó va resaltado y preseleccionado). En una máquina `[express]` se muestra una nota (sus
   ejecuciones viven en CloudWatch Logs). Con `:write`, `R` hace **redrive** de una ejecución fallida
   (confirm modal).
4. En `events`, `enter` entra a un event bus → sus **rules** con estado `[enabled]`/`[disabled]`;
   `enter` en una rule → detalle partido **meta / patrón / targets** (el `event_pattern` queda visible
   y los targets se filtran con `/`). Con `:write`, `S` sobre un bus **envía un evento de prueba**
   (confirm modal).
5. `ctrl-e` abre el picker; elige otro profile → el ambiente y la lista cambian.
6. `?` muestra la ayuda; `q` sale y la terminal queda limpia.

**Epoch guard:** al cambiar de ambiente con un request en vuelo, nunca se pintan datos de la
cuenta anterior (probado en `app::tests::epoch_guard_discards_stale_and_accepts_fresh`).

**Escala (logs):** con miles de log groups, `logs` no los carga todos — trae una página (≤50)
y `/` consulta al servidor por substring (`logGroupNamePattern`, debounced ~280ms), rankeando
los resultados con fuzzy local. El título indica `· parcial` cuando hay más en el servidor. Los
**logs del group** se traen por **rango de tiempo** (`w`/`:since`/`:from-to`) y se paginan en
demanda (`o`); el reloj de la ventana vive solo en `effects` (la vista nunca lo ve).

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

- **v0** ✅ shell + `logs` (CloudWatch): groups → streams → **eventos** (`get_log_events`) +
  **logs del group por rango de tiempo** (`filter_log_events`, `t`; `w`/`:since`/`:from-to`,
  paginación `o`, filtro server-side) + **expandir una línea** (`enter`, JSON pretty).
- **v1** ✅ `sqs` — colas, attributes, *peek*, `PurgeQueue` (gated por modo escritura + confirm).
- **v2** ✅ `sfn` — state machines, ejecuciones (status coloreado), timeline de estados con duración,
  `Redrive` (gated).
- **v3** ✅ `events` — event buses, rules (estado coloreado), detalle con patrón + targets,
  `SendEvent` (gated).

Backlog: `SendEvent` con payload editable, copiar ARN (`y`), abrir en consola (`o`), config en disco,
más vistas (Lambda, DynamoDB, ECS…).

## Stack

`tokio` · `ratatui` + `crossterm` · `color-eyre` · `tui-input` · `aws-config` +
`aws-sdk-cloudwatchlogs` / `aws-sdk-sqs` / `aws-sdk-sfn` / `aws-sdk-eventbridge` · `serde_json`.
