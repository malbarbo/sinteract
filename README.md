# sinteract

A 2D graphics library for [spython](https://github.com/malbarbo/spython)
and [sgleam](https://github.com/malbarbo/sgleam). A program builds a
`Scene`, a list of paths, text, bitmaps and clipped subtrees, and sinteract
shows it in the terminal, in a window, or writes it as PDF or SVG. The
terminal output uses the Kitty graphics protocol, DEC Sixel, or 24-bit
half-blocks, whichever the terminal supports. The PDF and SVG outputs are
vector, with text as glyph outlines.

```rust,no_run
use sinteract::display::Printer;
use sinteract::scene::{Paint, Path, PathStyle, Scene};

let blue = PathStyle {
    fill: Paint::rgba(0, 0, 255, 255),
    ..PathStyle::default()
};
let mut scene = Scene::new(40.0, 30.0);
scene.add_path(
    Path::builder(blue, 0.0, 0.0)
        .line_to(40.0, 0.0)
        .line_to(40.0, 30.0)
        .line_to(0.0, 30.0)
        .build(),
);

if let Ok(mut printer) = Printer::new() {
    let _ = printer.print(&scene);
}
let pdf: Vec<u8> = sinteract::renderer::pdf::render_to_pdf(&scene);
```

`Path::builder` begins a path at a point, and `Scene::add_path` appends
the path that `build` returns. `Scene::clip` collects what its closure
draws into one clipped element. The builder turns an arc into cubics, so
a renderer only sees moves, lines, quadratics and cubics.

A `Printer` prints images where the cursor sits. It fails to open when the
terminal shows no graphics, so a REPL falls back to the text of the value.
A bitmap holds its `Image`, a PNG, a JPEG, a GIF, a WebP or a BMP, so a scene
draws the same in a `Printer`, a `Display` or a document. The fonts of the
text are embedded, so a scene measures and draws the same on every
machine.

For an animation, a `Display` owns the terminal or the window, presents a
scene per frame and delivers the input as a stream of `Event`s, a key, the
mouse, a resize or a tick. A frame that does not reach the display comes
back as a `PresentError`, and a read of the terminal that fails comes back
as an `Interrupt`, both with the error that caused them. The library
writes nothing to stderr and ends no session on its own, so the program
chooses the words and decides whether to stop. A `Sender` wakes the loop
from another thread with a close or a bare wake.

A game runs on a `Stage`, which keeps one loop for two cases. When a
server runs the game, the stage opens a session with the server, reads
the input of each player and writes the frames for the players. When
nobody runs it, the stage opens a window or the terminal, and the user is
player 1:

```rust,ignore
let (mut stage, _players) = Stage::open("My game", 400.0, 300.0, players, options)?;
loop {
    match stage.wait(None) {
        Ok(StageEvent::Tick) => stage.present(Target::All, draw())?,
        Ok(StageEvent::Input { player, event }) => update(player, event),
        Ok(StageEvent::Error(e)) => eprintln!("{e}"),
        Err(Interrupt::Read(e)) => eprintln!("{e}"),
        Err(Interrupt::Close) => break,
        Err(Interrupt::Wake | Interrupt::Timeout) => {}
    }
}
```

A session has three sides. The engine runs the program, the view draws
the frames and sends the input, and the server owns the room and tells
the engine who plays. `session` is the side of the engine, `server` holds
the rules of a room and `view` reads the frames for a view. None of the
three does I/O, so a server that runs on Tokio and a page in a browser
use the same rules. The messages are Cap'n Proto and carry each image
once. The schema is in `schema/`, one file for the drawing, one for the
input and one for the session. A server needs neither the displays nor
the rasterizer, and builds with `default-features = false`.

The scene, the rasterizer, the text, the PDF and SVG writers and the three
sides of a session build on `wasm32`, so a view in a browser paints a
scene without native code. `web/` holds that view, a page in TypeScript
that draws on a canvas, and a second page that draws with the renderer of
the crate compiled to WebAssembly.

## Examples

The top of each example gives the commands that run it, all with
`--release`. `examples/gallery.rs` draws every kind of element, in a
window, in the terminal, at the cursor or as PNG, SVG or PDF:

```sh
cargo run --release --example gallery
cargo run --release --example gallery pdf > gallery.pdf
```

`examples/engine.rs` is a game of balls and paddles for 1 to 8 players,
and `examples/players.rs` is a game where each player moves a square of
its own. Both run on a `Stage`, so each one runs alone in a window, or
in a session. `examples/view.rs` runs an engine as a subprocess and plays
the part of the server for one player:

```sh
cargo build --release --examples
target/release/examples/view target/release/examples/engine 200
```

`web/server/` is a server with one room, which serves the page of `web/`
and prints the link of each player. `examples/remote.rs` plays in a room
of that server from the terminal or a window, with the link of a player:

```sh
make -C web
cargo run --release --manifest-path web/server/Cargo.toml -- \
    --players 2 target/release/examples/players
target/release/examples/remote 'http://127.0.0.1:8765/?token=...'
```

## Other documents

`SERVER.md` is the contract with the server of Sarcade. It says how a
server drives a room with `server`, and what the engine and the view
expect. `PLAN.md` is the plan for the server and client modes of spython
and sgleam. Both are in Portuguese. `web/README.md` describes the HTML
client, its two pages, its tests in Chrome and the test server.
`fonts/README.md` tells where the Sinteract fonts come from and how
`fonts/derive.py` writes them. `AGENTS.md` describes the layout of the
code, the commands that check a change and the rules for the code and the
English of the repository.

## License

MIT or Apache-2.0. The fonts in `fonts/` derive from the Liberation fonts
and are under the SIL Open Font License 1.1, in `fonts/OFL.txt`.
