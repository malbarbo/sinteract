//! A view that plays in a room of a server over a WebSocket, as the page
//! of the browser does, and shows the frames in a window or in the
//! terminal. It takes the link of a player that the server prints, and
//! connects to `/play` of that server with the token of the link:
//!
//! ```text
//! cargo build --release --examples
//! cargo run --release --manifest-path web/server/Cargo.toml -- \
//!     --players 1 target/release/examples/engine 20
//! target/release/examples/remote 'http://127.0.0.1:8080/?token=...'
//! ```
//!
//! At the end it prints to stderr the rate of the frames and the gaps
//! between them.

// The displays do not build on wasm32. Without a `main`, the empty crate
// needs `no_main`.
#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use std::io::{self, ErrorKind};
use std::net::TcpStream;
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use sinteract::display::{Display, TerminalOptions, open_native};
use sinteract::event::{Event, Interrupt};
use sinteract::view::{FrameReader, SUBPROTOCOL, encode_input};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

/// How many messages the network thread holds before it waits for the
/// loop.
const BACKLOG: usize = 16;

/// How long a read of the socket waits before the network thread sends
/// the input that waits, which bounds the delay of a key.
const POLL: Duration = Duration::from_millis(1);

fn main() -> ExitCode {
    let Some(link) = std::env::args().nth(1) else {
        eprintln!("usage: remote LINK");
        return ExitCode::FAILURE;
    };
    let socket = match connect(&link) {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("remote: {link}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut display =
        match open_native("sinteract remote", 400.0, 300.0, TerminalOptions::default()) {
            Ok(display) => display,
            Err(e) => {
                eprintln!("remote: {e}");
                return ExitCode::FAILURE;
            }
        };
    let (to_loop, from_net) = mpsc::sync_channel(BACKLOG);
    let (to_net, from_loop) = mpsc::channel();
    let wake = display.sender();
    let net = thread::spawn(move || run_socket(socket, to_loop, from_loop, wake));

    let mut reader = FrameReader::new();
    let mut stats = Stats::default();
    loop {
        match display.wait_event(None) {
            // The server keeps the time, so the ticks of the display go
            // nowhere.
            Ok(Event::Tick) => {}
            Ok(Event::Input(ev)) => {
                if to_net.send(encode_input(&ev)).is_err() {
                    break;
                }
            }
            Err(Interrupt::Wake) => {
                if !show(display.as_mut(), &from_net, &mut reader, &mut stats) {
                    break;
                }
            }
            Err(Interrupt::Close) => break,
            Err(Interrupt::Read(e)) => eprintln!("remote: {e}"),
            Err(Interrupt::Timeout) => {}
        }
    }
    // Without the receiver, the network thread fails its next send and
    // closes the socket.
    drop(to_net);
    drop(from_net);
    display.close();
    drop(display);
    let _ = net.join();
    stats.report();
    ExitCode::SUCCESS
}

/// Open the WebSocket at `/play` of the server of `link`, with the token
/// of the link.
fn connect(link: &str) -> Result<WebSocket<TcpStream>, String> {
    let rest = link
        .strip_prefix("http://")
        .ok_or("the link starts with http://")?;
    let (host, query) = rest.split_once('/').unwrap_or((rest, ""));
    let token = query
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))
        .ok_or("the link has no token")?;
    let mut request = format!("ws://{host}/play?token={token}")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    // A key goes out as soon as it is written, as the frames of the server.
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    let (socket, _) = tungstenite::client(request, stream).map_err(|e| e.to_string())?;
    socket
        .get_ref()
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// Pass each binary message of the server to the loop and wake it, and
/// send the input of the loop. A read waits at most [`POLL`], so the input
/// does not wait for a message. At the end of the socket the channel
/// closes, and a last wake tells the loop.
fn run_socket(
    mut socket: WebSocket<TcpStream>,
    to_loop: SyncSender<(Instant, Vec<u8>)>,
    from_loop: Receiver<Vec<u8>>,
    wake: sinteract::display::Sender,
) {
    loop {
        match socket.read() {
            Ok(Message::Binary(payload)) => {
                if to_loop.send((Instant::now(), payload.into())).is_err() || wake.wake().is_err() {
                    let _ = socket.close(None);
                    return;
                }
            }
            Ok(Message::Close(frame)) => {
                if let Some(frame) = frame {
                    eprintln!("remote: the server closed: {}", frame.reason);
                }
                break;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => {
                eprintln!("remote: {e}");
                break;
            }
        }
        if let Err(e) = send_input(&mut socket, &from_loop) {
            if e.kind() != ErrorKind::BrokenPipe {
                eprintln!("remote: {e}");
            }
            let _ = socket.close(None);
            return;
        }
    }
    drop(to_loop);
    let _ = wake.wake();
}

/// Send the input that the loop wrote. Fails with `BrokenPipe` once the
/// loop is gone.
fn send_input(socket: &mut WebSocket<TcpStream>, from_loop: &Receiver<Vec<u8>>) -> io::Result<()> {
    loop {
        match from_loop.try_recv() {
            Ok(event) => socket
                .send(Message::binary(event))
                .map_err(io::Error::other)?,
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Err(ErrorKind::BrokenPipe.into()),
        }
    }
}

/// Show the frames of the messages that arrived. Returns `false` when the
/// server closed the socket.
fn show(
    display: &mut dyn Display,
    from_net: &Receiver<(Instant, Vec<u8>)>,
    reader: &mut FrameReader,
    stats: &mut Stats,
) -> bool {
    loop {
        let (at, payload) = match from_net.try_recv() {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        };
        match reader.read(&payload) {
            Ok(Some(scene)) => {
                stats.arrived(at);
                if let Err(e) = display.present(scene) {
                    eprintln!("remote: {e}");
                    return false;
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("remote: {e}"),
        }
    }
}

/// The frames that came from the server, from the first to the last.
#[derive(Default)]
struct Stats {
    /// The gaps between two frames, in the order that they came.
    gaps: Vec<Duration>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    fn arrived(&mut self, at: Instant) {
        if let Some(last) = self.last.replace(at) {
            self.gaps.push(at - last);
        }
        self.first.get_or_insert(at);
    }

    fn report(&mut self) {
        let span = self
            .first
            .zip(self.last)
            .map_or(0.0, |(first, last)| (last - first).as_secs_f64());
        let frames = self.gaps.len() + usize::from(self.first.is_some());
        let fps = if span > 0.0 {
            self.gaps.len() as f64 / span
        } else {
            0.0
        };
        self.gaps.sort_unstable();
        let at = |q: f64| {
            let i = ((self.gaps.len() as f64 - 1.0) * q).round() as usize;
            self.gaps.get(i).copied().unwrap_or_default()
        };
        eprintln!(
            "remote: {frames} frames, {fps:.1} fps, gap median {:.2?}, p10 {:.2?}, p90 {:.2?}, max {:.2?}",
            at(0.5),
            at(0.1),
            at(0.9),
            at(1.0),
        );
    }
}
