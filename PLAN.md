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
`protocol.capnp` para a sessão. Cada direção tem a sua raiz, com o nome
dela, e nenhum lado recebe um braço que só outro lado manda.

| direção             | raiz             | mágica | braços                                 |
|---------------------|------------------|--------|----------------------------------------|
| engine → servidor   | `EngineToServer` | `SIE1` | `asset`, `frame`, `tickTaken`          |
| servidor → engine   | `ServerToEngine` | `SIS1` | `input`, `tick`, `lost`                |
| view → servidor     | `ViewToServer`   |        | `event`                                |
| servidor → view     | `ServerToView`   |        | `asset`, `frame`, `forget`             |

Num pipe, cada mensagem vai atrás de um cabeçalho de 8 bytes: a mágica e o
tamanho em `u32` LE. Num WebSocket vai só o payload, e a versão vai no
subprotocolo `sinteract.v1`. A view só fala por WebSocket, então não tem
mágica. O player é o número do jogador na partida, a
partir de 1, e vai no primeiro campo do payload das mensagens que falam de
um jogador: o `input` do servidor e o `frame` da engine, em que o 0 quer
dizer todos. O `tick` e o `asset` são
da sessão inteira e não têm player, e a `ViewToServer` e a `ServerToView`
também não, porque o servidor sabe o player pela conexão.

O `frame` da engine guarda a cena num campo `Data`, como uma mensagem
inteira cuja raiz é a `Scene`. O servidor lê dela só os `id`s dos bitmaps
e copia os bytes para o `frame` de uma `ServerToView`, uma vez por frame, e
todas as views do frame dividem a mesma cópia. Num frame de 1000
elementos, a cópia soma cerca de 70 µs aos 135 µs do servidor. Copiar a
cena campo a campo, com a `Scene` como struct nas duas raízes, custava
cerca de 400 µs. O preço é que o schema diz o tipo da cena só num
comentário, e um leitor abre duas mensagens.

Um leitor pula a mensagem, o elemento ou o evento de um braço que não
conhece, e o valor de enum ou o byte de verbo que não conhece. Uma paint de
braço novo desenha a cor `fallback`. Um schema evolui só acrescentando
campos com default. Um leitor não percorre mais words do que a mensagem
tem. Uma mensagem com dois ponteiros para o mesmo alvo pode passar disso,
e então o leitor a recusa. Um clip ou uma layer dentro de outros 16
(`MAX_NESTING`) some com o que tem, na `Scene` e no leitor, então um
frame desenha o mesmo no jogo local e no servidor.

A engine não sabe de rede. O servidor tira o player da conexão de cada
view, então uma view não joga em nome de outra.

Regras da sessão com servidor:

- o servidor lança a engine, e a primeira mensagem dela tem a raiz
  `Hello`, com o mínimo e o máximo de jogadores do jogo, até 1024. Se a
  primeira mensagem não se lê como um `hello`, a sala acaba, e depois do
  `hello` a engine não manda mais nada até o `start`. O lobby é do
  servidor, fora do protocolo, e usa esses limites. No começo da partida,
  o servidor manda a sua primeira mensagem, de raiz `Start`, com os
  apelidos dos jogadores, que são os mesmos até o fim. Só então as views
  conectam e mandam eventos;
- o número de um jogador é o lugar dele no `start`, a partir de 1, e um
  `input` de player 0 é erro;
- nenhuma mensagem encerra a partida. O fim do transporte encerra, o fim
  do pipe ou o close do WebSocket, que também cobre quem quebra. Para
  encerrar a partida, o servidor fecha o fd 3 da engine. Um jogador cujo
  WebSocket fecha continua na partida, parado, e a engine não fica sabendo;
- o servidor dá a cada jogador um token, no link dele, que a página
  guarda no `sessionStorage`. Uma conexão que traz o token ocupa o lugar
  do jogador, na primeira vez e depois de uma queda, e recebe os assets do
  frame mais novo e depois o frame. A conexão antiga, se ainda parece
  viva, não recebe nem muda mais nada;
- quando uma view cai ou é trocada por outra, o servidor manda à engine
  um `Up` para cada tecla e botão que ela segurava, como a janela faz ao
  perder o foco;
- um bitmap guarda a sua `Image`, e a imagem viaja na cena. O front end
  carrega a imagem uma vez com `Image::load`, que reduz uma imagem maior
  que 2048 por 2048 pixels para um PNG, mantendo a proporção, e o
  `asset::image_head` dá o tamanho e o tipo do original. A
  `Session::write_frame` manda um asset logo antes do primeiro frame que
  o desenha e dá o `id` pelo conteúdo da imagem, então um programa que
  monta a mesma imagem a cada frame a manda uma vez só. Na view, o
  `view::FrameReader` guarda as imagens dos assets e devolve
  cada frame como uma `Scene` com as imagens;
- uma imagem é um PNG, um JPEG, um GIF ou um WebP, e o formato vem dos
  primeiros bytes. O servidor não decodifica nada. Ele lê o tamanho no
  cabeçalho, que é o tamanho que o decodificador aloca, e o limite de
  pixels barra uma imagem bomba, pequena em bytes e enorme em pixels. No
  GIF, o tamanho cobre também a primeira imagem, se ela passa da tela. A
  view nativa não deixa o decodificador passar do tamanho do cabeçalho. A
  view do navegador depende do servidor, porque `createImageBitmap` não
  tem limite. Um JPEG gira pela orientação do EXIF, na view, no tamanho
  que o programa vê e no shrink, e a view do navegador passa
  `imageOrientation: "from-image"`. Um PNG ou um WebP com EXIF não gira.
  De um GIF ou de um WebP animado, só o primeiro quadro aparece;
- o servidor guarda os assets num `Cache` do módulo `asset`, com no
  máximo oito imagens de 2048 por 2048 e 48 MiB. A cada frame, ele tira
  o que os frames usaram há mais tempo até caber, por último os assets
  que esse frame desenha, e manda à engine um `lost` para cada um. Um
  asset que não cabe junto com os que chegaram depois do último frame se
  perde na hora. A `Session`
  manda de novo, com outro `id`, uma imagem perdida que um frame volta a
  desenhar, e recusa um frame cujas imagens não cabem juntas numa sala.
  O servidor não confia na engine para isso. No jogo local, o renderer
  guarda as imagens decodificadas até oito imagens de 2048 por 2048, e
  solta as que desenhou há mais tempo, mas não uma que o frame desenhou.
  Num frame com mais imagens do que cabem, as que não cabem decodificam
  a cada bitmap. Uma imagem que não decodifica
  aparece como uma caixa cinza com um X vermelho, no lugar da imagem,
  porque o `transform` do bitmap leva o quadrado unitário ao canvas,
  qualquer que seja o tamanho da imagem, e a view pula um bitmap de um
  `id` sem asset;
- o servidor lê os `id`s dos bitmaps de cada frame e guarda o frame com
  os assets que ele desenha. Uma view recebe os que lhe faltam, o frame,
  e o `forget` de um asset que nem o frame na tela nem o próximo
  desenham. O `forget` só existe na `ServerToView`. Enquanto o WebSocket
  de um jogador está ocupado, o servidor segura o próximo frame dele até
  entregá-lo, e depois passa ao mais novo, então uma view lenta pula
  frames mas avança;
- uma conexão nova começa sem assets, e a view aplica um `forget` depois
  de mostrar o frame que chegou antes dele;
- não há keep-alive no schema. O servidor usa o ping do WebSocket para
  perceber a view que caiu;
- a view manda um `resize` como primeiro evento de cada conexão;
- um timer do servidor manda o `tick` de todos os jogadores. Uma view
  não manda `tick`, porque o `InputEvent` não tem esse braço. A `Session` da engine responde
  cada `tick` com um `tickTaken` quando o entrega, e o servidor só manda
  o próximo depois disso, então uma engine mais lenta que o timer não
  acumula `tick` no pipe.

O `SERVER.md` descreve o servidor do Sarcade sobre esse protocolo, com um
exemplo em Tokio.

## Loop e ritmo

`Display::wait_event(deadline)` devolve um `Event` ou um `Interrupt`
(`Wake`, `Timeout`, `Read` ou `Close`). O ritmo vem de um evento `Tick` na
fila. O terminal e a janela fazem o próprio `Tick`, e a `Session` recebe o
`tick` do servidor, que marca o ritmo de todos os jogadores, porque cada
view tem o seu ritmo. O `Tick` é um `Event` e não um `InputEvent`, então a
entrada de um jogador não carrega um. O loop do aluno é o mesmo em todos os modos:

```python
while ev := wait_event():
    if ev.is_tick(): on_tick(); show_image(draw())
    else: handle(ev)
```

`time.sleep` continua existindo, mas sai da API de animação. No navegador,
o Worker espera com `Atomics.wait` e o `requestAnimationFrame` da thread
principal empurra o `Tick` a uma taxa fixa.

## Texto e fontes

As doze Liberation (Sans, Serif e Mono em quatro variantes) vão embutidas.
Uma família resolve por alias, depois pelas fontes do sistema com a feature
`native-fonts`, e por fim cai na Liberation Sans. A engine mede o texto, e
o `TextSpec` leva a família depois da resolução, então a view desenha com a
mesma face.

## Engine

`Display` é um trait selado, implementado por `Terminal` e `Window`, e
`open_native` escolhe a janela ou o terminal. A `Session` é o lado da
engine numa sessão com servidor. O `Session::start` manda o hello e espera
o start, então a sessão só existe depois dele. Ele entrega cada jogador
como um `Player`, com o apelido, e só a sessão cria um `Player`, então a
engine não fala de um jogador que não está no jogo. A sessão fica com o
`Read` e o
`Write` da sessão, como os descritores 3 e 4, e tudo que a engine manda ao
servidor passa por ela. O `wait` devolve os eventos com o jogador, sempre com no
máximo um `Tick` na fila, e o `write_frame` manda o frame, com cada imagem
nova antes.
O `Stage` junta os dois modos num loop só. Com `SINTERACT_SESSION` no
ambiente, o `Stage::open` abre a `Session` nos descritores 3 e 4, e senão
abre a janela ou o terminal, em que o usuário é o jogador 1. O `wait`
devolve cada entrada com o jogador, e o `present` manda o frame a um
`Target`, então o spython e o sgleam escrevem o jogo uma vez para os dois
modos.

## Dependências do servidor

O servidor usa só `server`, `scene` e `event`, e o renderer `svg` se ele
converte o frame em SVG. A feature `render`, default, traz o que ele não
usa: `tiny-skia`, `png` e `image`, do renderer `pixmap`, `pdf-writer`, do
`pdf`, e `icy_sixel`, do encoder Sixel, que vem do git e traz `quantette`,
`palette` e `rand`. As features `terminal` e `window` ligam a `render`.
Com `default-features = false`, o sinteract puxa `capnp`, `kurbo`,
`ttf-parser` e as dependências pequenas deles, cerca de 6 crates em vez de
66, e nada do git.

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
teclas vira canal de entrada com o `Tick`, e o Worker escreve o frame num
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
- a `Session`, com o `tick` do servidor e a fila que junta
  movimentos por jogador;
- a imagem na cena, os assets com `lost` e `forget`, a
  `Session::write_frame`, o `view::FrameReader` e o `Cache` de uma sala;
- o `ServerCore`, com as regras de uma sala e sem I/O;
- um módulo por papel, `session` para a engine, `server` para o servidor
  e `view` para a view, com o `wire` fechado atrás deles;
- a feature `render`, que um servidor desliga.

Falta:

- a migração do spython e do sgleam para `Display` e `Session`, e os modos
  `--server` e `--client`;
- o lado do navegador no simplecode, com o cliente de `web/`, que desenha
  a cena num canvas;
- o servidor do Sarcade, que o Gabriel escreve;
- os eixos do pad.

## Pontos abertos

- O que sobrou do item A da revisão: `seq` e `Error`.
