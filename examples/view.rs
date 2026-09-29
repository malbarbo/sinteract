//! A view for an engine that talks the protocol on fd 3 and fd 4, such as
//! `examples/engine.rs`. It runs the engine as a subprocess and plays the
//! part of the server for one player, with a [`ServerCore`]. At the hello
//! of the engine it starts the room with player 1, shows the frames in the
//! terminal or in a window, and sends the input of the user and a tick for
//! each tick of the display. At the end it prints to stderr what the
//! frames cost:
//!
//! ```text
//! cargo build --examples
//! target/debug/examples/view target/debug/examples/engine 200
//! ```

// The view hands the engine fd 3 and fd 4, which only unix has. Without a
// `main`, the empty crate needs `no_main`.
#![cfg_attr(not(unix), no_main)]
#![cfg(unix)]

use std::fmt;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use command_fds::{CommandFdExt, FdMapping};
use sinteract::display::{Display, PresentError, Sender, TerminalOptions, open_native};
use sinteract::event::{Event, Interrupt};
use sinteract::server::{Conn, Next, ServerCore};
use sinteract::wire::server_to_view::FrameReader;

/// How many reads of the engine the reader thread holds before it waits
/// for the loop, so an engine that draws faster than the view does not
/// fill the memory.
const BACKLOG: usize = 4;

/// How much the view asks of the pipe of the engine at a time.
const READ_BYTES: usize = 64 * 1024;

/// The one player of the room.
const PLAYER: NonZeroU32 = NonZeroU32::MIN;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((engine, engine_args)) = args.split_first() else {
        eprintln!("usage: view ENGINE [ARGS...]");
        return ExitCode::FAILURE;
    };
    let (mut child, mut to_engine, mut from_engine) = match spawn(engine, engine_args) {
        Ok(spawned) => spawned,
        Err(e) => {
            eprintln!("view: cannot run {engine}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut core = ServerCore::new();
    let conn = match start(&mut core, &mut from_engine, &mut to_engine) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("view: {e}");
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
    };

    // The size of the scene is only known at the first frame, and the
    // engine sends no frame before a tick, which needs the display open.
    // So the window opens at a guess and letterboxes.
    let mut display = match open_native("sinteract view", 400.0, 300.0, TerminalOptions::default())
    {
        Ok(display) => display,
        Err(e) => {
            eprintln!("view: {e}");
            let _ = child.kill();
            return ExitCode::FAILURE;
        }
    };
    let (to_loop, from_reader) = mpsc::sync_channel(BACKLOG);
    let wake = display.sender();
    thread::spawn(move || read_engine(from_engine, to_loop, wake));

    let mut reader = FrameReader::new();
    let mut stats = Stats::default();
    loop {
        match display.wait_event(None) {
            Ok(Event::Tick) => {
                stats.tick();
                core.tick();
            }
            Ok(Event::Input(ev)) => core.input(conn, &ev),
            // The bytes of the engine come through the channel, and the
            // reader thread wakes the loop after each read.
            Err(Interrupt::Wake) => {
                take_engine(&mut core, &from_reader);
                if let Err(e) = show(display.as_mut(), &mut core, conn, &mut reader, &mut stats) {
                    if let Some(e) = e {
                        eprintln!("view: {e}");
                    }
                    break;
                }
            }
            Err(Interrupt::Close) => break,
            Err(Interrupt::Read(e)) => eprintln!("view: {e}"),
            Err(Interrupt::Timeout) => {}
        }
        if let Err(e) = send_engine(&mut core, &mut to_engine) {
            eprintln!("view: {e}");
            break;
        }
    }
    // The end of fd 3 tells the engine to end.
    drop(to_engine);
    // The reader thread may wait on a full channel. Without the receiver
    // its send fails and it lets go of the pipe, so an engine that still
    // writes gets an error instead of blocking, and `child.wait` returns.
    drop(from_reader);
    display.close();
    drop(display);
    let _ = child.wait();
    stats.report();
    ExitCode::SUCCESS
}

/// Read the engine up to its hello, start the room with the one player,
/// and send the engine the start.
fn start(
    core: &mut ServerCore,
    from_engine: &mut impl Read,
    to_engine: &mut impl Write,
) -> Result<Conn, String> {
    let mut buf = vec![0; READ_BYTES];
    while core.players().is_none() {
        if core.is_over() {
            return Err("the engine ended before its hello".into());
        }
        match from_engine.read(&mut buf) {
            Ok(0) => {
                if let Some(e) = core.engine_ended() {
                    return Err(e.to_string());
                }
            }
            Ok(n) => {
                for e in core.from_engine(buf.get(..n).expect("a read fits its buffer")) {
                    eprintln!("view: {e}");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    core.start(&["view"]).map_err(|e| e.to_string())?;
    let conn = core.connect(PLAYER).expect("the room has player 1");
    send_engine(core, to_engine).map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Write what the core has for the engine. The view closes the pipe of
/// the engine at its own end, so it does not look at whether the core
/// writes more.
fn send_engine(core: &mut ServerCore, to_engine: &mut impl Write) -> io::Result<()> {
    let mut out = Vec::new();
    core.take_engine_output(&mut out);
    if out.is_empty() {
        return Ok(());
    }
    to_engine.write_all(&out)?;
    to_engine.flush()
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

/// Pass each read of the engine to the loop and wake it. At the end of the
/// stream the channel closes, and a last wake tells the loop.
fn read_engine(mut from_engine: impl Read, to_loop: SyncSender<Vec<u8>>, wake: Sender) {
    let mut buf = vec![0; READ_BYTES];
    loop {
        let n = match from_engine.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("view: read error: {e}");
                break;
            }
        };
        if to_loop
            .send(buf.get(..n).expect("a read fits its buffer").to_vec())
            .is_err()
            || wake.wake().is_err()
        {
            return;
        }
    }
    drop(to_loop);
    let _ = wake.wake();
}

/// Pass the reads of the engine that arrived to the core.
fn take_engine(core: &mut ServerCore, from_reader: &Receiver<Vec<u8>>) {
    loop {
        let errors = match from_reader.try_recv() {
            Ok(bytes) => core.from_engine(&bytes),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => core.engine_ended().into_iter().collect(),
        };
        for e in errors {
            eprintln!("view: {e}");
        }
        if core.is_over() {
            return;
        }
    }
}

/// Show the frames that the core has for the view. `reader` keeps the
/// images of the assets that come before them. Returns `Err` when the view
/// is done, with the error of the display if it failed, or with `None`
/// when the engine ended.
fn show(
    display: &mut dyn Display,
    core: &mut ServerCore,
    conn: Conn,
    reader: &mut FrameReader,
    stats: &mut Stats,
) -> Result<(), Option<PresentError>> {
    loop {
        let payload = match core.next_for(conn) {
            Next::Send(payload) => payload,
            Next::Idle => return Ok(()),
            Next::Gone => return Err(None),
        };
        match reader.read(&payload) {
            Ok(Some(scene)) => {
                let start = Instant::now();
                display.present(scene)?;
                stats.shown(start.elapsed());
            }
            Ok(None) => {}
            Err(e) => eprintln!("view: {e}"),
        }
    }
}

/// What the frames cost, from the first tick to the end.
#[derive(Default)]
struct Stats {
    frames: u32,
    /// The time between two ticks of the display.
    tick_gap: Span,
    present: Span,
    last_tick: Option<Instant>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Stats {
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_tick.replace(now) {
            self.tick_gap.add(now - last);
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
            "view: {} frames, {:.1} fps, tick every {}, present {}",
            self.frames, fps, self.tick_gap, self.present,
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
