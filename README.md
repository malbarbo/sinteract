# simage

A small 2D graphics library built around a typed `Scene` (draw list) and a
`Renderer` trait. Front ends build a `simage::scene::Scene` via RAII
builders; renderers replay it through the trait.

Outputs:

- **Raster** — `tiny_skia::Pixmap` via `PixmapRenderer` (in `terminal`).
- **Terminal display** — Kitty graphics protocol, DEC Sixel, or 24-bit
  ANSI half-blocks (`▀`), chosen automatically per the active terminal.
- **PDF** — vector path output via `PdfRenderer`, with text rendered as
  outlined glyph paths.
- **Animation** — terminal alt-screen + raw-mode driver with key polling.

Designed to be shared between teaching tools that need to display geometric
images: originally extracted from
[spython](https://github.com/malbarbo/spython) and intended for use in
[sgleam](https://github.com/malbarbo/sgleam).

## Pipeline

```
front end         simage::scene::Scene            simage::renderer::Renderer
─────────         ────────────────────            ──────────────────────────
build via    →    Vec<Element>             →     PixmapRenderer (terminal)
RAII guards       (Path, Clipped, Text, ...)      PdfRenderer    (PDF)
                                                  your impl      (custom)
```

A `Path` carries `(style, verbs, coords)` — `verbs` is a `Vec<Verb>` (a
`#[repr(u8)]` enum: `Move=0, Line=1, Quad=2, Cubic=3`) consuming 2/2/4/6
floats per verb from `coords`. The wire format carries the same bytes
(`verbMove`/`verbLine`/`verbQuad`/`verbCubic` in `schema/frame.capnp`);
the boundary parses them back into typed verbs and rejects unknown bytes.

Arcs are pre-expanded to cubics in `PathBuilder::arc_to`, so every renderer
only sees move / line / quad / cubic primitives. Paths and clip scopes are
RAII: `Scene::path` returns a `PathBuilder` that commits the path on drop;
`Scene::clip` / `Scene::clip_rect` return a `ClipBuilder` that accumulates
the elements drawn inside the clip and commits a single
`Element::Clipped { clip, elements }` on drop — balanced nesting is
structural, not bookkeeping.

## Example

```rust
use simage::scene::{Scene, PathStyle, Rgba};

let mut scene = Scene::new(40.0, 30.0);
{
    let mut p = scene.path(PathStyle {
        fill: Rgba { r: 0, g: 0, b: 255, a: 1.0 },
        ..PathStyle::default()
    });
    p.move_to(0.0, 0.0);
    p.line_to(40.0, 0.0);
    p.line_to(40.0, 30.0);
    p.line_to(0.0, 30.0);
}

simage::terminal::show_image(&scene);          // terminal
let pdf: Vec<u8> = simage::pdf::render_to_pdf(&scene);
```

## License

Dual-licensed under MIT or Apache-2.0.
