//! A view for an engine that talks the protocol on stdio, such as
//! `examples/engine.rs`. It runs the engine as a subprocess, shows its
//! frames in the terminal or in a window, and sends it the Vsync and the
//! keys. At the end it prints to stderr what the frames cost:
//!
//! ```text
//! cargo build --examples
//! target/debug/examples/view target/debug/examples/engine 200
//! ```

use std::io::{BufReader, BufWriter, Read};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use sinteract::event::{Event, InputEvent};
use sinteract::frontend::{Frontend, Sender, TerminalOptions, open_native};
use sinteract::scene::Scene;
use sinteract::wire::framing::UNROUTED;
use sinteract::wire::to_view::{self, Message};
use sinteract::wire::{ReadError, to_engine};

/// How many messages the reader thread holds before it waits for the loop,
/// so an engine that draws faster than the view does not fill the memory.
const BACKLOG: usize = 4;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((engine, engine_args)) = args.split_first() else {
        eprintln!("usage: view ENGINE [ARGS...]");
        return ExitCode::FAILURE;
    };
    let mut child = match Command::new(engine)
        .args(engine_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("view: cannot run {engine}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut to_engine = BufWriter::new(child.stdin.take().expect("stdin is piped"));
    let from_engine = BufReader::new(child.stdout.take().expect("stdout is piped"));

    // The size of the scene is only known at the first frame, and the
    // engine sends no frame before a Vsync, which needs the frontend open.
    // So the window opens at a guess and letterboxes.
    let mut fr = match open_native("sinteract view", 400.0, 300.0, TerminalOptions::default()) {
        Ok(fr) => fr,
        Err(e) => {
            eprintln!("view: {e}");
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
    };
    let (to_loop, from_reader) = mpsc::sync_channel(BACKLOG);
    let wake = fr.sender();
    thread::spawn(move || read_engine(from_engine, to_loop, wake));

    let mut stats = Stats::default();
    loop {
        match fr.wait_event(None) {
            Event::Input(InputEvent::Close) => {
                let _ = to_engine::write(&mut to_engine, UNROUTED, &InputEvent::Close);
                break;
            }
            Event::Input(ev) => {
                if to_engine::write(&mut to_engine, UNROUTED, &ev).is_err() {
                    break;
                }
            }
            // The Sender of the frontend only carries replies, so the reader
            // thread sends an empty one to wake the loop, and the messages
            // come through the channel.
            Event::Reply { .. } => match drain(fr.as_mut(), &from_reader, &mut stats) {
                Session::Open => {}
                // The engine ended the session, so the view sends no close.
                Session::Closed => break,
            },
            Event::Timeout => {}
        }
    }
    drop(to_engine);
    fr.close();
    drop(fr);
    let _ = child.wait();
    stats.report();
    ExitCode::SUCCESS
}

/// Pass each message of the engine to the loop and wake it. The end of the
/// stream is a close, as the engine had sent one.
fn read_engine(mut from_engine: impl Read, to_loop: SyncSender<Message>, wake: Sender) {
    loop {
        let message = match to_view::read(&mut from_engine) {
            Ok(Some((_, message))) => message,
            Ok(None) => Message::Close,
            Err(ReadError::Payload(e)) => {
                eprintln!("view: skipping a message that does not decode: {e}");
                continue;
            }
            Err(ReadError::Broken(e)) => {
                eprintln!("view: read error: {e}");
                Message::Close
            }
        };
        let last = matches!(message, Message::Close);
        if to_loop.send(message).is_err() || wake.send_reply(0, Vec::new()).is_err() || last {
            return;
        }
    }
}

enum Session {
    Open,
    Closed,
}

/// Act on the messages of the engine that arrived. The assets go to the
/// frontend in order, and only the last frame is shown, since the ones
/// before it are already stale.
fn drain(fr: &mut dyn Frontend, from_reader: &Receiver<Message>, stats: &mut Stats) -> Session {
    let mut last: Option<Scene> = None;
    let mut session = Session::Open;
    for message in from_reader.try_iter() {
        match message {
            Message::Asset { id, blob, mime } => fr.push_asset(id, &blob, mime.as_deref()),
            Message::Frame(scene) => {
                if last.replace(scene).is_some() {
                    stats.skipped += 1;
                }
            }
            Message::Close => {
                session = Session::Closed;
                break;
            }
        }
    }
    if let Some(scene) = last {
        let start = Instant::now();
        fr.present(&scene);
        stats.shown(start.elapsed());
    }
    session
}

#[derive(Default)]
struct Stats {
    frames: u32,
    skipped: u32,
    presenting: Duration,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    fn shown(&mut self, presenting: Duration) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = Some(now);
        self.frames += 1;
        self.presenting += presenting;
    }

    fn report(&self) {
        let span = self
            .first
            .zip(self.last)
            .map_or(0.0, |(first, last)| (last - first).as_secs_f64());
        let fps = if span > 0.0 {
            f64::from(self.frames - 1) / span
        } else {
            0.0
        };
        eprintln!(
            "view: {} frames, {} skipped, {:.1} fps, present {:?} per frame",
            self.frames,
            self.skipped,
            fps,
            self.presenting / self.frames.max(1),
        );
    }
}
