# awsdeck

**k9s, pero para mi AWS.** Un TUI en Rust: un solo binario que abre una consola de terminal
para saltar entre los servicios de AWS que uso a diario —CloudWatch Logs hoy; SQS, Step
Functions y EventBridge en camino— con la misma navegación, los mismos keybindings y el
ambiente (cuenta + región) siempre visible y cambiable al instante.

> Estado: **v0 + v1** — el shell extensible + las vistas `logs` (CloudWatch) y `sqs` (colas, peek, purge gated).
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
| `:` | command bar (saltar de servicio, p. ej. `:logs`, `:sqs`) |
| `/` | filtrar la lista actual |
| `enter` | drill (entrar al detalle) |
| `esc` | volver |
| `j` / `k` · `↑` / `↓` · `g` / `G` | navegar |
| `r` | refrescar |
| `p` | purgar cola SQS (gated: modo escritura + confirm) |
| `:write` | alternar modo escritura (habilita acciones mutantes) |
| `ctrl-e` | cambiar de ambiente (picker de profiles) |
| `?` | ayuda |
| `q` | salir |

## Cómo probar los cambios

```bash
AWSDECK_MOCK=1 cargo run    # ver el TUI con datos, sin tocar AWS
cargo test                  # 18 tests, sin red
cargo clippy --all-targets  # lint
cargo fmt --check           # formato
```

Recorrido rápido (con `AWSDECK_MOCK=1 cargo run`):

1. Arranca en `logs` y lista log groups (el header muestra `profile · region`).
2. `/` filtra en vivo; `enter` hace **drill** a los streams; `esc` regresa.
3. `ctrl-e` abre el picker; elige otro profile → el ambiente y la lista cambian.
4. `?` muestra la ayuda; `q` sale y la terminal queda limpia.

**Epoch guard:** al cambiar de ambiente con un request en vuelo, nunca se pintan datos de la
cuenta anterior (probado en `app::tests::epoch_guard_discards_stale_and_accepts_fresh`).

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
- **v2** `sfn` — ejecuciones, timeline, `Redrive` (gated).
- **v3** `events` — buses, rules, `SendEvent` (gated).

Backlog: copiar ARN (`y`), abrir en consola (`o`), config en disco, más vistas (Lambda, DynamoDB, ECS…).

## Stack

`tokio` · `ratatui` + `crossterm` · `color-eyre` · `tui-input` · `aws-config` + `aws-sdk-cloudwatchlogs`.
