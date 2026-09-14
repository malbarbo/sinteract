# sinteract — Plano de modo servidor / cliente / multiplayer

Plano consolidado da discussão de design. WebSocket / networking concreto fica
para a última fase — primeiro construir toda a infra local.

## Objetivo

sinteract é a lib gráfica compartilhada por **spython** (já) e **sgleam** (em
breve). Adicionar suporte para:

- **Servidor de jogos**: spython/sgleam roda como subprocesso de um servidor;
  inputs entram por stdin, frames saem por stdout. Anti-trapaça em multiplayer.
- **Cliente multiplayer**: spython/sgleam (ou simplecode no browser) conecta a
  um servidor, recebe DrawList por rede, pinta localmente, envia input.
  Sem rodar engine.
- **Singleplayer continua existindo** sem mudanças visíveis pro aluno.

## Modos de execução

Cada host (spython, sgleam) é uma binária só, com 3 modos via flag:

```
spython game.py            singleplayer (engine local + terminal/window)
spython --server game.py   server      (engine local + stdio FlatBuffers)
spython --client URL       client      (sem engine; recebe frames + manda input)
```

No browser via simplecode:

| Modo | O que roda | Render |
|---|---|---|
| Singleplayer browser | Engine wasm no Worker, FB → SharedArrayBuffer | JS canvas renderer (lê SAB) |
| Multiplayer browser | **Sem engine.** WebSocket | JS canvas renderer (lê WS) |

**O JS canvas renderer é o mesmo módulo** nos dois casos browser. Decoder FB
único, paint code único. Mesma simetria nativa: TerminalFrontend recebe
DrawList de engine local OU de WebSocket — não sabe a diferença.

## Decisões consolidadas

### A. Wire format

- **FlatBuffers** (schema-based, evolução por regras explícitas, JS oficial).
- **Bitmaps**: cache **negociado por sessão**.
  - `Message::Asset { id, blob }` (uploaded antes do jogo iniciar)
  - `Message::Frame { DrawList }` (referencia bitmaps por id)
  - `Message::Event { InputEvent }` (cliente → servidor)
  - `Message::Close`
- `DrawCmd::Bitmap { id: u32, cx, cy, w, h, angle, flip_h, flip_v }`.

### B. Loop e timing

- **Tick é evento na fila**, não timer interno do engine.
- `Frontend::wait_event(deadline) -> Option<InputEvent>` — bloqueia até evento
  chegar ou timeout.
- Em wasm: `Atomics.wait` no shared buffer (já é o padrão usado pra `sleep`
  hoje em simplecode).
- rAF da main thread empurra Tick + `Atomics.notify` no ritmo de `tick_rate`.
- **Throttle a taxa fixa** (`tick_rate = 30` no aluno) para previsibilidade
  entre monitores. Sem "1 rAF = 1 Tick".
- Aluno define:
  - `on_tick()` (sem dt) — beginner
  - `on_tick_time(dt)` (com dt) — advanced
  - Se ambos definidos, `on_tick` é chamado.
- Loop Python idêntico em todos os modos:
  ```python
  while ev := wait_event():
      if ev.is_tick(): on_tick(); show_image(self.draw())
      else: handle(ev)
  ```
- `time.sleep` continua existindo como primitivo low-level (sleep puro, ignora
  eventos), mas **sai da API de animação**.

### B'. Shared buffer (simplecode)

- **Atomics + fila de eventos** continuam hand-coded:
  - Renomear "key event" → "input event" (variantes KEYPRESS, KEYDOWN, KEYUP,
    TICK, CLOSE).
  - Mesmo spinlock + ring buffer.
- **Frame output buffer** (worker → main) usa **FlatBuffers**:
  - Worker escreve `[u32 len][FB bytes...]` + `Atomics.notify(FRAME_READY)`.
  - Main lê em rAF callback, decodifica, pinta no canvas, kicka próximo Tick.
- Modelo **"render previous, simulate next"** — paint sempre dentro de rAF,
  síncrono com vsync.
- env imports:
  - `draw_svg(ptr, len)` → substituído por `draw_frame()` que sinaliza buffer.
  - `get_key_event(...)` → substituído por `wait_event(deadline_ms)`.
  - `sleep(ms)` → mantido para `time.sleep` user-level, removido do loop de
    animação.

### C. Texto e fontes

- **Fontes embutidas**: Liberation Sans + Liberation Serif + Liberation Mono,
  **4 variantes cada** (Regular/Bold/Italic/BoldItalic).
- ~700KB WOFF2 no bundle JS, ~1MB no binário nativo.
- **Resolução de família** (case-insensitive):
  1. Aliases (`sans-serif`, `serif`, `monospace`, `mono`, "Liberation X") →
     embutida correspondente.
  2. Nome exato → busca no sistema (nativo via `fontdb`; JS via `ctx.font`
     fallback chain).
  3. Fallback final → Liberation Sans.
- **`TextNode` ganha `family: String` + `weight: u16`**.
- `family` no wire é a família **realmente usada** pelo servidor pra medir
  (pós-resolução), não a string que o aluno pediu.
- **Drops italic-shear synth e bold-as-regular** do `terminal.rs`/`pdf.rs`
  (variantes reais agora).
- Sem wrap automático (aluno quebra linhas com múltiplas chamadas).
- **Mudança feita já**: `FontItalic` → `FontStyle` com `Slant` → `Oblique`.
  Field `TextNode.italic` → `TextNode.style`. Compilou e passou nos testes.

### D. Topologia

- spython/sgleam: 3 modos via flag (`--server`, `--client URL`, default).
- sinteract hospeda toda lib genérica.
- **Frontend é enum** (sem `Box<dyn>`, sem custo de vtable):
  ```rust
  pub enum Frontend {
      Terminal(TerminalFrontend),
      Window(WindowFrontend),
      Stdio(StdioFrontend),
  }
  ```
- Servidor de jogos é processo separado (não em sinteract). Pode crescer no
  `simplecode/server` ou ser binário novo. Recebe WebSocket, spawna
  `spython --server` subprocess, liga stdio.
- **WebSocket fica pra Fase 8** (deferred).

## Estrutura de arquivos em sinteract

```
sinteract/src/
├── ir.rs          (existe, alterar) — DrawList, TextNode com family/weight
├── sink.rs        (existe)          — DrawSink trait
├── pdf.rs         (existe, alterar) — fontes reais por variante
├── terminal.rs    (existe, alterar) — TerminalFrontend struct
├── window.rs      (existe, alterar) — WindowFrontend struct
├── text.rs        (alterar)         — fontdb + sans/serif/mono + variantes
├── event.rs       (NOVO)            — InputEvent, KeyEvent, EventType (TICK etc)
├── wire.rs        (NOVO)            — bindings FlatBuffers + ser/de
├── frontend.rs    (NOVO)            — enum Frontend + métodos
├── stdio.rs       (NOVO)            — StdioFrontend
└── client.rs      (NOVO, parcial)   — Transport trait + run<T>(t, frontend)
                                       (WebSocket impl deferred)

sinteract/schema/
└── frame.fbs      (NOVO)            — schema FlatBuffers

sinteract/fonts/
├── LiberationSans-{Regular,Bold,Italic,BoldItalic}.ttf
├── LiberationSerif-{Regular,Bold,Italic,BoldItalic}.ttf
└── LiberationMono-{Regular,Bold,Italic,BoldItalic}.ttf
                  (hoje só tem Liberation Sans Regular)
```

## Mudanças nos hosts

### spython

- `cli/src/main.rs`: parsing de `--server` / `--client URL`.
- `engine/src/lib.rs`: aceitar `Frontend` em vez de chamar `host::*`.
- `engine/src/host/native.rs`: virar `pick_native()` em sinteract, deletar daqui.
- `engine/src/host/wasm.rs`: ajustar pra escrever FB no shared buffer (em vez
  de SVG via env).
- `lib/spython/image.py`: passar `style` em vez de `italic`, adicionar `font` +
  peso (`weight`).
- `lib/spython/world.py`: novo loop `while ev := wait_event(): ...`.

### sgleam

Replicar mesma estrutura.

### simplecode (browser)

- `src/env.ts`: adicionar `wait_event` (Atomics.wait); manter `sleep` só pra
  time.sleep user.
- `src/ui_channel.ts`: renomear "key" → "input"; adicionar TICK type;
  adicionar FRAME_READY slot e frame output buffer no layout.
- `src/worker.ts`: ler FB do output buffer; escrever no canvas via novo
  `sinteract-render.ts`.
- `src/sinteract-render.ts` (NOVO): decoder FlatBuffers + Canvas 2D painter;
  carrega WOFF2 via `@font-face`.
- `src/main.ts`: rAF loop com push Tick + notify worker.
- Servir Liberation WOFF2 estaticamente (preload sans, lazy serif/mono).

## Plano de implementação (fases)

### Fase 1 — IR e wire (não toca host)

1. Adicionar `family: String`, `weight: u16` em `TextNode`. Atualizar testes.
2. Adicionar `DrawCmd::Bitmap { id, cx, cy, w, h, angle, flip_h, flip_v }`
   (hoje é placeholder vazio).
3. Definir `sinteract::event::InputEvent` (KEYPRESS/KEYDOWN/KEYUP/TICK/CLOSE).
4. Escrever `sinteract/schema/frame.fbs` (DrawList, Asset, Event, Message).
5. Adicionar `sinteract::wire` com encode/decode FB + testes round-trip.

### Fase 2 — Frontend enum (refator interno sinteract)

6. Definir `sinteract::frontend::Frontend` enum.
7. Refatorar `terminal.rs` pra implementar TerminalFrontend (state em struct,
   não global).
8. Refatorar `window.rs` pra implementar WindowFrontend.
9. Adicionar métodos: `wait_event(deadline)`, `set_tick_rate(hz)`,
   `push_asset(asset)`, `present(dl)`, `enter()`, `exit()`.
10. Manter funções livres existentes (`show_image_dl`, `enter_animation` etc)
    como wrappers pra não quebrar spython entre fases.

### Fase 3 — StdioFrontend

11. `sinteract::stdio::StdioFrontend` — bloqueia em stdin com FB framing.
12. Suporte: FRAME → stdout, EVENT → lê via stdin, ASSET → upload via stdin.
13. Testes de integração (mock stdin/stdout).

### Fase 4 — Fontes

14. Embutir Liberation Serif e Liberation Mono (4 variantes cada). Hoje só tem
    Liberation Sans Regular.
15. Adicionar `fontdb` como dep nativa.
16. Implementar `resolve_family(name, weight, italic) -> ResolvedFont` em
    `text.rs`.
17. Atualizar renderers (terminal, pdf) pra escolher variante correta de fonte.

### Fase 5 — Integração spython

18. `engine/src/host/native.rs` → mudar pra sinteract `pick_native()`.
19. `engine/src/lib.rs` aceita `Frontend`.
20. CLI ganha `--server` / `--client`.
21. `lib/spython/world.py` muda pra wait_event loop.
22. `lib/spython/image.py` passa style/font/weight.

### Fase 6 — Integração simplecode (browser singleplayer)

23. `sinteract-render.ts` — decoder FB + canvas painter.
24. `ui_channel.ts` — adicionar TICK, FRAME_READY, frame output buffer.
25. `env.ts` — `wait_event` via Atomics.wait.
26. rAF loop em `main.ts`.
27. WOFF2 das 12 fontes servidas estaticamente.

### Fase 7 — sgleam

28. Replicar fase 5 pra sgleam.

### Fase 8 — Networking (deferred)

29. `WebSocketTransport` em sinteract atrás de feature `websocket`.
30. `sinteract::client::run` completo.
31. CLI client mode em spython funciona end-to-end.
32. Servidor de jogos (no simplecode/server ou novo binário).

## Pontos abertos

- Multiplexar input local + rede em `sinteract::client::run` — resolver na fase 8.
- Protocolo de handshake (Hello, Join, AssetManifest) — definir antes da fase 8.
- `tungstenite` em sinteract atrás de feature `websocket` (decisão tomada).
- Timing exato do throttle no rAF: `t - lastTick >= 1000/tick_rate`.
- Multi-jogador num jogo (vários inputs pra mesma engine): servidor multiplexa
  em `Event { player_id, input }`.

## Estado atual

- ✅ Decisões A, B, C, D consolidadas.
- ✅ Mudança preparatória: `FontItalic` → `FontStyle` (variante `Slant` →
  `Oblique`), `TextNode.italic` → `TextNode.style`. Aplicado em
  `sinteract/src/{ir,terminal,pdf}.rs` e
  `spython/engine/src/drawlist.rs`.
- ✅ **Fase 1 completa.**
  - `TextNode` ganhou `family: String` e `weight: u16`. `Default` impl
    adicionada (weight=400, family="").
  - `DrawCmd::Bitmap(BitmapNode { id, cx, cy, w, h, angle, flip_h, flip_v })`.
  - `sinteract::event` módulo: `InputEvent`, `KeyEvent`, `KeyKind`, MOD_*
    bitmask. Compatível com wasm32. Depois o bitmask virou a struct
    `Modifiers`, com quatro `bool`, e o campo `repeat` do `KeyEvent`, e
    `event::key` passou a nomear as teclas que não digitam texto.
  - `schema/scene.capnp`, `schema/event.capnp` e `schema/protocol.capnp`
    (Cap'n Proto). Bindings geradas em `src/wire/*_capnp.rs` (commit). O
    comando para regenerar está no cabeçalho do `scene.capnp`.
    `Message`, `DrawCmd` e `InputEvent` usam unions nativas — sem wrapper
    intermediário. `DrawList` é payload direto da `Message::frame`.
  - `sinteract::wire` com `encode_frame/event/asset/close` + `decode` →
    `Decoded`. Round-trip testes passam. Cap'n Proto self-frames cada
    mensagem; stdio framing externo `[SINT][u32 LE len][bytes]` é
    defesa adicional contra peer não-sinteract no pipe. Depois o decoder
    passou a pular elemento, evento e mensagem de braço desconhecido
    (`Decoded::Unknown`), e o stdio passou a descartar a mensagem que não
    decodifica em vez de fechar a sessão.
  - **Migrações de wire** (2026-05-08): planus → flatbuffers (descobrimos
    que planus 1.3 produz `[file_id][root_offset][body]`, oposto ao spec)
    → Cap'n Proto. flatbuffers oficial seguia o spec mas o gerador Rust
    não suporta `[union]` (google/flatbuffers#6256), o que forçava um
    wrapper `table DrawCmd { op: DrawOp; }`. Cap'n Proto trata unions
    como cidadãos de primeira classe e `capnp` 1.1.0 já vem no apt;
    crate `capnp = "0.25"` + plugin `capnpc-rust` (instalado em
    `~/.cargo/bin/` via `cargo install capnpc`).
  - **Simplificações de IR + wire** (2026-05-08, pós-Cap'n Proto):
    - Removido `DrawCmd::PathEnd` — fechamento implícito por qualquer
      cmd-terminator (PathBegin/Clip*/Text/Bitmap) ou fim da lista.
    - Path bundling: `DrawCmd` (9 variantes, uma cmd por segmento) →
      `DrawNode` (5 variantes), com `Path { style, verbs: Vec<u8>,
      coords: Vec<f32> }` carregando o path inteiro num único nó. Verb
      bytes (0=move, 1=line, 2=quad, 3=cubic) consumidos com 2/2/4/6
      floats. `DrawList.cmds` → `DrawList.nodes`. Wire ~3-4× menor para
      paths longos (path de 100 lineTo: ~970B vs 2.5KB anterior). API
      pública do `DrawList` (`path_begin / move_to / line_to / quad_to
      / cubic_to / arc_to / clip_push / clip_pop / text / bitmap`)
      preservada — a montagem do `Path` acontece em buffer interno
      (`open: Option<OpenPath>`), commit-on-terminator.
- ✅ **Fase 2 completa.**
  - `sinteract::frontend::Frontend` enum: `Terminal | Window | Stdio`. Métodos
    `enter/exit/set_tick_rate/wait_event/present/push_asset`.
  - `TickClock` helper para agendar Ticks com `tick_rate` configurável.
  - `key_event_from_legacy()` adapta a tupla `(i32, String, [bool;5])` que
    `terminal::poll_key_event` / `window::poll_key_event` retornam. Foi
    removido depois, quando os dois passaram a devolver `KeyEvent`.
  - Refator interno mínimo: `TerminalFrontend`/`WindowFrontend` delegam
    para as funções livres existentes (estado global preservado por
    enquanto). Funções livres continuam disponíveis para o spython
    integrar gradualmente.
- ✅ **Fase 3 completa.**
  - `sinteract::stdio::StdioFrontend` com framing
    `[SINT][u32 LE len][bytes]` em ambos os sentidos.
  - `wait_event` lê e decodifica mensagens de stdin; mensagens
    inesperadas viram log + continua. EOF → `None`. Erro de framing →
    `InputEvent::Close`.
  - Suite de testes com `Cursor`/`SharedWriter` cobre present/asset/close
    + corrupção de magic.
- ✅ **Fase 4 completa.**
  - 12 fontes Liberation embutidas (Sans/Serif/Mono × Regular/Bold/Italic/
    BoldItalic) em `sinteract/fonts/`.
  - `sinteract::text::resolve(family, weight, style) -> ResolvedFont`.
    Aliases (sans-serif/serif/monospace/mono/Liberation X, case-insensitive),
    fallback por `fontdb` com leak controlado de `Face<'static>`,
    fallback final para Liberation Sans.
    Hoje `resolve` e `ResolvedFont` são privados de `text`. A API pública
    é `text::measure`, e os renderers usam `TextLayout`.
  - `terminal.rs` e `pdf.rs` passaram a consultar `resolve(...)` e usar
    métricas reais (`face.units_per_em()`, `face.underline_metrics()`).
    Synth de italic-shear + bold-as-regular foram removidos.
- 🟡 **Próximo passo: Fase 5** — integração spython (CLI `--server`/
  `--client`, `world.run` usando `Frontend`, Python passa `family/weight/
  font_style` ao `_drawlist`). **Diferida pelo usuário** — só executar
  quando ele pedir.

### Resumo de testes / estado

- 72 testes da sinteract passam.
- spython compila sem mudanças (drawlist.rs preenche `family=""` e
  `weight=400` por enquanto).
- planus-cli (1.3.0) instalado em `~/.cargo/bin/planus`. Necessário só
  para regerar `wire/generated.rs`.
