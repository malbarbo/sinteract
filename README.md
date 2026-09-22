# sinteract

A 2D graphics library for [spython](https://github.com/malbarbo/spython)
and [sgleam](https://github.com/malbarbo/sgleam). A program builds a
`Scene`, a list of paths, text, bitmaps and clipped subtrees, and sinteract
shows it in the terminal, in a window, or writes it as PDF or SVG. The
terminal output uses the Kitty graphics protocol, DEC Sixel, or 24-bit
half-blocks, whichever the terminal supports. The PDF and SVG outputs are
vector, with text as glyph outlines.

```rust,no_run
use sinteract::display::terminal::Printer;
use sinteract::scene::{Paint, PathStyle, Scene};

let blue = PathStyle {
    fill: Paint::rgba(0, 0, 255, 1.0),
    ..PathStyle::default()
};
let mut scene = Scene::new(40.0, 30.0);
scene
    .path(blue, 0.0, 0.0)
    .line_to(40.0, 0.0)
    .line_to(40.0, 30.0)
    .line_to(0.0, 30.0);

if let Ok(mut printer) = Printer::new() {
    let _ = printer.print(&scene);
}
let pdf: Vec<u8> = sinteract::renderer::pdf::render_to_pdf(&scene);
```

`Scene::path` begins a path at a point and returns a scope that commits it
when it is dropped, and `Scene::clip` returns a scope that collects what is
drawn inside it into one clipped element. An arc becomes cubics inside the
scope, so a renderer only sees moves, lines, quadratics and cubics.

A `Printer` prints images where the cursor sits. It fails to open when the
terminal shows no graphics, so a REPL falls back to the text of the value.
Its `Assets` hold the PNG images that the bitmaps of the scenes name, and
it keeps them from one print to the next.

For an animation, a `Display` owns the terminal or the window, presents a
scene per frame and delivers the input as a stream of `Event`s. A frame that
does not reach the display comes back as a `PresentError`, and a message of
the peer that does not decode comes back as an `Interrupt`, both with the error
that caused them. The library writes nothing to stderr and ends no session
on its own, so the program chooses the words and decides whether to stop. A
`Sender` wakes it from another thread with a close or a bare wake. The same
loop runs over stdio, where the frames go to a view as Cap'n Proto messages
and the input comes back. The schema is in `schema/`, one file for the
drawing, one for the input and one for the session, and `PLAN.md` describes
the server and client modes.

The scene, the rasterizer, the text measuring and the PDF and SVG writers
build on `wasm32`, so a view in a browser can paint a scene without native
code.

## License

MIT or Apache-2.0.
