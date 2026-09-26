//! A view for an engine that talks the protocol on fd 3 and fd 4, such as
//! `examples/engine.rs`. It runs the engine as a subprocess and plays the
//! part of the server for one player. At the hello of the engine it starts
//! the session with player 1, shows the frames in the terminal or in a
//! window, and sends a tick for each Vsync of the display and the input of
//! the user as player 1. Like a server, it keeps the assets under the
//! limits of a room, and tells the engine which ones it drops. At the end
//! it prints to stderr what the frames cost, and how many assets came and
//! were lost:
//!
//! ```text
//! cargo build --examples
//! target/debug/examples/view target/debug/examples/engine 200
//! ```

// The view hands the engine fd 3 and fd 4, which only unix has. Without a
// `main`, the empty crate needs `no_main`.
#![cfg_attr(not(unix), no_main)]
#![cfg(unix)]

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, BufReader, BufWriter, PipeReader, PipeWriter, Read, Write};
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use command_fds::{CommandFdExt, FdMapping};
use sinteract::asset::{self, Cache, Footprint};
use sinteract::display::{Display, PresentError, Sender, TerminalOptions, open_native};
use sinteract::event::{Event, InputEvent, Interrupt};
use sinteract::scene::Scene;
use sinteract::wire::ReadError;
use sinteract::wire::to_engine::{self, Member, Roster};
use sinteract::wire::to_view::{self, Message};

/// How many messages the reader thread holds before it waits for the loop,
/// so an engine that draws faster than the view does not fill the memory.
const BACKLOG: usize = 4;

/// The one player of the session.
const PLAYER: NonZeroU32 = NonZeroU32::MIN;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((engine, engine_args)) = args.split_first() else {
        eprintln!("usage: view ENGINE [ARGS...]");
        return ExitCode::FAILURE;
    };
    let (mut child, to_engine, from_engine) = match spawn(engine, engine_args) {
        Ok(spawned) => spawned,
        Err(e) => {
            eprintln!("view: cannot run {engine}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut to_engine = BufWriter::new(to_engine);
    let mut from_engine = BufReader::new(from_engine);
    match to_view::read(&mut from_engine) {
        Ok(Some(Message::Hello(takes))) if takes.contains(1) => {}
        Ok(Some(Message::Hello(takes))) => {
            eprintln!(
                "view: the game takes from {} to {} players, not 1",
                takes.min(),
                takes.max()
            );
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
        other => {
            eprintln!("view: the first message of the engine is not a hello: {other:?}");
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
    }
    let roster = Roster::new(vec![Member {
        player: PLAYER,
        nickname: "view".into(),
    }])
    .expect("one player");
    if let Err(e) = to_engine::write_start(&mut to_engine, &roster) {
        eprintln!("view: {e}");
        let _ = child.kill();
        return ExitCode::FAILURE;
    }

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
    let mut cache = Cache::new();
    loop {
        match fr.wait_event(None) {
            Ok(Event::Input(ev)) => {
                let sent = match ev {
                    InputEvent::Vsync => {
                        stats.vsync();
                        to_engine::write_tick(&mut to_engine)
                    }
                    InputEvent::Key(_)
                    | InputEvent::Mouse(_)
                    | InputEvent::Resize { .. }
                    | InputEvent::Pad(_) => to_engine::write_input(&mut to_engine, PLAYER, &ev),
                };
                if sent.is_err() {
                    break;
                }
            }
            // The messages of the engine come through the channel, and the
            // reader thread wakes the loop after each one.
            Err(Interrupt::Wake) => match drain(
                fr.as_mut(),
                &from_reader,
                &mut cache,
                &mut to_engine,
                &mut stats,
            ) {
                Drained::Open => {}
                Drained::EngineEnded | Drained::DisplayFailed => break,
            },
            Err(Interrupt::Close) => break,
            Err(Interrupt::Read(e)) => eprintln!("view: {e}"),
            Err(Interrupt::Timeout) => {}
        }
    }
    // The end of fd 3 tells the engine to end.
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

/// Run `engine` with the session on fd 3 and fd 4, and return the ends of
/// the view. The engine gets no stdin, since the terminal of the view reads
/// the keys from it.
fn spawn(engine: &str, args: &[String]) -> io::Result<(Child, PipeWriter, PipeReader)> {
    let (engine_reads, to_engine) = io::pipe()?;
    let (from_engine, engine_writes) = io::pipe()?;
    // The command holds the ends of the engine and drops them at the end of
    // the statement. A view that kept them would never see the end of the
    // stream.
    let child = Command::new(engine)
        .args(args)
        .env("SINTERACT_SESSION", "1")
        .stdin(Stdio::null())
        .fd_mappings(vec![
            FdMapping {
                parent_fd: OwnedFd::from(engine_reads),
                child_fd: 3,
            },
            FdMapping {
                parent_fd: OwnedFd::from(engine_writes),
                child_fd: 4,
            },
        ])
        .map_err(io::Error::other)?
        .spawn()?;
    Ok((child, to_engine, from_engine))
}

/// Pass each message of the engine to the loop and wake it. At the end of
/// the stream the channel closes, and a last wake tells the loop.
fn read_engine(mut from_engine: impl Read, to_loop: SyncSender<Message>, wake: Sender) {
    loop {
        let message = match to_view::read(&mut from_engine) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(ReadError::Payload(e)) => {
                eprintln!("view: skipping a message that does not decode: {e}");
                continue;
            }
            Err(ReadError::Broken(e)) => {
                eprintln!("view: read error: {e}");
                break;
            }
        };
        if to_loop.send(message).is_err() || wake.wake().is_err() {
            return;
        }
    }
    drop(to_loop);
    let _ = wake.wake();
}

enum Drained {
    Open,
    /// The stream of the engine ended.
    EngineEnded,
    /// The display of the view failed, and the engine hears nothing of it
    /// until the view closes fd 3.
    DisplayFailed,
}

/// Act on the messages of the engine that arrived. The assets go to the
/// display in order, and only the last frame is shown, since the ones
/// before it are already stale. An asset that `cache` drops, or that does
/// not fit, is lost for the engine, and the display forgets it after it
/// shows the frame, since the one on the screen may still draw it.
fn drain(
    fr: &mut dyn Display,
    from_reader: &Receiver<Message>,
    cache: &mut Cache,
    to_engine: &mut impl Write,
    stats: &mut Stats,
) -> Drained {
    let mut last: Option<Scene> = None;
    let mut forgets = Vec::new();
    let mut session = Drained::Open;
    loop {
        let message = match from_reader.try_recv() {
            Ok(message) => message,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                session = Drained::EngineEnded;
                break;
            }
        };
        match message {
            Message::Asset { id, blob } => {
                if cache.contains(id) {
                    eprintln!("view: skipping asset {id}, which is live");
                    continue;
                }
                let kept = Footprint::of(&blob).and_then(|footprint| cache.asset(id, footprint));
                let dropped = match kept {
                    Ok(dropped) => dropped,
                    Err(e) => {
                        eprintln!("view: asset {id}: {e}");
                        vec![id]
                    }
                };
                stats.lost += dropped.len() as u32;
                for gone in dropped {
                    if gone != id {
                        forgets.push(gone);
                    }
                    if to_engine::write_lost(to_engine, gone).is_err() {
                        return Drained::EngineEnded;
                    }
                }
                if !cache.contains(id) {
                    continue;
                }
                stats.assets += 1;
                match fr.push_asset(id, &blob) {
                    Ok(()) => {}
                    // The rest of the frame still draws.
                    Err(e @ PresentError::Asset(_)) => eprintln!("view: {e}"),
                    Err(e) => {
                        eprintln!("view: {e}");
                        return Drained::DisplayFailed;
                    }
                }
            }
            Message::Frame { scene, .. } => {
                stats.frame_arrived();
                cache.frame(None, asset::bitmap_ids(&scene));
                if last.replace(scene).is_some() {
                    stats.skipped += 1;
                }
            }
            Message::Hello(_) => eprintln!("view: skipping a hello after the first one"),
            Message::Forget(_) => eprintln!("view: skipping a forget from the engine"),
        }
    }
    if let Some(scene) = last {
        let start = Instant::now();
        if let Err(e) = fr.present(scene) {
            eprintln!("view: {e}");
            return Drained::DisplayFailed;
        }
        stats.shown(start.elapsed());
    }
    for id in forgets {
        fr.forget_asset(id);
    }
    session
}

/// What the frames cost, from the first Vsync to the end.
#[derive(Default)]
struct Stats {
    frames: u32,
    skipped: u32,
    /// The time between two Vsyncs.
    vsync_gap: Span,
    /// The time from a tick to the frame of the engine for it, which the
    /// view sees when it drains the channel.
    engine: Span,
    present: Span,
    /// When each tick without its frame went out, oldest first.
    ticks: VecDeque<Instant>,
    last_vsync: Option<Instant>,
    /// The assets that the view kept.
    assets: u32,
    /// The assets that the view told the engine it lost.
    lost: u32,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    fn vsync(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_vsync.replace(now) {
            self.vsync_gap.add(now - last);
        }
        self.ticks.push_back(now);
    }

    /// The engine answers each tick with one frame, in order.
    fn frame_arrived(&mut self) {
        if let Some(tick) = self.ticks.pop_front() {
            self.engine.add(tick.elapsed());
        }
    }

    fn shown(&mut self, presenting: Duration) {
        let now = Instant::now();
        self.first.get_or_insert(now);
        self.last = Some(now);
        self.frames += 1;
        self.present.add(presenting);
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
            "view: {} frames, {} skipped, {:.1} fps, {} assets, {} lost\n\
             view: vsync every {}, engine {}, present {}",
            self.frames,
            self.skipped,
            fps,
            self.assets,
            self.lost,
            self.vsync_gap,
            self.engine,
            self.present,
        );
    }
}

/// The mean and the longest of a few durations.
#[derive(Default)]
struct Span {
    total: Duration,
    max: Duration,
    count: u32,
}

impl Span {
    fn add(&mut self, d: Duration) {
        self.total += d;
        self.max = self.max.max(d);
        self.count += 1;
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mean = self.total / self.count.max(1);
        write!(f, "{mean:.2?} (max {:.2?})", self.max)
    }
}
