//! An engine that runs a game and talks the protocol on fd 3 and fd 4, for
//! `examples/view.rs`. Balls bounce, and the arrows of any player move a
//! paddle, for 1 to 8 players. The paddle is an image that changes its
//! color at each move, so a new image goes out at each move.
//! `q` ends the session from this side. The first argument is the number
//! of balls. With `big` after it, the image of the paddle has 1024 by 1024
//! pixels, so the 32 that fit in a room fill it after 32 moves, and the
//! view drops the oldest ones:
//!
//! ```text
//! cargo build --examples
//! target/debug/examples/view target/debug/examples/engine 200
//! target/debug/examples/view target/debug/examples/engine 1 big
//! ```

// The view hands the engine fd 3 and fd 4, which only unix has. Without a
// `main`, the empty crate needs `no_main`.
#![cfg_attr(not(unix), no_main)]
#![cfg(unix)]

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::FromRawFd;
use std::process::ExitCode;
use std::time::Instant;

use sinteract::asset::{Asset, Assets};
use sinteract::event::{InputEvent, KeyKind, key};
use sinteract::scene::{Paint, PathStyle, RotatedRect, Scene};
use sinteract::session::{Session, SessionEvent};
use sinteract::wire::to_view::{self, PlayerRange};

const WIDTH: f32 = 400.0;
const HEIGHT: f32 = 300.0;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let balls = args.first().and_then(|n| n.parse().ok()).unwrap_or(1);
    let big = args.get(1).is_some_and(|arg| arg == "big");
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
    let mut game = Game::new(balls, big);
    let mut assets = Assets::new();
    let mut last = None;
    // The end of fd 4, when the engine exits, ends the session for the
    // server.
    loop {
        let event = match session.wait(&mut from_server, &mut to_view) {
            Ok(event) => event,
            Err(e) => {
                eprintln!("engine: {e}");
                break;
            }
        };
        match event {
            SessionEvent::Tick => {
                let now = Instant::now();
                let dt = last.map_or(0.0, |t: Instant| (now - t).as_secs_f32());
                last = Some(now);
                // A long pause would throw the balls through the walls.
                game.step(dt.min(0.1));
                if let Err(e) = draw(&game, &mut assets, &mut to_view) {
                    eprintln!("engine: {e}");
                    break;
                }
            }
            SessionEvent::Lost(id) => assets.lost(id),
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
            SessionEvent::Start(_)
            | SessionEvent::Input {
                event:
                    InputEvent::Mouse(_)
                    | InputEvent::Resize { .. }
                    | InputEvent::Tick
                    | InputEvent::Pad(_),
                ..
            } => {}
        }
    }
    ExitCode::SUCCESS
}

/// Write a frame of `game` for every player, after the images that it
/// draws and that have not gone out.
fn draw(game: &Game, assets: &mut Assets, to_view: &mut impl Write) -> io::Result<()> {
    let paddle = assets.image(&game.paddle_png()).map_err(io::Error::other)?;
    let scene = game.scene(paddle);
    for (id, png) in assets.frame(&scene) {
        to_view::write_asset(to_view, id, &png)?;
    }
    to_view::write_frame(to_view, None, &scene)
}

struct Game {
    balls: Vec<Ball>,
    paddle: f32,
    /// How many times the paddle moved, which picks its color.
    moves: u32,
    /// The size of the image of the paddle in pixels.
    image_size: (u32, u32),
}

struct Ball {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
}

const RADIUS: f32 = 6.0;
const PADDLE_WIDTH: f32 = 60.0;
const PADDLE_HEIGHT: f32 = 6.0;
const PADDLE_STEP: f32 = 20.0;
/// How many colors the paddle takes, more than the 32 images of `big`
/// that fit in a room, so a color comes back after the view dropped it.
const COLORS: u32 = 40;

impl Game {
    /// The balls start spread over the field, each at its own speed. A
    /// `big` game has a large image for the paddle.
    fn new(balls: usize, big: bool) -> Self {
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
            moves: 0,
            image_size: if big {
                (1024, 1024)
            } else {
                (PADDLE_WIDTH as u32, PADDLE_HEIGHT as u32)
            },
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
        self.moves = (self.moves + 1) % COLORS;
    }

    /// The paddle as a PNG, in a green that the moves shift.
    fn paddle_png(&self) -> Vec<u8> {
        let (w, h) = self.image_size;
        let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("the paddle has a size");
        let shade = (self.moves * 3) as u8;
        pixmap.fill(tiny_skia::Color::from_rgba8(
            80,
            200 - shade,
            120 + shade,
            255,
        ));
        pixmap.encode_png().expect("a pixmap encodes")
    }

    /// The field, with the image `paddle` for the paddle.
    fn scene(&self, paddle: Asset) -> Scene {
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
        let rect = RotatedRect {
            cx: self.paddle + PADDLE_WIDTH / 2.0,
            cy: HEIGHT - 12.0 + PADDLE_HEIGHT / 2.0,
            w: PADDLE_WIDTH,
            h: PADDLE_HEIGHT,
            angle_deg: 0.0,
        };
        scene.bitmap(paddle.fit(rect));
        scene
    }
}
