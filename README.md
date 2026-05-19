# simage

A small 2D graphics library built around a typed `Scene` (draw list) and a
`DrawSink` trait. Front ends build a `simage::scene::Scene` via RAII
builders; renderers replay it through the trait.

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
front end         simage::scene::Scene            simage::sink::DrawSink
─────────         ────────────────────            ──────────────────────
build via    →    Vec<DrawNode>            →     PixmapSink   (terminal)
RAII guards       (Path, ClipPush, Text, ...)     PdfSink      (PDF)
                                                  your sink    (custom)
```

A `Path` carries `(style, verbs, coords)` — `verbs` is a flat byte stream
(0=move, 1=line, 2=quad, 3=cubic) consuming 2/2/4/6 floats per verb from
`coords`. Arcs are pre-expanded to cubics in `PathBuilder::arc_to`, so every
renderer only sees move / line / quad / cubic primitives. Paths and clip
scopes are RAII: `Scene::begin_path` returns a `PathBuilder` that commits
the path on drop; `Scene::push_clip` / `push_clip_rect` return a `ClipGuard`
that emits the matching `ClipPop` on drop.

## Example

```rust
use simage::scene::{Scene, PathStyle, Rgba};

let mut scene = Scene::new(40.0, 30.0);
{
    let mut p = scene.begin_path(PathStyle {
        fill: Rgba { r: 0, g: 0, b: 255, a: 1.0 },
        ..PathStyle::default()
    });
    p.move_to(0.0, 0.0);
    p.line_to(40.0, 0.0);
    p.line_to(40.0, 30.0);
    p.line_to(0.0, 30.0);
}

simage::terminal::show_image_dl(&scene);          // terminal
let pdf: Vec<u8> = simage::pdf::render_to_pdf_dl(&scene);
```

## License

Dual-licensed under MIT or Apache-2.0.
