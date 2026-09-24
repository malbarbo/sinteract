# sinteract, plano dos modos servidor, cliente e multiplayer

## Objetivo

O sinteract é a biblioteca gráfica do spython e do sgleam. Além do jogo
local, ela serve a dois modos novos. No modo servidor, o spython ou o sgleam
roda como subprocesso de um servidor de jogos, lê a entrada no stdin e
escreve os frames no stdout, e o estado do jogo nunca sai do processo. No
modo cliente, uma view recebe os frames pela rede, desenha e manda a
entrada, sem rodar engine. O jogo local continua como está para o aluno.

## Modos

Cada host é um binário só, com o modo escolhido na linha de comando:

```
spython jogo.py              local: engine e terminal ou janela
spython --server jogo.py     servidor: engine num Room, stdin e stdout
spython --client URL         cliente: view sem engine
```

No navegador, o simplecode roda a engine em wasm num Worker no jogo local,
e o multiplayer não roda engine nenhuma. Nos dois casos quem desenha é o
mesmo código, que lê a cena do buffer compartilhado ou do WebSocket.

## Papéis e protocolo

Há três lados. A engine roda o programa e manda os frames, a view desenha e
manda a entrada, e o servidor é dono da sessão e diz à engine quem joga. O
formato é Cap'n Proto, e `schema/` é a fonte da verdade, com um arquivo por
camada: `scene.capnp` para a cena, `event.capnp` para a entrada e
`protocol.capnp` para a sessão. Cada lado escreve a sua própria raiz, com
os braços abaixo.

| quem escreve | raiz            | mágica | braços                                     |
|--------------|-----------------|--------|--------------------------------------------|
| engine       | `EngineMessage` | `SIE1` | `asset`, `frame`, `close`                  |
| view         | `ViewMessage`   | `SIV1` | `event`, `close`                           |
| servidor     | `ServerMessage` | `SIS1` | `event`, `close`, `start`, `join`, `leave` |

Num pipe, cada mensagem vai atrás de um cabeçalho de 12 bytes: a mágica, o
player em `u32` LE e o tamanho em `u32` LE. O player é o número do jogador
na partida, a partir de 1, e o 0 quer dizer todos, ou a sessão inteira. Num
WebSocket vai só o payload, e a versão vai no subprotocolo `sinteract.v1`.
Uma view que fala com a engine sem servidor escreve `ViewMessage`, que a
engine lê com o `Stdio`.

Um leitor pula a mensagem, o elemento ou o evento de um braço que não
conhece, e o valor de enum ou o byte de verbo que não conhece. Uma paint de
braço novo desenha a cor `fallback`. Um schema evolui só acrescentando
campos com default.

A engine não sabe de rede. O servidor tira o player da conexão de cada
view, então uma view não joga em nome de outra.

Regras da sessão com servidor:

- o `start` é a primeira mensagem, com os jogadores que já estão na sala, e
  pode vir vazio;
- um jogador aparece uma vez no `start`, o número dele não se repete na
  partida, e `join` e `leave` de player 0 são erro;
- o `close` do servidor vai com player 0 e encerra a partida. Para tirar um
  jogador, o servidor fecha o WebSocket dele e manda `leave`;
- a engine manda os assets com player 0, os `id`s valem para a partida
  toda, e o servidor guarda todos para quem entrar depois;
- um frame para um jogador que saiu é descartado em silêncio, porque a
  engine pode tê-lo escrito antes de ler o `leave`;
- enquanto o WebSocket de um jogador está ocupado, o servidor guarda só o
  frame mais novo dele;
- não há keep-alive no schema. O servidor usa o ping do WebSocket, guarda
  o lugar de quem caiu por uns 30 s e depois manda `leave`;
- a view manda um `resize` como primeiro evento de cada conexão.

O documento `sgleam/RUNTIME_PROTOCOL.md` descreve o servidor do Sarcade
sobre esse protocolo, com um exemplo em Tokio.

## Loop e ritmo

`Display::wait_event(deadline)` devolve um `Event` ou um `Interrupt`
(`Wake`, `Timeout`, `Read` ou `Close`). O ritmo vem de um evento `Vsync` na
fila. O terminal e a janela fazem o próprio `Vsync`, o `Stdio` recebe o da
view, e o `Room` tem um relógio e ignora o das views, porque cada view tem
o seu ritmo. O loop do aluno é o mesmo em todos os modos:

```python
while ev := wait_event():
    if ev.is_tick(): on_tick(); show_image(draw())
    else: handle(ev)
```

`time.sleep` continua existindo, mas sai da API de animação. No navegador,
o Worker espera com `Atomics.wait` e o `requestAnimationFrame` da thread
principal empurra o `Vsync` a uma taxa fixa.

## Texto e fontes

As doze Liberation (Sans, Serif e Mono em quatro variantes) vão embutidas.
Uma família resolve por alias, depois pelas fontes do sistema com a feature
`native-fonts`, e por fim cai na Liberation Sans. A engine mede o texto, e
o `TextSpec` leva a família depois da resolução, então a view desenha com a
mesma face.

## Engine

`Display` é um trait selado, implementado por `Terminal`, `Window` e
`Stdio`, e `open_native` escolhe a janela ou o terminal. O `Room` é o lado
da engine numa sessão com jogadores. Ele não é um `Display`, porque o
evento e o frame levam o jogador: `wait_event` entrega um `RoomEvent`, e há
`present_to` e `present_all`. `Room::open` espera o `start` e devolve os
jogadores. Um host roda o jogo local num `Display` e o modo servidor num
`Room`, e um adaptador liga o jogo de um jogador só ao jogador 1.

## Dependências do servidor

O servidor usa só `wire`, `scene` e `event`, e o renderer `svg` se ele
converte o frame em SVG. A feature `render`, default, traz o que ele não
usa: `tiny-skia` e `png`, do renderer `pixmap`, `pdf-writer`, do `pdf`, e
`icy_sixel`, do encoder Sixel, que vem do git e traz `quantette`,
`palette` e `rand`. As features `terminal` e `window` ligam a `render`.
Com `default-features = false`, o sinteract puxa `capnp`, `kurbo`,
`ttf-parser` e as dependências pequenas deles, cerca de 5 crates em vez de
52, e nada do git.

Um crate separado para o `wire` não vale a pena. O `wire` converte a
`Scene` e o `InputEvent`, a `Scene` usa o `text` para medir, e o `text` traz
as fontes. O crate novo levaria `scene`, `event`, `text` e `wire`, que são
quase todo o sinteract fora dos renderers e das displays, e as mesmas
dependências que ficam sem a `render`. Ele ainda obrigaria a versionar dois
crates juntos, já que a versão do protocolo está no schema.

## Hosts

No spython, o `cli` ganha `--server` e `--client`, a engine recebe um
`Display` ou um `Room` em vez de chamar `host::*`, o `host/native.rs` some
em favor de `open_native`, e o `world.py` passa ao loop de `wait_event`. O
`image.py` passa `style`, `family` e `weight`. O sgleam faz o mesmo. No
modo servidor, os dois guardam o descritor 1 para o protocolo e apontam o
`print` do aluno para o stderr, já que um texto no stdout encerra a
partida.

No simplecode, o `env.ts` ganha `wait_event` com `Atomics.wait`, o canal de
teclas vira canal de entrada com o `Vsync`, e o Worker escreve o frame num
buffer de saída que a thread principal desenha no `requestAnimationFrame`.
As fontes WOFF2 são servidas estaticamente.

## Estado

Feito no sinteract:

- a cena, os três renderers e o texto com as doze fontes;
- o schema em três arquivos e o `wire` em camadas, com leitura tolerante a
  schema mais novo;
- o protocolo por direção, com os três lados e o player no cabeçalho;
- o trait `Display`, com `Terminal`, `Window` e `Stdio`, as features
  `terminal` e `window`, e o `wait_event` com `Interrupt`;
- a entrada com teclado, mouse, resize e pad de 12 botões;
- o `Room`, com o relógio próprio e a fila que junta movimentos por
  jogador;
- as funções do servidor: `framing::parse_header`, `to_server::decode` e
  `to_view::arm`;
- a feature `render`, que um servidor desliga.

Falta:

- a migração do spython e do sgleam para `Display` e `Room`, e os modos
  `--server` e `--client`;
- o lado do navegador no simplecode, e a escolha entre SVG feito no
  servidor e um decodificador que desenha num canvas;
- o servidor do Sarcade, que o Gabriel escreve;
- os eixos do pad.

## Pontos abertos

- O comentário de `ServerMessage.event` diz que o player 0 é o servidor,
  como num tick, mas o `Room` rejeita um evento de player 0, e
  `to_engine::write_input` aceita um `u32` qualquer.
- Um `close` do servidor com player diferente de 0 encerra a partida
  inteira sem aviso, e o `close` da engine com player N não tem sentido
  definido.
- O caminho do SVG no servidor precisa de `to_view::decode` público.
- Os bytes que sobram depois de uma mensagem são aceitos.
- O que sobrou do item A da revisão: `seq`, `Error` e `Hello`.
- O cache de assets do servidor não tem como apagar um asset.
