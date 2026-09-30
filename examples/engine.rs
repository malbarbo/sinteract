//! An engine that runs a game and talks the protocol on fd 3 and fd 4, for
//! `examples/view.rs`, or shows the game itself when no view runs it.
//! Balls bounce, and the arrows of any player move a paddle, for 1 to 8
//! players. The paddle is an image that changes its
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
//! target/debug/examples/engine 200        # in a window, with no view
//! ```

// The displays do not build on wasm32. Without a `main`, the empty crate
// needs `no_main`.
#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use std::process::ExitCode;
use std::time::Instant;

use sinteract::display::{Stage, StageEvent, TerminalOptions};
use sinteract::event::{InputEvent, Interrupt, KeyKind, key};
use sinteract::scene::{Bitmap, Image, Paint, Path, PathStyle, RotatedRect, Scene};
use sinteract::session::PlayerRange;
use sinteract::session::Target;

const WIDTH: f32 = 400.0;
const HEIGHT: f32 = 300.0;
const PLAYERS: PlayerRange = PlayerRange::new(1, 8).expect("1 to 8 is a range");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let balls = args.first().and_then(|n| n.parse().ok()).unwrap_or(1);
    let big = args.get(1).is_some_and(|arg| arg == "big");
    let mut game = Game::new(balls, big);
    // The example draws the same scene for every player, so it has no use
    // for the players.
    let options = TerminalOptions::default();
    let mut stage = match Stage::open("engine", WIDTH, HEIGHT, PLAYERS, options) {
        Ok((stage, _players)) => stage,
        Err(e) => {
            eprintln!("engine: {e}");
            return ExitCode::FAILURE;
        }
    };
    loop {
        match stage.wait(None) {
            Ok(StageEvent::Tick) => {
                game.tick();
                if let Err(e) = stage.present(Target::All, game.scene()) {
                    eprintln!("engine: {e}");
                    break;
                }
            }
            Ok(StageEvent::Input {
                event: InputEvent::Key(k),
                ..
            }) if k.kind == KeyKind::Press => {
                if k.key == "q" {
                    break;
                }
                game.key(&k.key);
            }
            Ok(StageEvent::Input { .. }) => {}
            // A message the engine cannot read is a bug in the server, and
            // the game goes on without it.
            Ok(StageEvent::Error(e)) => eprintln!("engine: {e}"),
            Err(Interrupt::Read(e)) => eprintln!("engine: {e}"),
            Err(Interrupt::Close) => break,
            Err(Interrupt::Wake | Interrupt::Timeout) => {}
        }
    }
    // In a session, the end of fd 4 ends the room for the server.
    stage.close();
    ExitCode::SUCCESS
}

struct Game {
    balls: Vec<Ball>,
    paddle: f32,
    /// How many times the paddle moved, which picks its color.
    moves: u32,
    /// The size of the image of the paddle in pixels.
    image_size: (u32, u32),
    /// The image of the paddle, for the color of `moves`. A move encodes it
    /// again, and a frame without a move reuses it.
    paddle_image: Image,
    /// When the last tick arrived.
    last_tick: Option<Instant>,
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
        let image_size = if big {
            (1024, 1024)
        } else {
            (PADDLE_WIDTH as u32, PADDLE_HEIGHT as u32)
        };
        Self {
            balls,
            paddle: (WIDTH - PADDLE_WIDTH) / 2.0,
            moves: 0,
            image_size,
            paddle_image: paddle_image(0, image_size),
            last_tick: None,
        }
    }

    /// Move the balls by the time since the last tick.
    fn tick(&mut self) {
        let now = Instant::now();
        let dt = self
            .last_tick
            .replace(now)
            .map_or(0.0, |t| (now - t).as_secs_f32());
        // A long pause would throw the balls through the walls.
        self.step(dt.min(0.1));
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
        self.paddle_image = paddle_image(self.moves, self.image_size);
    }

    /// The field, with the paddle.
    fn scene(&self) -> Scene {
        let mut scene = Scene::new(WIDTH, HEIGHT);
        let fill = |r, g, b| PathStyle {
            fill: Paint::rgba(r, g, b, 255),
            closed: true,
            ..PathStyle::default()
        };
        scene.add_path(
            Path::builder(fill(20, 20, 40), 0.0, 0.0)
                .line_to(WIDTH, 0.0)
                .line_to(WIDTH, HEIGHT)
                .line_to(0.0, HEIGHT)
                .build(),
        );
        for b in &self.balls {
            scene.add_path(
                Path::builder(fill(240, 180, 40), b.x - RADIUS, b.y)
                    .arc_to(RADIUS, RADIUS, 0.0, false, true, b.x + RADIUS, b.y)
                    .arc_to(RADIUS, RADIUS, 0.0, false, true, b.x - RADIUS, b.y)
                    .build(),
            );
        }
        let rect = RotatedRect {
            cx: self.paddle + PADDLE_WIDTH / 2.0,
            cy: HEIGHT - 12.0 + PADDLE_HEIGHT / 2.0,
            w: PADDLE_WIDTH,
            h: PADDLE_HEIGHT,
            angle_deg: 0.0,
        };
        scene.add_bitmap(Bitmap::fit(self.paddle_image.clone(), rect));
        scene
    }
}

/// The paddle of `size` pixels, in a green that `moves` shifts.
/// It encodes with [`png::Compression::Fast`], since the default level of
/// `Pixmap::encode_png` takes about 17 ms for 1024 by 1024 pixels, a
/// whole frame, and this one about 1.5 ms.
fn paddle_image(moves: u32, size: (u32, u32)) -> Image {
    let (w, h) = size;
    let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("the paddle has a size");
    let shade = (moves * 3) as u8;
    pixmap.fill(tiny_skia::Color::from_rgba8(
        80,
        200 - shade,
        120 + shade,
        255,
    ));
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    // The paddle is opaque, so its premultiplied pixels are the straight
    // ones that a PNG holds.
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(pixmap.data()))
        .expect("a pixmap encodes");
    Image::new(png).expect("the paddle is a PNG under the limit")
}
