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

use sinteract::display::{Display, PresentError, Sender, TerminalOptions, Upload, open_native};
use sinteract::event::{Event, NoEvent};
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
    // engine sends no frame before a Vsync, which needs the display open.
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
    let mut warned_bitmaps = false;
    loop {
        match fr.wait_event(None) {
            Ok(Event::Input(ev)) => {
                if to_engine::write_input(&mut to_engine, UNROUTED, &ev).is_err() {
                    break;
                }
            }
            // The messages of the engine come through the channel, and the
            // reader thread wakes the loop after each one.
            Err(NoEvent::Wake) => {
                match drain(fr.as_mut(), &from_reader, &mut stats, &mut warned_bitmaps) {
                    Session::Open => {}
                    Session::EngineClosed => break,
                    Session::DisplayFailed => {
                        let _ = to_engine::write_close(&mut to_engine, UNROUTED);
                        break;
                    }
                }
            }
            Err(NoEvent::Close) => {
                let _ = to_engine::write_close(&mut to_engine, UNROUTED);
                break;
            }
            Err(NoEvent::Damaged(e)) => eprintln!("view: {e}"),
            Err(NoEvent::Broken(e)) => eprintln!("view: {e}"),
            Err(NoEvent::Timeout) => {}
        }
    }
    drop(to_engine);
    // The reader thread may wait on a full channel. Without the receiver
    // its send fails and it lets go of the pipe, so an engine that still
    // writes gets an error instead of blocking, and `child.wait` returns.
    drop(from_reader);
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
        if to_loop.send(message).is_err() || wake.wake().is_err() || last {
            return;
        }
    }
}

enum Session {
    Open,
    /// The engine sent its close, so the view sends none back.
    EngineClosed,
    /// The display of the view failed, and the engine hears nothing of it
    /// until the view closes the session.
    DisplayFailed,
}

/// Act on the messages of the engine that arrived. The assets go to the
/// display in order, and only the last frame is shown, since the ones
/// before it are already stale.
fn drain(
    fr: &mut dyn Display,
    from_reader: &Receiver<Message>,
    stats: &mut Stats,
    warned_bitmaps: &mut bool,
) -> Session {
    let mut last: Option<Scene> = None;
    let mut session = Session::Open;
    for message in from_reader.try_iter() {
        match message {
            Message::Asset { id, blob, mime } => match fr.push_asset(id, &blob, mime.as_deref()) {
                Ok(Upload::Kept) => {}
                Ok(Upload::Dropped) if *warned_bitmaps => {}
                Ok(Upload::Dropped) => {
                    *warned_bitmaps = true;
                    eprintln!("view: this display draws no bitmap, so a frame goes without");
                }
                // The rest of the frame still draws.
                Err(e @ PresentError::Asset(_)) => eprintln!("view: {e}"),
                Err(e) => {
                    eprintln!("view: {e}");
                    return Session::DisplayFailed;
                }
            },
            Message::Frame(scene) => {
                if last.replace(scene).is_some() {
                    stats.skipped += 1;
                }
            }
            Message::Close => {
                session = Session::EngineClosed;
                break;
            }
        }
    }
    if let Some(scene) = last {
        let start = Instant::now();
        if let Err(e) = fr.present(scene) {
            eprintln!("view: {e}");
            return Session::DisplayFailed;
        }
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
