# simage

A small 2D graphics library built around a typed draw-list IR and a
`DrawSink` trait. Front ends build a `simage::ir::DrawList` via builder
methods; renderers replay it through the trait.

Outputs:

- **Raster** — `tiny_skia::Pixmap` via `PixmapSink` (in `terminal`).
- **Terminal display** — Kitty graphics protocol, DEC Sixel, or 24-bit
  ANSI half-blocks (`▀`), chosen automatically per the active terminal.
- **PDF** — vector path output via `PdfSink`, with text rendered as
  outlined glyph paths.
- **Animation** — terminal alt-screen + raw-mode driver with key polling.

Designed to be shared between teaching tools that need to display geometric
images: originally extracted from
[spython](https://github.com/malbarbo/spython) and intended for use in
[sgleam](https://github.com/malbarbo/sgleam).

## Pipeline

```
front end         simage::ir::DrawList            simage::sink::DrawSink
─────────         ────────────────────            ──────────────────────
build via    →    Vec<DrawCmd>             →     PixmapSink   (terminal)
builder           (PathBegin, MoveTo, ...)        PdfSink      (PDF)
methods                                           your sink    (custom)
```

Arcs are pre-expanded to cubics in `DrawList::arc_to`, so every renderer
only sees `MoveTo` / `LineTo` / `QuadTo` / `CubicTo`.

## Example

```rust
use simage::ir::{DrawList, PathStyle, Rgba};

let mut dl = DrawList::new(40.0, 30.0);
dl.path_begin(PathStyle {
    fill: Rgba { r: 0, g: 0, b: 255, a: 1.0 },
    ..PathStyle::default()
});
dl.move_to(0.0, 0.0);
dl.line_to(40.0, 0.0);
dl.line_to(40.0, 30.0);
dl.line_to(0.0, 30.0);
dl.path_end();

simage::terminal::show_image_dl(&dl);            // terminal
let pdf: Vec<u8> = simage::pdf::render_to_pdf_dl(&dl);
```

## License

Dual-licensed under MIT or Apache-2.0.
