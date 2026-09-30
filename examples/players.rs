//! A game for 1 to 8 players, to try a room with more than one view. Each
//! player moves a square of its own with the arrows, with WASD or with the
//! directional pad of a gamepad, while the key or the button stays down.
//! Each view sees every square with the nickname of its player, and its
//! own square with a white border. A square stops when the view of its
//! player leaves, since the server releases the keys of that view. The
//! square of a player whose view found a gamepad says so under the name.
//!
//! ```text
//! make -C web
//! cargo build --release --examples
//! cargo run --release --manifest-path web/server/Cargo.toml -- \
//!     --players 3 target/release/examples/players
//! target/release/examples/players          # one player, in a window
//! ```
//!
//! The server prints a link for each player, to open in a tab of its own
//! or in `target/release/examples/remote`. A terminal without the keyboard
//! protocol of Kitty sends no `Up`, so the local game wants the window.

// The displays do not build on wasm32. Without a `main`, the empty crate
// needs `no_main`.
#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::process::ExitCode;

use sinteract::display::{Stage, StageEvent, TerminalOptions};
use sinteract::event::{InputEvent, Interrupt, KeyKind, PadButton, PadEvent, key};
use sinteract::scene::{Paint, Path, PathStyle, Rgba, RotatedRect, Scene, Text, TextSpec};
use sinteract::session::{Player, PlayerRange, Target};

const WIDTH: f32 = 480.0;
const HEIGHT: f32 = 360.0;
const PLAYERS: PlayerRange = PlayerRange::new(1, 8).expect("1 to 8 is a range");

fn main() -> ExitCode {
    let options = TerminalOptions::default();
    let (mut stage, players) = match Stage::open("players", WIDTH, HEIGHT, PLAYERS, options) {
        Ok(opened) => opened,
        Err(e) => {
            eprintln!("players: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut game = Game::new(&players);
    loop {
        match stage.wait(None) {
            Ok(StageEvent::Tick) => {
                game.tick(stage.tick_rate());
                for square in &game.squares {
                    let scene = game.scene_for(square.player);
                    if let Err(e) = stage.present(Target::Player(square.player), scene) {
                        eprintln!("players: {e}");
                        stage.close();
                        return ExitCode::FAILURE;
                    }
                }
            }
            Ok(StageEvent::Input { player, event }) => game.input(player, &event),
            // A message the engine cannot read is a bug in the server, and
            // the game goes on without it.
            Ok(StageEvent::Error(e)) => eprintln!("players: {e}"),
            Err(Interrupt::Read(e)) => eprintln!("players: {e}"),
            Err(Interrupt::Close) => break,
            Err(Interrupt::Wake | Interrupt::Timeout) => {}
        }
    }
    stage.close();
    ExitCode::SUCCESS
}

struct Game {
    /// One square for each player, in the order of the start.
    squares: Vec<Square>,
}

/// The square of `player` at (`x`, `y`), its top left corner.
struct Square {
    player: Player,
    nickname: String,
    color: Rgba,
    x: f32,
    y: f32,
    /// The directions that a key or a button of the player holds.
    held: HashSet<Direction>,
    /// Whether the view of the player has a gamepad.
    pad: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Direction {
    Up,
    Down,
    Left,
    Right,
}

const SIDE: f32 = 32.0;
/// The speed of a square in pixels a second.
const SPEED: f32 = 160.0;
const COLORS: [Rgba; 8] = [
    opaque(230, 80, 80),
    opaque(80, 170, 240),
    opaque(90, 200, 110),
    opaque(240, 190, 60),
    opaque(190, 110, 230),
    opaque(60, 210, 200),
    opaque(240, 140, 60),
    opaque(200, 200, 200),
];

impl Game {
    /// The squares start in a row across the middle of the field.
    fn new(players: &[(Player, String)]) -> Self {
        let gap = WIDTH / (players.len() + 1) as f32;
        let squares = players
            .iter()
            .zip(COLORS.iter().cycle())
            .enumerate()
            .map(|(i, ((player, nickname), &color))| Square {
                player: *player,
                nickname: if nickname.is_empty() {
                    format!("player {}", player.number())
                } else {
                    nickname.clone()
                },
                color,
                x: gap * (i + 1) as f32 - SIDE / 2.0,
                y: (HEIGHT - SIDE) / 2.0,
                held: HashSet::new(),
                pad: false,
            })
            .collect();
        Self { squares }
    }

    /// Move each square by one tick at `rate` thousandths of a hertz.
    fn tick(&mut self, rate: NonZeroU32) {
        let step = SPEED * 1000.0 / rate.get() as f32;
        for s in &mut self.squares {
            let axis = |minus, plus| {
                f32::from(u8::from(s.held.contains(&plus)))
                    - f32::from(u8::from(s.held.contains(&minus)))
            };
            let dx = axis(Direction::Left, Direction::Right);
            let dy = axis(Direction::Up, Direction::Down);
            s.x = (s.x + dx * step).clamp(0.0, WIDTH - SIDE);
            s.y = (s.y + dy * step).clamp(0.0, HEIGHT - SIDE);
        }
    }

    fn input(&mut self, player: Player, event: &InputEvent) {
        let Some(square) = self.squares.iter_mut().find(|s| s.player == player) else {
            return;
        };
        match event {
            InputEvent::Key(k) => {
                let Some(dir) = key_direction(&k.key) else {
                    return;
                };
                match k.kind {
                    KeyKind::Down => {
                        square.held.insert(dir);
                    }
                    KeyKind::Up => {
                        square.held.remove(&dir);
                    }
                    KeyKind::Press => {}
                }
            }
            InputEvent::Pad(PadEvent::Down(button)) => {
                if let Some(dir) = pad_direction(*button) {
                    square.held.insert(dir);
                }
            }
            InputEvent::Pad(PadEvent::Up(button)) => {
                if let Some(dir) = pad_direction(*button) {
                    square.held.remove(&dir);
                }
            }
            InputEvent::Pad(PadEvent::Connected) => square.pad = true,
            InputEvent::Pad(PadEvent::Disconnected) => square.pad = false,
            InputEvent::Mouse(_) | InputEvent::Resize { .. } => {}
        }
    }

    /// The field as `player` sees it, with a white border on its square.
    fn scene_for(&self, player: Player) -> Scene {
        let mut scene = Scene::new(WIDTH, HEIGHT);
        scene.add_path(rect(fill(opaque(20, 20, 40)), 0.0, 0.0, WIDTH, HEIGHT));
        for s in &self.squares {
            let style = if s.player == player {
                PathStyle {
                    stroke: Paint::rgba(255, 255, 255, 255),
                    stroke_width: 3.0,
                    ..fill(s.color)
                }
            } else {
                fill(s.color)
            };
            scene.add_path(rect(style, s.x, s.y, SIDE, SIDE));
            let cx = s.x + SIDE / 2.0;
            if let Some(name) = label(&s.nickname, cx, s.y - 10.0) {
                scene.add_text(name);
            }
            if s.pad
                && let Some(pad) = label("pad", cx, s.y + SIDE + 10.0)
            {
                scene.add_text(pad);
            }
        }
        scene
    }
}

fn key_direction(name: &str) -> Option<Direction> {
    match name {
        key::ARROW_UP | "w" | "W" => Some(Direction::Up),
        key::ARROW_DOWN | "s" | "S" => Some(Direction::Down),
        key::ARROW_LEFT | "a" | "A" => Some(Direction::Left),
        key::ARROW_RIGHT | "d" | "D" => Some(Direction::Right),
        _ => None,
    }
}

fn pad_direction(button: PadButton) -> Option<Direction> {
    match button {
        PadButton::DpadUp => Some(Direction::Up),
        PadButton::DpadDown => Some(Direction::Down),
        PadButton::DpadLeft => Some(Direction::Left),
        PadButton::DpadRight => Some(Direction::Right),
        _ => None,
    }
}

fn fill(color: Rgba) -> PathStyle {
    PathStyle {
        fill: Paint::Solid(color),
        closed: true,
        ..PathStyle::default()
    }
}

fn rect(style: PathStyle, x: f32, y: f32, w: f32, h: f32) -> Path {
    Path::builder(style, x, y)
        .line_to(x + w, y)
        .line_to(x + w, y + h)
        .line_to(x, y + h)
        .build()
}

/// A white text of 12 pixels centered at (`cx`, `cy`), or `None` when the
/// text does not measure.
fn label(text: &str, cx: f32, cy: f32) -> Option<Text> {
    let spec = TextSpec {
        size: 12.0,
        text: text.into(),
        ..TextSpec::default()
    };
    let m = sinteract::text::measure(&spec.family, spec.weight, spec.style, spec.size, &spec.text)?;
    let node = spec.fit(RotatedRect {
        cx,
        cy,
        w: m.width(),
        h: m.height(),
        angle_deg: 0.0,
    })?;
    Some(Text {
        fill: Paint::rgba(255, 255, 255, 255),
        ..node
    })
}

const fn opaque(r: u8, g: u8, b: u8) -> Rgba {
    Rgba { r, g, b, a: 255 }
}
