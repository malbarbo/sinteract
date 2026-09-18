# sinteract

A 2D graphics library for [spython](https://github.com/malbarbo/spython)
and [sgleam](https://github.com/malbarbo/sgleam). A program builds a
`Scene`, a list of paths, text, bitmaps and clipped subtrees, and sinteract
shows it in the terminal, in a window, or writes it as PDF or SVG. The
terminal output uses the Kitty graphics protocol, DEC Sixel, or 24-bit
half-blocks, whichever the terminal supports. The PDF and SVG outputs are
vector, with text as glyph outlines.

```rust,no_run
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

sinteract::frontend::terminal::show_image(&scene);
let pdf: Vec<u8> = sinteract::renderer::pdf::render_to_pdf(&scene);
```

`Scene::path` begins a path at a point and returns a scope that commits it
when it is dropped, and `Scene::clip` returns a scope that collects what is
drawn inside it into one clipped element. An arc becomes cubics inside the
scope, so a renderer only sees moves, lines, quadratics and cubics.

For an animation, a `Frontend` owns the terminal or the window, presents a
scene per frame and delivers the input as a stream of `Event`s. A `Sender`
wakes it from another thread with a reply or a close. The
same loop runs over stdio, where the frames go to a view as Cap'n Proto
messages and the input comes back. The schema is in `schema/`, one file for
the drawing, one for the input and one for the session, and `PLAN.md`
describes the server and client modes.

The scene, the rasterizer, the text measuring and the PDF and SVG writers
build on `wasm32`, so a view in a browser can paint a scene without native
code.

## License

MIT or Apache-2.0.
