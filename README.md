# sinteract

A 2D graphics library for [spython](https://github.com/malbarbo/spython)
and [sgleam](https://github.com/malbarbo/sgleam). A program builds a
`Scene`, a list of paths, text, bitmaps and clipped subtrees, and sinteract
shows it in the terminal, in a window, or writes it as PDF. The terminal
output uses the Kitty graphics protocol, DEC Sixel, or 24-bit half-blocks,
whichever the terminal supports. The PDF output is vector, with text as
glyph outlines.

```rust
use sinteract::scene::{Paint, PathStyle, Scene};

let mut scene = Scene::new(40.0, 30.0);
scene
    .path(PathStyle {
        fill: Paint::rgba(0, 0, 255, 1.0),
        ..PathStyle::default()
    })
    .move_to(0.0, 0.0)
    .line_to(40.0, 0.0)
    .line_to(40.0, 30.0)
    .line_to(0.0, 30.0);

sinteract::terminal::show_image(&scene);
let pdf: Vec<u8> = sinteract::pdf::render_to_pdf(&scene);
```

`Scene::path` returns a scope that commits the path when it is dropped, and
`Scene::clip` returns a scope that collects what is drawn inside it into one
clipped element. An arc becomes cubics inside the scope, so a renderer only
sees moves, lines, quadratics and cubics.

For an animation, a `Frontend` owns the terminal or the window, presents a
scene per frame and delivers the input as a stream of `InputEvent`. The
same loop runs over stdio, where the frames go to a peer as Cap'n Proto
messages and the input comes back. The schema is in `schema/`, one file for
the drawing, one for the input and one for the session, and `PLAN.md`
describes the server and client modes.

The scene, the rasterizer, the text measuring and the PDF writer build on
`wasm32`, so a browser client can paint a scene without a native host.

## License

MIT or Apache-2.0.
