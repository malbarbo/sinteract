//! A server to test the HTML client with one room. It runs the engine with
//! fd 3 and fd 4, starts the room with `--players` players as soon as the
//! hello comes, prints the link of each player and serves the page of
//! `web/dist/index.html` at each link. It follows the sketch of SERVER.md,
//! with no lobby:
//!
//! ```text
//! make -C web
//! cargo build --release --examples
//! cargo run --release --manifest-path web/server/Cargo.toml -- \
//!     --players 2 target/release/examples/engine 20
//! ```

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as Ws, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::{Listener, ListenerExt};
use bytes::Bytes;
use command_fds::{CommandFdExt, FdMapping};
use futures::{SinkExt, StreamExt};
use sinteract::server::{LobbyCore, MAX_VIEW_BYTES, Next, SUBPROTOCOL, ServerCore};
use sinteract::session::{ENGINE_TO_SERVER_FD, Player, SERVER_TO_ENGINE_FD, SESSION_VAR};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};
use tokio::time::MissedTickBehavior;

const USAGE: &str = "usage: sinteract-test-server [--addr ADDR] [--players N] \
                     [--page FILE] ENGINE [ARGS...]";

/// The rate of the tick in thousandths of a hertz, 60 Hz, which the start
/// gives the engine.
const TICK_RATE: NonZeroU32 = NonZeroU32::new(60_000).expect("60 Hz is not zero");

/// The period of the tick.
const TICK: Duration = Duration::from_nanos(1_000_000_000_000 / TICK_RATE.get() as u64);

struct Options {
    addr: SocketAddr,
    players: usize,
    page: PathBuf,
    engine: String,
    engine_args: Vec<String>,
}

/// The phase of the room. The lobby becomes a game at the hello.
enum Phase {
    Lobby(LobbyCore),
    Game(Game),
    /// The room ended before the start.
    Over,
}

/// The room after the start, with the player of each token.
struct Game {
    core: ServerCore,
    tokens: HashMap<String, Player>,
}

/// The room: the phase behind a mutex and the two ways to wake the tasks.
struct Room {
    phase: Mutex<Phase>,
    /// Wakes the task that writes to fd 3.
    to_engine: Notify,
    /// Wakes the tasks of the views.
    views: watch::Sender<()>,
    players: usize,
    /// The address of the listener, with its port when `--addr` asks for
    /// port 0.
    addr: SocketAddr,
    page: PathBuf,
}

impl Room {
    fn phase(&self) -> std::sync::MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls the `ServerCore`, or returns `None` before the start and in a
    /// room that ended before it.
    fn core<T>(&self, f: impl FnOnce(&mut ServerCore) -> T) -> Option<T> {
        match &mut *self.phase() {
            Phase::Game(game) => Some(f(&mut game.core)),
            Phase::Lobby(_) | Phase::Over => None,
        }
    }

    /// Calls the `ServerCore` as `core` does, and wakes whoever may have
    /// something new.
    fn with<T>(&self, f: impl FnOnce(&mut ServerCore) -> T) -> Option<T> {
        let out = self.core(f);
        self.wake();
        out
    }

    fn wake(&self) {
        self.to_engine.notify_one();
        self.views.send_replace(());
    }

    fn is_over(&self) -> bool {
        match &*self.phase() {
            Phase::Lobby(_) => false,
            Phase::Game(game) => game.core.is_over(),
            Phase::Over => true,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sinteract-test-server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse(args: Vec<String>) -> Result<Options, String> {
    let mut options = Options {
        addr: SocketAddr::from(([127, 0, 0, 1], 8765)),
        players: 2,
        page: PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../dist/index.html")),
        engine: String::new(),
        engine_args: Vec::new(),
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--addr" => {
                let v = value("--addr")?;
                options.addr = v.parse().map_err(|e| format!("--addr {v}: {e}"))?;
            }
            "--players" => {
                let v = value("--players")?;
                options.players = v.parse().map_err(|e| format!("--players {v}: {e}"))?;
            }
            "--page" => options.page = value("--page")?.into(),
            _ => {
                options.engine = arg;
                options.engine_args = args.collect();
                return Ok(options);
            }
        }
    }
    Err("no engine".into())
}

async fn run(options: Options) -> io::Result<()> {
    let (engine_reads, fd3) = io::pipe()?;
    let (fd4, engine_writes) = io::pipe()?;
    let child = {
        // The Command holds the ends of the engine. It goes out of scope
        // here, or the server would never see the end of fd 4.
        let mut command = Command::new(&options.engine);
        command
            .args(&options.engine_args)
            .env(SESSION_VAR, "1")
            .stdin(Stdio::null())
            // The Ctrl-C of the server does not reach the engine.
            .process_group(0)
            .kill_on_drop(true)
            .fd_mappings(vec![
                FdMapping {
                    parent_fd: OwnedFd::from(engine_reads),
                    child_fd: SERVER_TO_ENGINE_FD,
                },
                FdMapping {
                    parent_fd: OwnedFd::from(engine_writes),
                    child_fd: ENGINE_TO_SERVER_FD,
                },
            ])
            .map_err(io::Error::other)?;
        command.spawn()?
    };
    let fd3 = pipe::Sender::from_owned_fd(OwnedFd::from(fd3))?;
    let fd4 = pipe::Receiver::from_owned_fd(OwnedFd::from(fd4))?;
    // A frame goes out as soon as it is written, so two frames in a row do
    // not wait for each other in one segment.
    let listener = tokio::net::TcpListener::bind(options.addr)
        .await?
        .tap_io(|tcp| _ = tcp.set_nodelay(true));
    let room = Arc::new(Room {
        phase: Mutex::new(Phase::Lobby(LobbyCore::new())),
        to_engine: Notify::new(),
        views: watch::channel(()).0,
        players: options.players,
        addr: listener.local_addr()?,
        page: options.page,
    });
    tokio::spawn(write_engine(room.clone(), fd3));
    let reader = tokio::spawn(read_engine(room.clone(), fd4, child));
    tokio::spawn(tick(room.clone()));
    let app = Router::new()
        .route("/", get(page))
        .route("/play", get(upgrade))
        .with_state(room.clone());
    eprintln!("waiting for the hello of the engine");
    tokio::select! {
        r = axum::serve(listener, app) => r?,
        _ = tokio::signal::ctrl_c() => {
            // The engine still sends its last frames.
            if room.with(ServerCore::close).is_some() {
                let _ = reader.await;
            }
        }
    }
    Ok(())
}

/// Moves to fd 3 what the core wrote for the engine, in the order of the
/// lock.
async fn write_engine(room: Arc<Room>, mut fd3: pipe::Sender) {
    let mut buf = Vec::new();
    loop {
        match room.core(|core| core.take_engine_output(&mut buf)) {
            // Closes fd 3, and the engine reads the end of the session.
            Some(false) => return,
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

/// Passes the bytes of fd 4 to the room until the room is over, and waits
/// for the engine to exit.
async fn read_engine(room: Arc<Room>, mut fd4: pipe::Receiver, mut child: Child) {
    let mut buf = vec![0; 64 * 1024];
    while !room.is_over() {
        let read = match fd4.read(&mut buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => buf.get(..n),
        };
        for e in from_engine(&room, read) {
            eprintln!("engine: {e}");
        }
    }
    eprintln!("the engine ended");
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// Gives the room the bytes of the engine, or the end of fd 4 with `None`,
/// and returns the errors. In the lobby, the room starts at the hello, and
/// every error and the end of fd 4 end the room.
fn from_engine(room: &Room, read: Option<&[u8]>) -> Vec<String> {
    let mut phase = room.phase();
    let (next, errors) = match (std::mem::replace(&mut *phase, Phase::Over), read) {
        (Phase::Lobby(lobby), Some(bytes)) => match lobby.from_engine(bytes) {
            Ok(lobby) if lobby.players().is_some() => start(room, lobby),
            Ok(lobby) => (Phase::Lobby(lobby), Vec::new()),
            Err(e) => (Phase::Over, vec![e.to_string()]),
        },
        (Phase::Lobby(_), None) => (
            Phase::Over,
            vec!["the engine ended before the start".into()],
        ),
        (Phase::Game(mut game), read) => {
            let errors = match read {
                Some(bytes) => game.core.from_engine(bytes),
                None => game.core.engine_ended().into_iter().collect(),
            };
            let errors = errors.iter().map(ToString::to_string).collect();
            (Phase::Game(game), errors)
        }
        (Phase::Over, _) => (Phase::Over, Vec::new()),
    };
    *phase = next;
    drop(phase);
    room.wake();
    errors
}

/// Starts the room with the players of the options, and prints the link of
/// each one.
fn start(room: &Room, lobby: LobbyCore) -> (Phase, Vec<String>) {
    let nicknames: Vec<String> = (1..=room.players).map(|n| format!("player {n}")).collect();
    let core = match lobby.start(&nicknames, TICK_RATE) {
        Ok(core) => core,
        Err((_, e)) => return (Phase::Over, vec![e.to_string()]),
    };
    let mut tokens = HashMap::new();
    for (player, nickname) in core.players() {
        let token = format!("{:032x}", rand::random::<u128>());
        eprintln!("{nickname}: http://{}/?token={token}", room.addr);
        tokens.insert(token, player);
    }
    (Phase::Game(Game { core, tokens }), Vec::new())
}

/// The tick. A late tick does not turn into a burst.
async fn tick(room: Arc<Room>) {
    let mut every = tokio::time::interval(TICK);
    every.set_missed_tick_behavior(MissedTickBehavior::Skip);
    while !room.is_over() {
        every.tick().await;
        room.with(ServerCore::tick);
    }
}

/// The page, read at each request, so a new build shows at a reload.
async fn page(State(room): State<Arc<Room>>) -> Response {
    match tokio::fs::read(&room.page).await {
        Ok(html) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response(),
        Err(e) => {
            let message = format!("{}: {e}; run make -C web", room.page.display());
            (StatusCode::NOT_FOUND, message).into_response()
        }
    }
}

async fn upgrade(
    ws: WebSocketUpgrade,
    Query(q): Query<HashMap<String, String>>,
    State(room): State<Arc<Room>>,
) -> Response {
    let player = q.get("token").and_then(|t| match &*room.phase() {
        Phase::Game(game) => game.tokens.get(t).copied(),
        Phase::Lobby(_) | Phase::Over => None,
    });
    let Some(player) = player else {
        return StatusCode::FORBIDDEN.into_response();
    };
    ws.protocols([SUBPROTOCOL])
        .max_message_size(MAX_VIEW_BYTES)
        .on_upgrade(move |socket| play(socket, room, player))
}

/// A view: sends what the core has for it and passes its input to the core.
async fn play(socket: WebSocket, room: Arc<Room>, player: Player) {
    // Before the first next_for.
    let mut wake = room.views.subscribe();
    let Some(conn) = room.with(|core| core.connect(player)).flatten() else {
        return;
    };
    eprintln!("player {} connected", player.number());
    let (mut sink, mut stream) = socket.split();
    let outgoing = async {
        loop {
            let next = room.core(|core| core.next_for(conn)).unwrap_or(Next::Gone);
            match next {
                Next::Send(payload) => {
                    if sink
                        .send(Ws::Binary(Bytes::from_owner(payload)))
                        .await
                        .is_err()
                    {
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
            let Ws::Binary(payload) = message else {
                continue;
            };
            if let Some(Err(e)) = room.with(|core| core.from_view(conn, &payload)) {
                eprintln!("player {}: {e}", player.number());
            }
        }
    };
    tokio::select! {
        _ = outgoing => {}
        _ = incoming => {}
    }
    room.with(|core| core.leave(conn));
    eprintln!("player {} left", player.number());
}
