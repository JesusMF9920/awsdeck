# awsdeck

**k9s, pero para mi AWS.** Un TUI en Rust: un solo binario que abre una consola de terminal
para saltar entre los servicios de AWS que uso a diario —CloudWatch Logs hoy; SQS, Step
Functions y EventBridge en camino— con la misma navegación, los mismos keybindings y el
ambiente (cuenta + región) siempre visible y cambiable al instante.

> Estado: **v0 + v1 + v2** — el shell extensible + las vistas `logs` (CloudWatch), `sqs` (colas, peek,
> purge gated) y `sfn` (Step Functions: ejecuciones, timeline, redrive gated).
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
| `:` | command bar (saltar de herramienta, p. ej. `:logs`, `:sqs`) |
| `/` | buscar (fuzzy; en `logs` consulta al servidor; `↑`/`↓` navegan los resultados sin salir) |
| `enter` | abrir herramienta (menú) / drill al detalle |
| `esc` | con filtro aplicado lo limpia (1er `esc`); si no, vuelve un nivel (drill back; en la raíz, al menú) |
| `:menu` · `backspace` | volver al menú principal |
| `j` / `k` · `↑` / `↓` · `g` / `G` | navegar |
| `r` | refrescar |
| `p` | purgar cola SQS (gated: modo escritura + confirm) |
| `R` | redrive ejecución `sfn` fallida (gated: modo escritura + confirm) |
| `:write` | alternar modo escritura (habilita acciones mutantes) |
| `ctrl-e` | cambiar de ambiente (picker de profiles) |
| `?` | ayuda |
| `q` | salir |

## Cómo probar los cambios

```bash
AWSDECK_MOCK=1 cargo run    # ver el TUI con datos, sin tocar AWS
cargo test                  # 97 tests, sin red
cargo clippy --all-targets  # lint
cargo fmt --check           # formato
```

Recorrido rápido (con `AWSDECK_MOCK=1 cargo run`):

1. Arranca en el **menú principal**; `j`/`k` + `enter` para abrir una herramienta (`logs`, `sqs`).
   `:menu` o `backspace` vuelven al menú.
2. En `logs`/`sqs`, `/` **busca fuzzy** (p. ej. `ordapi` encuentra `orders-api`) y dentro del
   filtro `↑`/`↓` navegan los resultados sin tener que salir; `enter` hace **drill** al detalle.
   `esc` es de **dos etapas** (estilo k9s): con un filtro aplicado lo limpia primero; el siguiente
   `esc` regresa un nivel (y desde la raíz de la vista, al menú).
3. En `sfn`, `enter` entra a una state machine → sus **ejecuciones con status coloreado** y duración;
   `enter` en una FAILED → detalle con input/output, error/cause y el **timeline de estados** (el que
   reventó va resaltado y preseleccionado). En una máquina `[express]` se muestra una nota (sus
   ejecuciones viven en CloudWatch Logs). Con `:write`, `R` hace **redrive** de una ejecución fallida
   (confirm modal).
4. `ctrl-e` abre el picker; elige otro profile → el ambiente y la lista cambian.
5. `?` muestra la ayuda; `q` sale y la terminal queda limpia.

**Epoch guard:** al cambiar de ambiente con un request en vuelo, nunca se pintan datos de la
cuenta anterior (probado en `app::tests::epoch_guard_discards_stale_and_accepts_fresh`).

**Escala (logs):** con miles de log groups, `logs` no los carga todos — trae una página (≤50)
y `/` consulta al servidor por substring (`logGroupNamePattern`, debounced ~280ms), rankeando
los resultados con fuzzy local. El título indica `· parcial` cuando hay más en el servidor.

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

- **v0** ✅ shell + `logs` (CloudWatch).
- **v1** ✅ `sqs` — colas, attributes, *peek*, `PurgeQueue` (gated por modo escritura + confirm).
- **v2** ✅ `sfn` — state machines, ejecuciones (status coloreado), timeline de estados con duración,
  `Redrive` (gated).
- **v3** `events` — buses, rules, `SendEvent` (gated).

Backlog: copiar ARN (`y`), abrir en consola (`o`), config en disco, más vistas (Lambda, DynamoDB, ECS…).

## Stack

`tokio` · `ratatui` + `crossterm` · `color-eyre` · `tui-input` · `aws-config` +
`aws-sdk-cloudwatchlogs` / `aws-sdk-sqs` / `aws-sdk-sfn` · `serde_json`.
