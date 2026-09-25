# sinteract, plano dos modos servidor, cliente e multiplayer

## Objetivo

O sinteract é a biblioteca gráfica do spython e do sgleam. Além do jogo
local, ela serve a dois modos novos. No modo servidor, o spython ou o sgleam
roda como subprocesso de um servidor de jogos, lê a sessão no descritor 3
e escreve os frames no descritor 4, e o estado do jogo nunca sai do
processo. No modo cliente, uma view recebe os frames pela rede, desenha e
manda a entrada, sem rodar engine. O jogo local continua como está para o aluno.

## Modos

Cada host é um binário só, com o modo escolhido na linha de comando:

```
spython jogo.py              local: engine e terminal ou janela
spython --server jogo.py     servidor: engine numa Session, fds 3 e 4
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

| quem escreve | raiz            | mágica | braços                                             |
|--------------|-----------------|--------|----------------------------------------------------|
| engine       | `EngineMessage` | `SIE1` | `asset`, `frame`, `hello`, `forget` |
| view         | `ViewMessage`   |        | `event`                 |
| servidor     | `ServerMessage` | `SIS1` | `event`, `start`, `tick`, `lost` |

Num pipe, cada mensagem vai atrás de um cabeçalho de 8 bytes: a mágica e o
tamanho em `u32` LE. Num WebSocket vai só o payload, e a versão vai no
subprotocolo `sinteract.v1`. A view só fala por WebSocket, então não tem
mágica. O player é o número do jogador na partida, a
partir de 1, e vai no primeiro campo do payload das mensagens que falam de
um jogador: o `event` do servidor e o `frame` da engine, em que o 0 quer
dizer todos. O `start`, o `tick` e o `asset` são
da sessão inteira e não têm player, e a `ViewMessage` também
não, porque o servidor sabe o player pela conexão.

Um leitor pula a mensagem, o elemento ou o evento de um braço que não
conhece, e o valor de enum ou o byte de verbo que não conhece. Uma paint de
braço novo desenha a cor `fallback`. Um schema evolui só acrescentando
campos com default.

A engine não sabe de rede. O servidor tira o player da conexão de cada
view, então uma view não joga em nome de outra.

Regras da sessão com servidor:

- o servidor lança a engine, e a primeira mensagem dela é o `hello`, com
  o mínimo e o máximo de jogadores do jogo. Se a primeira mensagem não é
  um `hello`, a sala acaba. O lobby é do servidor, fora do protocolo, e
  usa esses limites. No começo da partida, o servidor manda o `start` com
  os jogadores, que são os mesmos até o fim. Só então as views conectam e
  mandam eventos;
- um jogador aparece uma vez no `start`, o número dele não se repete na
  partida, e um `event` de player 0 é erro;
- nenhuma mensagem encerra a partida. O fim do transporte encerra, o fim
  do pipe ou o close do WebSocket, que também cobre quem quebra. Para
  encerrar a partida, o servidor fecha o fd 3 da engine. Um jogador cujo
  WebSocket fecha continua na partida, parado, e a engine não fica sabendo;
- o servidor dá a cada jogador um token, no link dele, que a página
  guarda no `sessionStorage`. Uma conexão que traz o token ocupa o lugar
  do jogador, na primeira vez e depois de uma queda, e recebe todos os
  assets e o frame mais novo. A conexão antiga, se ainda parece viva, não recebe nem muda mais
  nada;
- quando uma view cai ou é trocada por outra, o servidor manda à engine
  um `Up` para cada tecla e botão que ela segurava, como a janela faz ao
  perder o foco;
- a engine manda um asset logo antes do primeiro frame que o desenha. A
  tabela `Assets` do módulo `asset` dá o `id` pelo conteúdo do PNG, então
  um programa que monta a mesma imagem a cada frame a manda uma vez só, e
  reduz uma imagem maior que 2048 por 2048 pixels, mantendo a proporção;
- o servidor guarda os assets num `Cache` do módulo `asset`, com no
  máximo oito imagens de 2048 por 2048 e 48 MiB. Para caber um asset
  novo, ele tira o que os frames usaram há mais tempo, fora os que o
  último frame de algum jogador desenha, e manda à engine um `lost` para
  cada um. Um asset que não cabe nem assim se perde na hora. A engine
  manda de novo, com outro `id`, uma imagem perdida que um frame volta a
  desenhar. O servidor não confia na engine para isso, e o jogo local usa
  o mesmo `Cache` com o `Display`. Um bitmap sem imagem aparece na view
  como uma caixa cinza com um X vermelho, no lugar da imagem, porque o
  `transform` do bitmap leva o quadrado unitário ao canvas, qualquer que
  seja o tamanho da imagem;
- o servidor lê os `id`s dos bitmaps de cada frame e guarda o frame com
  os assets que ele desenha. Uma view recebe os que lhe faltam, o frame,
  e o `forget` de um asset que nem o frame na tela nem o próximo
  desenham. Só o servidor manda `forget`. Enquanto o WebSocket de um
  jogador está ocupado, o servidor segura o próximo frame dele até
  entregá-lo, e depois passa ao mais novo, então uma view lenta pula
  frames mas avança;
- uma conexão nova começa sem assets, e a view aplica um `forget` depois
  de mostrar o frame que chegou antes dele;
- não há keep-alive no schema. O servidor usa o ping do WebSocket para
  perceber a view que caiu;
- a view manda um `resize` como primeiro evento de cada conexão;
- um timer do servidor manda o `tick`, que é o `Vsync` de todos os
  jogadores, e o servidor não repassa o `Vsync` da view. A engine guarda
  no máximo um `Vsync` na fila, então não acumula `tick`.

O documento `sgleam/RUNTIME_PROTOCOL.md` descreve o servidor do Sarcade
sobre esse protocolo, com um exemplo em Tokio.

## Loop e ritmo

`Display::wait_event(deadline)` devolve um `Event` ou um `Interrupt`
(`Wake`, `Timeout`, `Read` ou `Close`). O ritmo vem de um evento `Vsync` na
fila. O terminal e a janela fazem o próprio `Vsync`, e a `Session` recebe o
`tick` do servidor, que marca o ritmo de todos os jogadores, porque cada
view tem o seu ritmo. Um `event` do servidor que é um `Vsync` é um erro de
leitura. O loop do aluno é o mesmo em todos os modos:

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

`Display` é um trait selado, implementado por `Terminal` e `Window`, e
`open_native` escolhe a janela ou o terminal. A `Session` é o lado da
engine numa sessão com servidor. Ela não faz E/S. O host lhe dá os bytes
que leu, com `feed`, ou chama `wait` sobre um `Read`, como o descritor 3,
e ela devolve os eventos com o jogador, sempre com no máximo um `Vsync` na
fila. A engine
escreve os frames no descritor 4 com `to_view`. Um host roda o jogo local
num `Display` e o modo servidor numa `Session`, e um adaptador liga o jogo
de um jogador só ao jogador 1.

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
`Display` ou uma `Session` em vez de chamar `host::*`, o `host/native.rs`
some em favor de `open_native`, e o `world.py` passa ao loop de
`wait_event`. O `image.py` passa `style`, `family` e `weight`. O sgleam faz
o mesmo. No modo servidor, o protocolo passa pelos descritores 3 e 4,
então o `print` do aluno no stdout não atrapalha a partida.

No simplecode, o `env.ts` ganha `wait_event` com `Atomics.wait`, o canal de
teclas vira canal de entrada com o `Vsync`, e o Worker escreve o frame num
buffer de saída que a thread principal desenha no `requestAnimationFrame`.
As fontes WOFF2 são servidas estaticamente.

## Estado

Feito no sinteract:

- a cena, os três renderers e o texto com as doze fontes;
- o schema em três arquivos e o `wire` em camadas, com leitura tolerante a
  schema mais novo;
- o protocolo por direção, com os três lados e o player no payload;
- o trait `Display`, com `Terminal` e `Window`, as features `terminal` e
  `window`, e o `wait_event` com `Interrupt`;
- a entrada com teclado, mouse, resize e pad de 12 botões;
- a `Session`, com o `Vsync` do `tick` do servidor e a fila que junta
  movimentos por jogador;
- os assets com `lost` e `forget`, a tabela `Assets` da engine e o
  `Cache` de uma sala;
- as funções do servidor: `framing::parse_header`, `to_server::decode` e
  `to_view::arm`;
- a feature `render`, que um servidor desliga.

Falta:

- a migração do spython e do sgleam para `Display` e `Session`, e os modos
  `--server` e `--client`;
- o lado do navegador no simplecode, e a escolha entre SVG feito no
  servidor e um decodificador que desenha num canvas;
- o servidor do Sarcade, que o Gabriel escreve;
- os eixos do pad.

## Pontos abertos

- O caminho do SVG no servidor precisa de `to_view::decode` público.
- Os bytes que sobram depois de uma mensagem são aceitos.
- O que sobrou do item A da revisão: `seq` e `Error`.
- Um asset vai para todos os jogadores, então um jogador vê no DevTools a
  imagem que só outro jogador desenha. O asset pode ganhar um `player` no
  fim, como o frame.
