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
    fill: Paint::rgba(0, 0, 255, 1.0),
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
A bitmap holds its `Image`, a PNG, a JPEG, a GIF or a WebP, so a scene
draws the same in a `Printer`, a `Display` or a document.

For an animation, a `Display` owns the terminal or the window, presents a
scene per frame and delivers the input as a stream of `Event`s. A frame that
does not reach the display comes back as a `PresentError`, and a read of the
terminal that fails comes back as an `Interrupt`, both with the error that
caused them. The library writes nothing to stderr and ends no session on its
own, so the program chooses the words and decides whether to stop. A
`Sender` wakes it from another thread with a close or a bare wake. In a
session with a server, the engine reads the input of the players with a
`Session` and writes its frames with it, as Cap'n Proto messages that
carry each image once. The schema is in `schema/`, one file for the
drawing, one for the input and one for the session, and `PLAN.md`
describes the server and client modes.

The scene, the rasterizer, the text measuring and the PDF and SVG writers
build on `wasm32`, so a view in a browser can paint a scene without native
code.

## License

MIT or Apache-2.0. The fonts in `fonts/` are the Liberation fonts, under
the SIL Open Font License 1.1, in `fonts/OFL.txt`.
