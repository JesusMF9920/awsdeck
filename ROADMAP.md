# awsdeck — Roadmap

> **Estado:** planeación · **Nombre de trabajo:** `awsdeck` (alt: `cloudeck`, renombrable)
> Un TUI en Rust que funciona como un *shell sobre los servicios de AWS que uso a diario*.

---

## 1. Visión

**k9s, pero para mi AWS.** Un solo binario que abre una consola de terminal donde puedo
saltar entre los servicios que toco todos los días —CloudWatch Logs, SQS, Step Functions,
EventBridge— con la misma navegación, los mismos keybindings y el ambiente (cuenta + región)
siempre visible y cambiable al instante.

**Anti-objetivo explícito:** no repetir el destino de `cwtail`. Esa herramienta murió porque
era de un solo propósito (solo log groups) y no tenía a dónde crecer. Aquí, "log groups" es
**solo una vista más** dentro de un shell extensible. La meta no es una herramienta de Step
Functions ni de logs: es la cáscara que las hospeda a todas y a las que falten.

**Criterio de éxito:** que la siga abriendo a los 6 meses porque cada servicio que necesito
ya vive ahí, y meter el siguiente cuesta un PR pequeño.

---

## 2. Principios de diseño

1. **Extensible por defecto.** Agregar un servicio = implementar el contrato `View` y
   registrarlo. El core no se entera de qué servicios existen salvo por el registro.
2. **Async fuera del trait (effects pattern).** Las vistas son síncronas y puras: estado,
   render y manejo de teclas. Jamás llaman al SDK directo. Esto mantiene el trait
   *object-safe* y evita el infierno de async-in-traits de Rust.
3. **El ambiente es estado global de primera clase.** `profile` + `region` viven en el `App`.
   Cambiar de ambiente reconstruye los clients y refresca la vista activa.
4. **UX consistente = adherencia.** Command bar, filtros, drill/back y ayuda funcionan igual
   en todas las vistas. Header siempre muestra el ambiente activo.
5. **Prod-safe.** Lectura por default en producción. Cualquier acción mutante (purge, redrive,
   send event) va detrás de un confirm modal **y** de un toggle explícito de modo escritura.

---

## 3. Arquitectura

### 3.1 Flujo de datos

```
[ teclado ] ─► App.route ─► View.on_key ─► Vec<Action>
                                              │
                                              ▼
                                         effects.rs ─► spawn(task) ─► AWS SDK
                                                                        │
            View.on_message ◄──── App ◄──── mpsc ◄──── Message (result) ┘
                  │
                  ▼
              render(frame)
```

- **`View` (contrato de plugin).** Trait síncrono y object-safe. Cada vista declara su `id`
  (alias de comando, p. ej. `"logs"`), su título para el header, qué `Action`s emitir al
  activarse, cómo reacciona a teclas (devolviendo `Action`s), cómo ingiere un `Message` y
  cómo se dibuja. Nada de SDK, nada de async aquí.
- **`Action` (intenciones).** Lo que el usuario/vista quiere que pase, normalmente async:
  `Refresh`, `Drill(id)`, `SwitchEnv(env)`, y las mutantes `PurgeQueue`, `Redrive`, `SendEvent`.
- **`Message` (resultados).** Lo que regresa del mundo async: `LogGroupsLoaded`, `QueuesLoaded`,
  `HistoryLoaded`, `Error`. Llega por un canal `mpsc` y se reparte a la vista activa.
- **`effects` (dispatcher).** Recibe `Action`s, lanza tasks de `tokio` contra el client
  correcto y manda el `Message` de vuelta. Es el único lugar que conoce el SDK.
- **`AwsContext` / `ClientFactory`.** Dado un `Env { profile, region }`, construye y cachea
  perezosamente los clients tipados (`aws-sdk-cloudwatchlogs`, `aws-sdk-sqs`, ...). Al cambiar
  de ambiente se crea un contexto nuevo.

### 3.2 Layout de módulos

```
src/
  main.rs            # init runtime + terminal, corre el loop
  tui.rs             # wrapper de terminal con Drop guard (restaura aunque haya panic)
  app.rs             # Env activo, vista activa, registry de vistas, routing
  action.rs          # enum Action (intents)
  message.rs         # enum Message (results)
  effects.rs         # dispatcher: Action -> task de tokio -> Message
  aws/
    context.rs       # Env + ClientFactory (cachea clients por ambiente)
  views/
    mod.rs           # trait View + registry
    logs.rs          # CloudWatch log groups + streams  ← cwtail vive aquí
    sqs.rs           # colas + mensajes (v1)
    sfn.rs           # ejecuciones + history (v2)
    events.rs        # buses + rules + targets (v3)
  ui/
    header.rs        # indicador de ambiente + breadcrumbs
    command_bar.rs   # `:` comandos y `/` filtro
    help.rs          # overlay de ayuda
```

---

## 4. Stack

- **Runtime async:** `tokio`
- **AWS:** `aws-config` + un crate `aws-sdk-*` por servicio (logs, sqs, sfn, eventbridge)
- **TUI:** `ratatui` + `crossterm`
- **Errores:** `color-eyre`
- **JSON / payloads:** `serde_json`
- **Input de command bar / filtro:** `tui-input`
- **Clipboard (copiar ARN/URL):** `arboard`
- **Config local (futuro):** `directories` + `toml`

> Nota: **no** se usa `async-trait`. Mantener el trait síncrono es una decisión, no una omisión.

---

## 5. Decisiones clave (ADR-lite)

| # | Decisión | Por qué |
|---|----------|---------|
| 1 | `View` síncrono y object-safe | Async en trait objects mete `Box<dyn Future>`, lifetimes y dolor. Sacando el async a `effects`, el trait queda limpio, testeable con mensajes falsos y `Box<dyn View>` funciona sin fricción. |
| 2 | Effects pattern (Action → task → Message) | Desacopla el render del I/O. El UI nunca se congela esperando a AWS. Las vistas se testean inyectando `Message`s sin tocar la red. |
| 3 | Ambiente con epoch / descarte de respuestas viejas | Cada request se etiqueta con el epoch del `Env` que lo lanzó. Tras un switch, las respuestas del ambiente anterior se descartan para no pintar datos de la cuenta equivocada. Correctitud crítica trabajando sobre prod. |
| 4 | Read-only por default en prod | Evita un purge o redrive accidental en la cuenta productiva. Escritura solo con toggle explícito + confirm. |
| 5 | Command bar estilo k9s (`:logs`, `:sqs`) | Una sola forma de navegar entre servicios; agregar una vista la hace accesible sin tocar menús. |

---

## 6. Fases

Cada fase deja algo que **ya uso**. Un servicio nuevo = un PR autocontenido contra un core estable.

### v0 — El shell + vista `logs` (reemplaza `cwtail`)
**Objetivo:** probar toda la arquitectura end-to-end con una sola vista real, y jubilar `cwtail` desde el día uno.

Entrega:
- Loop de render con `tokio::select!` sobre teclado (`EventStream`) y canal de mensajes.
- `tui.rs` con Drop guard + panic hook: la terminal queda limpia siempre, incluso en panic.
- Header con `profile · region` activos.
- Command bar: `:` abre comandos, `:logs` activa la vista.
- `ctrl-e`: picker de profiles leídos de `~/.aws/config`, cambia ambiente, refresca la vista.
- Vista `logs`: lista log groups del ambiente activo; `/` filtra; `enter` hace drill a log streams; `esc` regresa.
- `?` ayuda, `q` salir.
- `ClientFactory` por ambiente + epoch guard contra respuestas stale.

**Criterios de aceptación:**
- [ ] `cargo run` abre el TUI y al salir (o en panic) la terminal queda intacta.
- [ ] El header refleja el ambiente activo y cambia con `ctrl-e`.
- [ ] `:logs` lista log groups reales del SDK (tras validar primero contra un mock).
- [ ] `/` filtra, `enter` entra a streams, `esc` vuelve.
- [ ] Un switch de ambiente con un request en vuelo no pinta datos del ambiente anterior.
- [ ] El core (`app`, `effects`, `views/mod`) no referencia ningún servicio concreto salvo por el registry.

### v1 — Vista `sqs`
Listar colas del ambiente; ver attributes (mensajes visibles, in-flight, DLQ); *peek* de mensajes (receive sin borrar). Acción mutante `PurgeQueue` detrás de confirm + modo escritura.

### v2 — Vista `sfn` (Step Functions)
Listar ejecuciones por state machine con status coloreado; drill al timeline de estados con duración; en fallos, saltar al estado que reventó y mostrar input/output. `Redrive` como acción gated.

### v3 — Vista `events` (EventBridge)
Listar event buses, rules y targets; inspeccionar el patrón de cada rule; `SendEvent` de prueba (gated) para disparar un evento contra un bus.

---

## 7. Convenciones

### Keybindings (iguales en todas las vistas)
| Tecla | Acción |
|-------|--------|
| `:` | command bar (saltar de servicio) |
| `/` | filtrar la lista actual |
| `enter` | drill (entrar al detalle) |
| `esc` | volver |
| `r` | refresh |
| `ctrl-e` | cambiar de ambiente |
| `y` | copiar ARN/URL al clipboard |
| `?` | ayuda |
| `q` | salir |

### Reglas de código
- El core nunca conoce servicios concretos: solo el registry los conecta.
- Toda llamada a AWS vive en `effects` / `aws`; ninguna vista importa un `aws-sdk-*`.
- Credenciales: siempre vía `aws-config` (profiles/SSO). Nunca hardcodear nada.
- Cada vista nueva debe ser testeable inyectándole `Message`s sin red.

---

## 8. Backlog / futuro

- Más vistas: Lambda (invoke + config), DynamoDB (scan/query), ECS (services/tasks), RDS (estado), S3.
- Archivo de config (`~/.config/awsdeck/config.toml`): región default, profiles favoritos, vistas a mostrar, modo escritura por ambiente.
- Vínculo a CloudWatch Logs desde otras vistas (p. ej. del Lambda de un estado de SFN → sus logs).
- Abrir el recurso en la consola de AWS (`o`).
- Temas / paleta, y modo "denso" para pantallas chicas.
- Navegación fuzzy entre recursos (estilo `:` con autocompletado).

---

## 9. Decisiones abiertas

- Nombre final (`awsdeck` vs `cloudeck` vs otro).
- ¿Soporte de SSO/`aws sso login` en v0 o lo dejamos para v1?
- ¿Multi-región simultánea (ver dos regiones a la vez) o una a la vez por ahora? (Propuesta: una a la vez en v0.)
