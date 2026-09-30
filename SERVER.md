# Protocolo Sarcade ↔ runtime

Este contrato liga o servidor do Sarcade ao runtime do sgleam e do
spython. O schema em `schema/*.capnp` é a fonte da verdade do formato das
mensagens. As regras da sala ficam em `sinteract::server`, as da engine em
`sinteract::session` e as da view em `sinteract::view`. Este documento diz
como o servidor usa o `server` e o que esperar dos outros dois lados. Uma
mudança da API pública do `sinteract` atualiza este arquivo no mesmo
commit.

O servidor não precisa conhecer Cap'n Proto. Ele depende só do `sinteract`,
sem as features de tela, e chama o `LobbyCore` e o `ServerCore` para cada
coisa que acontece. Os dois não fazem E/S e não têm relógio. A E/S e o
timer ficam com o Tokio.

```toml
sinteract = { git = "...", default-features = false }
```

## Os três lados

- A **engine** é o processo do jogo (`sgleam server jogo.gleam`). Ela roda o
  programa, lê a sessão no fd 3 e escreve os frames no fd 4. Do lado dela,
  o `sinteract` oferece o `Stage`, que abre a sessão, transforma os bytes
  do fd 3 nos eventos do jogo e escreve os frames com as imagens deles.
- A **view** é o navegador. Ela desenha os frames e manda a entrada pelo
  WebSocket.
- O **servidor** é o Sarcade. Ele é dono da sessão, faz o lobby, diz à
  engine quem joga e repassa a entrada de cada view com o número do
  jogador.

Cada direção tem a sua mensagem raiz no schema `protocol.capnp`:

| de → para          | raiz             | mágica | braços                              |
|--------------------|------------------|--------|-------------------------------------|
| engine → servidor  | `EngineToServer` | `SIE1` | `asset`, `frame`, `tickTaken`       |
| servidor → engine  | `ServerToEngine` | `SIS1` | `input`, `tick`, `lost`             |
| view → servidor    | `ViewToServer`   | —      | só o campo `event`                  |
| servidor → view    | `ServerToView`   | —      | `asset`, `frame`, `forget`          |

A primeira mensagem da engine tem a raiz `Hello`, e a primeira do servidor
tem a raiz `Start`. Nenhuma das duas vem de novo, então nenhuma é braço de
uma união.

A view só fala pelo WebSocket, então as mensagens dela e para ela não têm
mágica.

Nenhuma mensagem encerra a sessão. O fim do fluxo encerra, o fim de um
pipe ou o close de um WebSocket, o que cobre também um lado que cai.

## Processo e ciclo de vida

Cada partida tem um processo de engine. O servidor cria dois pipes e os
entrega à engine como fd 3 e fd 4, com `SINTERACT_SESSION=1` no ambiente.
A engine lê as mensagens do servidor (`SIS1`) no fd 3 e escreve as suas
(`SIE1`) no fd 4. Os números são fixos, porque as permissões do Deno já
nomeiam `/dev/fd/3` e `/dev/fd/4`. O módulo `session` dá esses nomes como
`SESSION_VAR`, `SERVER_TO_ENGINE_FD` e `ENGINE_TO_SERVER_FD`. O crate
`command-fds` põe os pipes no lugar. O `Command` guarda as pontas da
engine, então o servidor o descarta logo depois do `spawn`. Senão, o
servidor guarda a ponta de escrita do fd 4 e nunca vê o fim dele.

O `stdin`, o `stdout` e o `stderr` ficam livres para o programa. Um `print`
do aluno não passa pelos pipes da sessão e não estraga a partida. O
servidor pode guardar o `stdout` e o `stderr` como log.

A engine roda num grupo de processos próprio (`process_group(0)`). Um
Ctrl-C no terminal do servidor manda SIGINT ao grupo inteiro, e a engine
no mesmo grupo morreria com o sinal, sem a tela final. No grupo próprio,
só o servidor recebe o sinal, e ele encerra a sala com o `close`.

A engine não tem relógio. Cada passo do jogo começa com uma leitura
bloqueante do fd 3, e sem `tick` o jogo espera.

A sala passa por cinco fases:

1. O `LobbyCore` espera o `hello` da engine, a primeira mensagem do fd 4,
   que diz quantos jogadores o jogo aceita. Uma primeira mensagem que não
   se lê como um `hello` encerra a sala (`LobbyError::Payload`).
2. Com o `hello`, o `LobbyCore::players()` diz a faixa de jogadores do
   jogo, e o lobby do servidor junta os jogadores. A engine não manda nada
   até o `start`, e um byte dela nessa fase encerra a sala
   (`LobbyError::BeforeStart`).
3. O `LobbyCore::start` começa o jogo com os jogadores do lobby e devolve o
   `ServerCore`. Os jogadores são os mesmos até o fim.
4. O `close` do servidor para de escrever no fd 3. O servidor fecha o
   fd 3, e o fim dele diz à engine que acabe. A engine ainda pode mandar os
   últimos frames.
5. A sala acaba (`is_over`) quando o fd 4 chega ao fim, ou quebra com um
   cabeçalho inválido ou com o fim no meio de uma mensagem.

A engine também pode sair sozinha, e o fim do fd 4 acaba a sala do mesmo
jeito. Quando a sala acaba, cada view recebe o último frame e depois
`Gone`, e o servidor fecha o WebSocket. O servidor espera um pouco e mata o
processo se ele não sair. Depois de um fluxo quebrado, a engine pode seguir
viva e bloqueada numa escrita, então o servidor fecha o fd 3 ou mata o
processo.

## Framing

Nos pipes, cada mensagem tem um cabeçalho de 8 bytes na frente do payload
Cap'n Proto:

```text
+-----------------+------------------+----------------+
| mágica: 4 bytes | tamanho: u32 LE  | payload        |
| SIE1 ou SIS1    | múltiplo de 8    | tamanho bytes  |
+-----------------+------------------+----------------+
```

O limite do tamanho é 64 MiB e é verificado antes da alocação. O
`LobbyCore` e o `ServerCore` fazem o framing dos dois pipes. Eles escrevem
os cabeçalhos do fd 3 e separam as mensagens do fd 4, então o servidor
passa os bytes como chegaram, em pedaços de qualquer tamanho.

No WebSocket não há cabeçalho. Cada mensagem binária do WebSocket é um
payload inteiro, e a versão vai no subprotocolo, `sinteract.v1`, que o
`server::SUBPROTOCOL` e o `view::SUBPROTOCOL` guardam. O `ServerCore`
monta as mensagens para a view e lê as da view como chegaram.

O `player` é o número do jogador na partida, a partir de 1, a posição dele
nos membros do `start`. Ele vai no primeiro campo do payload das mensagens
que falam de um jogador:

- o `input` do servidor leva o jogador;
- o `frame` da engine leva o jogador que o recebe, ou 0 para todos.

A view não diz quem é, porque o servidor sabe pela conexão.

## O `LobbyCore` e o `ServerCore`

```rust
use sinteract::server::{Conn, EngineError, LobbyCore, LobbyError, Next, ServerCore};

let mut lobby = LobbyCore::new();
lobby = lobby.from_engine(&bytes)?;       // bytes do fd 4; um erro acaba a sala
lobby.players();                          // a faixa do hello, ou None antes dele
let mut core: ServerCore = lobby.start(&nicknames, tick_rate).map_err(|(lobby, e)| e)?;
core.players();                           // cada jogador com o apelido
let conn: Conn = core.connect(player)?;   // uma view com o token do jogador
core.from_view(conn, &payload)?;          // uma mensagem do WebSocket
core.tick();                              // pelo timer
core.take_engine_output(&mut buf);        // bytes para o fd 3, ou false
let errors: Vec<EngineError> = core.from_engine(&bytes); // bytes do fd 4
match core.next_for(conn) {               // o que mandar à view
    Next::Send(payload) => { /* uma mensagem binária */ }
    Next::Idle => { /* esperar */ }
    Next::Gone => { /* fechar o WebSocket */ }
}
core.leave(conn);                         // a conexão caiu
core.close();                             // o servidor encerra
core.engine_ended();                      // o fd 4 acabou
core.is_over();                           // a engine acabou
```

O `LobbyCore::from_engine` consome o lobby e devolve o lobby com os bytes
novos, ou um `LobbyError`, e todo `LobbyError` acaba a sala. O
`ServerCore::from_engine` devolve os erros na ordem do fluxo. Só o
`EngineError::Broken` acaba a sala, e o core descarta a mensagem dos
outros e segue.

O core escreve as mensagens para a engine num buffer, na ordem das
chamadas. O `take_engine_output(&mut buf)` move tudo para o fim de `buf`,
e o servidor escreve `buf` no fd 3. Se uma task só leva a saída, a ordem no
fd 3 é a ordem do lock. Depois do `close`, ou no fim da sala, ele devolve
`false`, e a task fecha o fd 3. Antes do `start` não há nada para a
engine.

### O lobby e os jogadores

- O `sinteract` não tem sala de espera. Depois do `hello`, o
  `LobbyCore::players()` devolve a faixa de jogadores do jogo, de 1 a 1024
  (`session::MAX_PLAYERS`), e o lobby do servidor junta os apelidos.
- O `LobbyCore::start(&nicknames, tick_rate)` numera os jogadores a partir
  de 1, na ordem da lista, e manda o `start` à engine, com a taxa do
  `tick`. Ele recusa um número de
  jogadores fora da faixa e um `start` antes do `hello` (`StartError`), e
  devolve o lobby junto com o erro, para outra tentativa.
- O `start` tira os caracteres de controle do apelido e o corta em 64
  bytes. O apelido vem do login ou do lobby, nunca da engine. O
  `ServerCore::players()` dá cada jogador com o apelido que a engine
  recebeu. O jogador é um `session::Player`, o mesmo tipo da engine, e só
  o `start` cria um. O `number()` dá o número dele, para um log.
- Depois do `start`, o servidor dá a cada jogador um token, como 16 bytes
  aleatórios no link do jogador, e a página guarda o token no
  `sessionStorage`. O servidor confere o token e chama
  `connect(player)`, na primeira vez e depois de uma queda.
- O `connect` devolve um `Conn` novo. A conexão anterior do mesmo lugar
  recebe `Gone` e não muda mais nada, mesmo que para o servidor ela ainda
  pareça viva. O `connect` devolve `None` no fim da sala e para um
  jogador de outra sala.
- O `leave(conn)` diz que o WebSocket fechou. O lugar fica, e a engine
  continua vendo o jogador. Um segundo `leave`, ou o `leave` de uma
  conexão antiga, não faz nada.

### O ritmo e o fim

- O `tick` manda à engine a hora de desenhar os próximos frames. Um timer
  do servidor chama o `tick`, e o ritmo não sofre com o atraso do
  WebSocket de nenhum jogador.
- O `start` leva a taxa do timer em milésimos de hertz, no campo
  `tickRate`, 60000 para 60 Hz, e o servidor mantém essa taxa até o fim.
  O `tick` não leva hora, então a engine anda um passo dessa taxa a cada
  `tick`, e o jogo tem a mesma velocidade em qualquer taxa, sem o tremor
  de um relógio medido na chegada. Uma taxa 0 é dano.
- A engine responde a cada `tick` com um `tickTaken`, e o core só manda o
  próximo `tick` depois dele. Uma engine mais lenta que o timer não
  acumula `tick`. A `Session` da engine escreve o `tickTaken` sozinha.
- O `close` para de escrever no fd 3 e descarta o que o servidor ainda não
  levou. O core segue lendo o fd 4, e os últimos frames ainda chegam às
  views.
- O `is_over` diz que a engine acabou. O timer e a task do fd 4 param aí.

### A view

- O `from_view` recusa um payload acima de 64 KiB (`MAX_VIEW_BYTES`), e
  devolve um `ViewError` para um payload que não decodifica. Um braço de
  um schema mais novo é descartado.
- Um `event` vai à engine com o número do jogador da conexão, e a view
  nunca escolhe em nome de quem joga. Depois do `close`, a entrada é
  descartada, assim como a de uma conexão antiga. Um servidor que lê a
  entrada de outro jeito, sem WebSocket, passa o evento ao `input(conn,
  &event)`.
- O core guarda as teclas e os botões que a view segura. Quando a view
  sai, ou quando outra conexão toma o lugar, a engine recebe um `Up` de
  cada um, e nenhuma tecla fica presa. Um `Down` de uma tecla nova com 32
  teclas seguras é descartado.

As raízes `ViewToServer` e `ServerToEngine` são diferentes, então o core
decodifica e codifica de novo cada entrada. Isso custa pouco, e é o ponto
onde o número do jogador entra. Um evento de um braço que o `sinteract` do
servidor não conhece só passa depois que o servidor atualiza o
`sinteract`.

### A engine

Depois do `start`, o core olha o braço de cada mensagem do fd 4, o jogador de um frame e os
ids das imagens dele, sem decodificar o resto da cena:

- Um **`asset`** é uma imagem, PNG, JPEG, GIF ou WebP, que os frames
  seguintes desenham pelo `id`. Uma imagem de outro formato ou acima de
  2048×2048 pixels é recusada (`EngineError::Asset`), assim como um `id`
  que já nomeia um asset vivo (`EngineError::LiveId`).
- Um **`frame`** é uma cena inteira, para um jogador ou para todos. Vale só
  o mais novo. Enquanto o WebSocket de um jogador está ocupado, um frame
  novo substitui o que esperava, e uma conexão lenta pula frames sem
  atrasar as outras. O core guarda o último frame de cada jogador, e uma
  view que conecta começa por ele.
- Um frame para um jogador que não existe é um erro
  (`EngineError::NoSeat`).
- Um **`tickTaken`** libera o próximo `tick`.
- Um payload que não decodifica volta como `EngineError::Payload`, e o
  fluxo segue. Um fluxo quebrado volta como `EngineError::Broken`, e a sala
  acaba.

### Os limites das imagens

Cada view decodifica um asset a 4 bytes por pixel, e o servidor guarda os
bytes de cada asset e os manda a cada view. Os assets vivos de uma sala
ficam abaixo de dois limites, 8 imagens de 2048×2048 pixels
(`asset::MAX_LIVE_PIXELS`) e 48 MiB (`asset::MAX_LIVE_BYTES`).

A cada frame, o core marca os assets que o frame desenha e, se a sala
passa dos limites, descarta os que os frames usaram há mais tempo, e os do
frame atual por último. Uma engine que cabe nos limites a cada frame nunca
perde uma imagem que desenha. O core não separa os jogadores, porque as
views de uma sala quase sempre desenham as mesmas imagens. A engine recebe
um `lost` para cada asset descartado. Um asset que não cabe ao lado dos
que chegaram depois do último frame é perdido na hora.

A engine não cuida disso. O `Session::write_frame` manda cada imagem uma
vez, antes do primeiro frame que a desenha, e manda de novo, com outro `id`,
uma imagem que o servidor perdeu. Um frame cujas imagens juntas passam dos
limites não sai (`FrameError::Full`, ou `PresentError::Full` no `Stage`),
porque o servidor perderia uma delas a cada frame. O front end carrega
cada imagem uma vez com `Image::load`, que reduz uma imagem acima de
2048×2048 pixels, e guarda a `Image`.

### O que vai para cada view

O `next_for(conn)` devolve, em ordem:

1. um `forget` para cada asset que a view tem e que nem o frame na tela
   dela nem o próximo frame desenham;
2. os assets do próximo frame que a view ainda não tem;
3. o frame mais novo que a view ainda não recebeu;
4. no fim da sala, depois do último frame, e para uma conexão antiga,
   `Gone`, e `Idle` nos outros casos.

Os assets vêm antes do frame, então um asset chega antes do frame que o
usa. O frame guarda os assets que desenha como eram na chegada, então uma
view atrasada recebe os assets mesmo depois que o core os descartou. O
`forget` mantém a memória da view no limite, como o `lost` faz com a
engine. O `Next::Send` leva um `Arc<[u8]>`, e o frame para todos é um `Arc`
só. O `Bytes::from_owner` o põe numa mensagem do axum sem cópia.

### Como acordar as tasks

O `LobbyCore` ou o `ServerCore` ficam atrás de um `std::sync::Mutex`, e
nenhuma chamada espera.

- A task do fd 3 acorda com um `Notify::notify_one` depois de toda
  chamada que deixa saída, e também depois do `start`. O `notify_waiters`
  perde o aviso quando a task ainda não está esperando.
- As tasks das views acordam com um `watch` da sala toda, depois de
  `from_engine`, `engine_ended`, `leave` e `connect`. Cada task assina o
  `watch` antes do primeiro `next_for`.

A task do fd 3 e as das views não acordam ninguém depois do
`take_engine_output` e do `next_for`, senão uma acordaria a si mesma sem
parar. O esboço abaixo acorda as duas depois de toda outra chamada, o que
é simples e correto.

## Conexão e queda

- A view manda um `resize` como primeiro evento de cada conexão, para que
  a engine saiba o tamanho dela.
- O servidor liga o `TCP_NODELAY` em cada conexão. Sem ele, o algoritmo de
  Nagle segura um frame até o ACK do anterior, e a 60 Hz os frames chegam
  à view em pares, o que dá 30 quadros por segundo em tempos irregulares.
- O servidor usa o ping e o pong do WebSocket. O navegador responde ao
  ping sozinho, até numa aba em segundo plano, então não há mensagem de
  keep-alive.
- Quando a conexão cai, o servidor chama `leave`. O jogador continua na
  partida, e a engine recebe o `Up` das teclas que ele segurava. Quem volta
  traz o mesmo token, o servidor chama `connect`, e a view recebe de novo
  os assets e o frame mais novo do jogador. A view também manda o `up` das
  teclas apertadas quando perde o foco.
- O que o servidor diz à view fora do jogo, como o motivo do fim, vai no
  código e no motivo do close do WebSocket, ou em mensagens de texto. As
  mensagens binárias são só da engine.

## Exemplo completo do servidor

Um esboço com axum que compila com `sinteract` sem features, `tokio`
(feature `full`), `axum` 0.8 (feature `ws`), `bytes`, `futures`, `rand` 0.9
e `command-fds` (feature `tokio`). Ele tem uma task no fd 3, uma no fd 4,
uma no timer do `tick` e uma por WebSocket. O `POST /join?nick=` põe um
apelido no lobby, o `POST /start` começa a partida e devolve o link de
cada jogador, e o `GET /play?token=` abre o WebSocket. A sala guarda a fase
num enum, porque o `LobbyCore::start` consome o lobby e devolve o
`ServerCore`.

```rust
use std::collections::HashMap;
use std::io;
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use bytes::Bytes;
use command_fds::{CommandFdExt, FdMapping};
use futures::{SinkExt, StreamExt};
use sinteract::server::{EngineError, LobbyCore, MAX_VIEW_BYTES, Next, SUBPROTOCOL, ServerCore};
use sinteract::session::{ENGINE_TO_SERVER_FD, Player, SERVER_TO_ENGINE_FD, SESSION_VAR};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};
use tokio::time::MissedTickBehavior;

/// A fase da sala. O `LobbyCore` vira um `ServerCore` no start.
enum Phase {
    Lobby(LobbyCore),
    Game(ServerCore),
    /// A sala acabou antes do start.
    Over,
}

impl Phase {
    fn is_over(&self) -> bool {
        match self {
            Phase::Lobby(_) => false,
            Phase::Game(core) => core.is_over(),
            Phase::Over => true,
        }
    }
}

/// A sala: a fase atrás de um Mutex, os dois jeitos de acordar as tasks, o
/// lobby e o token de cada jogador.
struct Room {
    phase: Mutex<Phase>,
    /// Acorda a task que escreve no fd 3.
    to_engine: Notify,
    /// Acorda as tasks das views.
    views: watch::Sender<()>,
    /// Os apelidos de quem espera o início, na ordem de chegada.
    lobby: Mutex<Vec<String>>,
    /// O jogador de cada token, depois do início.
    tokens: Mutex<HashMap<String, Player>>,
}

impl Room {
    /// Chama o `ServerCore`, ou devolve `None` antes do start e numa sala
    /// que acabou antes dele.
    fn core<T>(&self, f: impl FnOnce(&mut ServerCore) -> T) -> Option<T> {
        match &mut *self.phase.lock().unwrap() {
            Phase::Game(core) => Some(f(core)),
            Phase::Lobby(_) | Phase::Over => None,
        }
    }

    /// Chama o `ServerCore` como o `core`, e acorda quem pode ter algo novo.
    fn with<T>(&self, f: impl FnOnce(&mut ServerCore) -> T) -> Option<T> {
        let out = self.core(f);
        self.to_engine.notify_one();
        self.views.send_replace(());
        out
    }

    fn is_over(&self) -> bool {
        self.phase.lock().unwrap().is_over()
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let (engine_reads, fd3) = io::pipe()?;
    let (fd4, engine_writes) = io::pipe()?;
    let child = {
        // O Command guarda as pontas da engine. Ele sai do escopo aqui, senão
        // o servidor nunca veria o fim do fd 4.
        let mut command = Command::new("sgleam");
        command
            .args(["server", "jogo.gleam"])
            .env(SESSION_VAR, "1")
            .stdin(Stdio::null())
            .process_group(0) // o Ctrl-C do servidor não chega à engine
            .kill_on_drop(true)
            .fd_mappings(vec![
                FdMapping { parent_fd: OwnedFd::from(engine_reads), child_fd: SERVER_TO_ENGINE_FD },
                FdMapping { parent_fd: OwnedFd::from(engine_writes), child_fd: ENGINE_TO_SERVER_FD },
            ])
            .map_err(io::Error::other)?;
        command.spawn()?
    };
    let fd3 = pipe::Sender::from_owned_fd(OwnedFd::from(fd3))?;
    let fd4 = pipe::Receiver::from_owned_fd(OwnedFd::from(fd4))?;
    let room = Arc::new(Room {
        phase: Mutex::new(Phase::Lobby(LobbyCore::new())),
        to_engine: Notify::new(),
        views: watch::channel(()).0,
        lobby: Mutex::default(),
        tokens: Mutex::default(),
    });
    tokio::spawn(write_engine(room.clone(), fd3));
    let reader = tokio::spawn(read_engine(room.clone(), fd4, child));
    tokio::spawn(tick(room.clone()));
    let app = Router::new()
        .route("/join", post(join))
        .route("/start", post(start))
        .route("/play", get(upgrade))
        .with_state(room.clone());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await?
        .tap_io(|tcp| _ = tcp.set_nodelay(true));
    tokio::select! {
        r = axum::serve(listener, app) => r?,
        _ = tokio::signal::ctrl_c() => {
            // No lobby não há tela final, e o kill_on_drop mata a engine.
            if room.with(ServerCore::close).is_some() {
                let _ = reader.await; // a engine ainda manda a tela final
            }
        }
    }
    Ok(())
}

/// Leva ao fd 3 o que o core escreveu para a engine, na ordem do lock. Antes
/// do start não há nada para a engine.
async fn write_engine(room: Arc<Room>, mut fd3: pipe::Sender) {
    let mut buf = Vec::new();
    loop {
        match room.core(|core| core.take_engine_output(&mut buf)) {
            Some(false) => return, // fecha o fd 3, e a engine lê o fim da sessão
            None if room.is_over() => return,
            Some(true) | None => {}
        }
        if buf.is_empty() {
            room.to_engine.notified().await;
        } else if fd3.write_all(&buf).await.is_err() {
            return;
        } else {
            buf.clear();
        }
    }
}

/// Passa os bytes do fd 4 à fase da sala até a sala acabar, e espera a
/// engine sair.
async fn read_engine(room: Arc<Room>, mut fd4: pipe::Receiver, mut child: Child) {
    let mut buf = vec![0; 64 * 1024];
    while !room.is_over() {
        let read = match fd4.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(&buf[..n]),
        };
        for e in from_engine(&room, read) {
            eprintln!("engine: {e}");
        }
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

/// Dá à fase os bytes da engine, ou o fim do fd 4 com `None`, e devolve os
/// erros. No lobby, todo erro e o fim do fd 4 acabam a sala.
fn from_engine(room: &Room, read: Option<&[u8]>) -> Vec<String> {
    let mut phase = room.phase.lock().unwrap();
    let errors = match (std::mem::replace(&mut *phase, Phase::Over), read) {
        (Phase::Lobby(lobby), Some(bytes)) => match lobby.from_engine(bytes) {
            Ok(lobby) => {
                *phase = Phase::Lobby(lobby);
                Vec::new()
            }
            Err(e) => vec![e.to_string()],
        },
        (Phase::Lobby(_), None) => vec!["a engine acabou antes do start".into()],
        (Phase::Game(mut core), read) => {
            let errors: Vec<EngineError> = match read {
                Some(bytes) => core.from_engine(bytes),
                None => core.engine_ended().into_iter().collect(),
            };
            *phase = Phase::Game(core);
            errors.iter().map(ToString::to_string).collect()
        }
        (Phase::Over, _) => Vec::new(),
    };
    drop(phase);
    room.to_engine.notify_one();
    room.views.send_replace(());
    errors
}

/// A taxa do tick em milésimos de hertz, 30 Hz, que o start dá à engine.
const TICK_RATE: NonZeroU32 = NonZeroU32::new(30_000).unwrap();

/// O tick. Um tick atrasado não vira rajada.
async fn tick(room: Arc<Room>) {
    let period = Duration::from_nanos(1_000_000_000_000 / u64::from(TICK_RATE.get()));
    let mut every = tokio::time::interval(period);
    every.set_missed_tick_behavior(MissedTickBehavior::Skip);
    while !room.is_over() {
        every.tick().await;
        room.with(ServerCore::tick);
    }
}

/// Põe um apelido no lobby.
async fn join(Query(q): Query<HashMap<String, String>>, State(room): State<Arc<Room>>) {
    let nickname = q.get("nick").cloned().unwrap_or_default();
    room.lobby.lock().unwrap().push(nickname);
}

/// Começa a partida com quem está no lobby, e devolve o link de cada um.
async fn start(State(room): State<Arc<Room>>) -> Response {
    let lobby = room.lobby.lock().unwrap().clone();
    let mut phase = room.phase.lock().unwrap();
    let Phase::Lobby(core) = std::mem::replace(&mut *phase, Phase::Over) else {
        return (StatusCode::CONFLICT, "a sala não está no lobby").into_response();
    };
    let core = match core.start(&lobby, TICK_RATE) {
        Ok(core) => core,
        Err((core, e)) => {
            *phase = Phase::Lobby(core);
            return (StatusCode::CONFLICT, e.to_string()).into_response();
        }
    };
    let mut tokens = room.tokens.lock().unwrap();
    let mut links = String::new();
    for (player, nickname) in core.players() {
        let token = format!("{:032x}", rand::random::<u128>());
        links += &format!("{nickname}: /play?token={token}\n");
        tokens.insert(token, player);
    }
    *phase = Phase::Game(core);
    drop(phase);
    room.lobby.lock().unwrap().clear();
    room.to_engine.notify_one(); // o start vai para a engine
    links.into_response()
}

async fn upgrade(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    State(room): State<Arc<Room>>,
) -> Response {
    let player = q.get("token").and_then(|t| room.tokens.lock().unwrap().get(t).copied());
    let Some(player) = player else {
        return StatusCode::FORBIDDEN.into_response();
    };
    ws.protocols([SUBPROTOCOL])
        .max_message_size(MAX_VIEW_BYTES)
        .on_upgrade(move |socket| play(socket, room, player))
}

/// Uma view: manda o que o core tem para ela e passa a entrada ao core.
async fn play(socket: WebSocket, room: Arc<Room>, player: Player) {
    let mut wake = room.views.subscribe(); // antes do primeiro next_for
    let Some(conn) = room.with(|core| core.connect(player)).flatten() else {
        return; // a sala acabou
    };
    let (mut sink, mut stream) = socket.split();
    let outgoing = async {
        loop {
            let next = room.core(|core| core.next_for(conn)).unwrap_or(Next::Gone);
            match next {
                Next::Send(payload) => {
                    if sink.send(Ws::Binary(Bytes::from_owner(payload))).await.is_err() {
                        return;
                    }
                }
                Next::Idle => {
                    if wake.changed().await.is_err() {
                        return;
                    }
                }
                Next::Gone => {
                    let _ = sink.close().await;
                    return;
                }
            }
        }
    };
    let incoming = async {
        while let Some(Ok(message)) = stream.next().await {
            let Ws::Binary(payload) = message else { continue };
            if let Some(Err(e)) = room.with(|core| core.from_view(conn, &payload)) {
                eprintln!("jogador {}: {e}", player.number());
            }
        }
    };
    tokio::select! {
        _ = outgoing => {}
        _ = incoming => {}
    }
    room.with(|core| core.leave(conn));
}
```

O esboço deixa de fora:
- o ping do WebSocket;
- o `stdout` e o `stderr` da engine como log;
- quem pode chamar o `start`, e a faixa de `players()` no lobby.

## O lado da engine

A engine abre um `Stage`. Com `SINTERACT_SESSION` no ambiente, o `Stage`
manda o `hello`, espera o `start`, lê o fd 3 e escreve no fd 4. Sem ela,
ele abre uma janela ou o terminal, e o usuário é o jogador 1, então o
mesmo jogo roda sem servidor:

```rust
use sinteract::display::{Stage, StageEvent, TerminalOptions};
use sinteract::event::Interrupt;
use sinteract::session::{PlayerRange, Target};

let players = PlayerRange::new(1, 4).expect("1 a 4 é uma faixa");
let options = TerminalOptions::default();
let (mut stage, jogadores) = Stage::open("jogo", 400.0, 300.0, players, options)?;
mundo.start(&jogadores); // cada Player com o apelido, o jogador 1 primeiro
loop {
    match stage.wait(None) {
        Ok(StageEvent::Tick) => {
            mundo.on_tick(1000.0 / stage.tick_rate().get() as f32); // segundos por tick
            for (p, _) in &jogadores {
                stage.present(Target::Player(*p), mundo.cena_de(*p))?;
            }
        }
        Ok(StageEvent::Input { player, event }) => mundo.on_input(player, event),
        Ok(StageEvent::Error(e)) => eprintln!("{e}"), // mensagem estragada, a sessão segue
        Err(Interrupt::Read(e)) => eprintln!("{e}"),  // o fd 3 quebrou, e o Close vem depois
        Err(Interrupt::Close) => break,               // o fd 3 acabou
        Err(Interrupt::Wake | Interrupt::Timeout) => {}
    }
}
stage.close(); // fecha o fd 4, e a sala acaba
```

O `Tick` vem só do `tick` do servidor, e o `Stage` escreve o `tickTaken`
no fd 4 antes de entregar o `Tick`. Um movimento do mouse ou um `resize`
que ainda não foi lido é substituído pelo mais novo do mesmo jogador, sem
passar por cima da entrada de outro jogador. A sessão guarda os `lost` do
servidor e manda de novo a imagem perdida no próximo frame que a desenha.
Uma escrita que falha no fd 4 fecha o fd 4 e encerra a sessão. O
`tick_rate` dá a taxa do `start` numa sessão, e a taxa do monitor numa
janela, que muda quando a janela passa para outro monitor. Por isso o
jogo lê a taxa de novo a cada `Tick`. O `examples/engine.rs` do
`sinteract` é uma engine completa.

Uma engine sem as features de tela, como uma que roda em wasm, usa a
`session::Session` direto, com o `Session::start(players, r, w)`, o
`tick_rate`, o `wait` e o `write_frame`, sobre o `Read` e o `Write` que ela tiver.

## O que o navegador mostra

Os frames são cenas do `sinteract`, e não SVG. O cliente em `web/` é a
view do navegador. Ele desenha no canvas com a API Canvas 2D e manda a
entrada pelo WebSocket. Há duas páginas, cada uma um HTML só: o
`web/dist/index.html` lê a cena em TypeScript, e o `web/dist/rust.html`
lê e desenha com o próprio crate, compilado para WebAssembly. As duas
abrem o WebSocket em `/play?token=` do mesmo servidor e desenham um frame
por quadro da tela, na ordem em que chegam. O `web/README.md` descreve os módulos, e o `web/server/` é um
servidor de teste, com uma sala e sem lobby, que segue o esboço acima.
