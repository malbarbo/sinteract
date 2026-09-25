//! An engine that runs a game and talks the protocol on fd 3 and fd 4, for
//! `examples/view.rs`. Balls bounce, and the arrows of any player move a
//! paddle, for 1 to 8 players. `q` ends the session from this side. The
//! argument is the number of balls:
//!
//! ```text
//! cargo build --examples
//! target/debug/examples/view target/debug/examples/engine 200
//! ```

// The view hands the engine fd 3 and fd 4, which only unix has. Without a
// `main`, the empty crate needs `no_main`.
#![cfg_attr(not(unix), no_main)]
#![cfg(unix)]

use std::fs::File;
use std::io::BufWriter;
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::time::Instant;

use sinteract::event::{InputEvent, KeyKind, key};
use sinteract::scene::{Paint, PathStyle, Scene};
use sinteract::session::{Session, SessionEvent};
use sinteract::wire::to_view::{self, PlayerRange};

const WIDTH: f32 = 400.0;
const HEIGHT: f32 = 300.0;

fn main() -> ExitCode {
    let balls = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    if std::env::var_os("SINTERACT_SESSION").is_none() {
        eprintln!("engine: run me from a view or a server, which open fd 3 and fd 4");
        return ExitCode::FAILURE;
    }
    // SAFETY: SINTERACT_SESSION says that the parent opened fd 3 and fd 4
    // for the session, and nothing else in this process uses them.
    let (mut from_server, to_view) = unsafe { (File::from_raw_fd(3), File::from_raw_fd(4)) };
    let mut to_view = BufWriter::new(to_view);
    let players = PlayerRange::new(1, 8).expect("1 to 8 is a range");
    if let Err(e) = to_view::write_hello(&mut to_view, players) {
        eprintln!("engine: {e}");
        return ExitCode::FAILURE;
    }
    let mut session = Session::new();
    let mut game = Game::new(balls);
    let mut last = None;
    // The end of fd 4, when the engine exits, ends the session for the
    // server.
    loop {
        let event = match session.wait(&mut from_server) {
            Ok(event) => event,
            Err(e) => {
                eprintln!("engine: {e}");
                break;
            }
        };
        match event {
            SessionEvent::Vsync => {
                let now = Instant::now();
                let dt = last.map_or(0.0, |t: Instant| (now - t).as_secs_f32());
                last = Some(now);
                // A long pause would throw the balls through the walls.
                game.step(dt.min(0.1));
                if let Err(e) = to_view::write_frame(&mut to_view, None, &game.scene()) {
                    eprintln!("engine: {e}");
                    break;
                }
            }
            SessionEvent::Input {
                event: InputEvent::Key(k),
                ..
            } => match k.kind {
                KeyKind::Press if k.key == "q" => break,
                KeyKind::Press => game.key(&k.key),
                KeyKind::Down | KeyKind::Up => {}
            },
            // A message the engine cannot read is a bug in the server, and
            // the game goes on without it.
            SessionEvent::Error(e) => eprintln!("engine: {e}"),
            SessionEvent::End(broken) => {
                if let Some(e) = broken {
                    eprintln!("engine: {e}");
                }
                break;
            }
            // The engine sends no asset yet.
            SessionEvent::Start(_)
            | SessionEvent::Lost(_)
            | SessionEvent::Input {
                event:
                    InputEvent::Mouse(_)
                    | InputEvent::Resize { .. }
                    | InputEvent::Vsync
                    | InputEvent::Pad(_),
                ..
            } => {}
        }
    }
    ExitCode::SUCCESS
}

struct Game {
    balls: Vec<Ball>,
    paddle: f32,
}

struct Ball {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
}

const RADIUS: f32 = 6.0;
const PADDLE_WIDTH: f32 = 60.0;
const PADDLE_STEP: f32 = 20.0;

impl Game {
    /// The balls start spread over the field, each at its own speed.
    fn new(balls: usize) -> Self {
        let balls = (0..balls)
            .map(|i| {
                let i = i as f32;
                Ball {
                    x: RADIUS + (i * 37.0) % (WIDTH - 2.0 * RADIUS),
                    y: RADIUS + (i * 53.0) % (HEIGHT / 2.0),
                    vx: 60.0 + (i * 17.0) % 120.0,
                    vy: 40.0 + (i * 29.0) % 100.0,
                }
            })
            .collect();
        Self {
            balls,
            paddle: (WIDTH - PADDLE_WIDTH) / 2.0,
        }
    }

    fn step(&mut self, dt: f32) {
        for b in &mut self.balls {
            b.x += b.vx * dt;
            b.y += b.vy * dt;
            if b.x < RADIUS || b.x > WIDTH - RADIUS {
                b.vx = -b.vx;
                b.x = b.x.clamp(RADIUS, WIDTH - RADIUS);
            }
            if b.y < RADIUS || b.y > HEIGHT - RADIUS {
                b.vy = -b.vy;
                b.y = b.y.clamp(RADIUS, HEIGHT - RADIUS);
            }
        }
    }

    fn key(&mut self, name: &str) {
        let step = if name == key::ARROW_LEFT {
            -PADDLE_STEP
        } else if name == key::ARROW_RIGHT {
            PADDLE_STEP
        } else {
            return;
        };
        self.paddle = (self.paddle + step).clamp(0.0, WIDTH - PADDLE_WIDTH);
    }

    fn scene(&self) -> Scene {
        let mut scene = Scene::new(WIDTH, HEIGHT);
        let fill = |r, g, b| PathStyle {
            fill: Paint::rgba(r, g, b, 1.0),
            closed: true,
            ..PathStyle::default()
        };
        scene
            .path(fill(20, 20, 40), 0.0, 0.0)
            .line_to(WIDTH, 0.0)
            .line_to(WIDTH, HEIGHT)
            .line_to(0.0, HEIGHT);
        for b in &self.balls {
            scene
                .path(fill(240, 180, 40), b.x - RADIUS, b.y)
                .arc_to(RADIUS, RADIUS, 0.0, false, true, b.x + RADIUS, b.y)
                .arc_to(RADIUS, RADIUS, 0.0, false, true, b.x - RADIUS, b.y);
        }
        let top = HEIGHT - 12.0;
        scene
            .path(fill(80, 200, 120), self.paddle, top)
            .line_to(self.paddle + PADDLE_WIDTH, top)
            .line_to(self.paddle + PADDLE_WIDTH, top + 6.0)
            .line_to(self.paddle, top + 6.0);
        scene
    }
}
