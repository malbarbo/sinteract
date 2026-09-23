//! Every kind of element of a `Scene`, one feature per cell, for a look at
//! what a display or a renderer draws. The rotations and the dashes move,
//! so a terminal redraws the frames. `q` or Escape ends it and prints the
//! frames per second.
//!
//! ```text
//! cargo run --example gallery              # a window, or the terminal
//! cargo run --example gallery window       # the window, or why it failed
//! cargo run --example gallery terminal     # the terminal
//! cargo run --example gallery print        # one image at the cursor
//! cargo run --example gallery png > g.png
//! cargo run --example gallery svg > g.svg
//! cargo run --example gallery pdf > g.pdf
//! ```

// The gallery opens a terminal or a window, which wasm32 lacks. Without a
// `main`, the empty crate needs `no_main`.
#![cfg_attr(target_arch = "wasm32", no_main)]
#![cfg(not(target_arch = "wasm32"))]

use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use sinteract::display::{Display, Printer, Terminal, TerminalOptions, Window, open_native};
use sinteract::event::{Event, InputEvent, Interrupt, KeyKind, key};
use sinteract::renderer::Renderer;
use sinteract::renderer::pdf::render_to_pdf;
use sinteract::renderer::pixmap::{PixmapRenderer, render_to_pixmap};
use sinteract::renderer::svg::render_to_svg;
use sinteract::scene::{
    Bitmap, ClipPath, DEFAULT_MITER_LIMIT, Dash, FillRule, FontStyle, LineCap, LineJoin, Paint,
    Path, PathStyle, Rgba, RotatedRect, Scene, SpreadMode, Stop, Text, TextSpec,
};

/// Draws a cell into the box at `(x, y)`, at the time `t` in seconds.
type DrawCell = fn(s: &mut Scene, x: f32, y: f32, t: f32);

/// The cells in reading order, each with its label.
const CELLS: [(&str, DrawCell); 16] = [
    ("fill and stroke", fill_and_stroke),
    ("line quad cubic arc", segments),
    ("caps", caps),
    ("joins", joins),
    ("miter limit 1, default, 6", miter_limit),
    ("fill rules and sub-paths", fill_rules),
    ("dashes", dashes),
    ("linear spread", linear_spread),
    ("radial spread", radial_spread),
    ("gradient stroke", gradient_stroke),
    ("rotated clip", rotated_clip),
    ("path and nested clip", nested_clip),
    ("weights and styles", text_styles),
    ("text transforms", text_transforms),
    ("bitmaps", bitmaps),
    ("families", families),
];
const COLS: usize = 4;
const CELL_W: f32 = 200.0;
const CELL_H: f32 = 150.0;
const WIDTH: f32 = COLS as f32 * CELL_W;
const HEIGHT: f32 = (CELLS.len() / COLS) as f32 * CELL_H;
/// The id of the one bitmap asset.
const BADGE: u32 = 1;
/// The size of the badge image in pixels.
const BADGE_SIZE: (u32, u32) = (32, 24);
/// The time of the still images, which gives every rotation an angle.
const STILL: f32 = 0.7;

fn main() -> ExitCode {
    let mode = std::env::args().nth(1);
    let result = match mode.as_deref() {
        None => open_native(
            "sinteract gallery",
            WIDTH,
            HEIGHT,
            TerminalOptions::default(),
        )
        .map_err(|e| e.to_string())
        .and_then(run),
        Some("window") => Window::open("sinteract gallery", WIDTH, HEIGHT)
            .map_err(|e| e.to_string())
            .and_then(|window| run(Box::new(window))),
        Some("terminal") => Terminal::open()
            .map_err(|e| e.to_string())
            .and_then(|terminal| run(Box::new(terminal))),
        Some("print") => print(),
        Some("png") => png(),
        Some("svg") => std::io::stdout()
            .write_all(render_to_svg(&gallery(STILL)).as_bytes())
            .map_err(|e| e.to_string()),
        Some("pdf") => std::io::stdout()
            .write_all(&render_to_pdf(&gallery(STILL)))
            .map_err(|e| e.to_string()),
        Some(other) => Err(format!(
            "unknown mode {other}, try window, terminal, print, png, svg or pdf"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gallery: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(mut display: Box<dyn Display>) -> Result<(), String> {
    display
        .push_asset(BADGE, &badge_png(), Some("image/png"))
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut frames = 0u32;
    let mut presenting = std::time::Duration::ZERO;
    loop {
        match display.wait_event(None) {
            Ok(Event::Input(InputEvent::Vsync)) => {
                let scene = gallery(start.elapsed().as_secs_f32());
                let before = Instant::now();
                display.present(scene).map_err(|e| e.to_string())?;
                presenting += before.elapsed();
                frames += 1;
            }
            Ok(Event::Input(InputEvent::Key(k)))
                if k.kind == KeyKind::Press && (k.key == "q" || k.key == key::ESCAPE) =>
            {
                break;
            }
            Ok(Event::Input(_)) | Err(Interrupt::Wake | Interrupt::Timeout) => {}
            Err(Interrupt::Read(e)) => eprintln!("gallery: {e}"),
            Err(Interrupt::Close) => break,
        }
    }
    display.close();
    // After the close, so the line goes to the main screen of a terminal.
    let seconds = start.elapsed().as_secs_f64();
    eprintln!(
        "{frames} frames in {seconds:.1} s, {:.1} fps, {:.1} ms per present",
        f64::from(frames) / seconds,
        presenting.as_secs_f64() * 1000.0 / f64::from(frames.max(1))
    );
    Ok(())
}

fn print() -> Result<(), String> {
    let mut printer = Printer::new().map_err(|e| e.to_string())?;
    printer
        .assets_mut()
        .insert_png(BADGE, &badge_png())
        .map_err(|e| e.to_string())?;
    printer.print(&gallery(STILL)).map_err(|e| e.to_string())
}

fn png() -> Result<(), String> {
    let mut renderer =
        PixmapRenderer::new(2.0, WIDTH, HEIGHT).ok_or("the pixmap does not allocate")?;
    renderer
        .assets_mut()
        .insert_png(BADGE, &badge_png())
        .map_err(|e| e.to_string())?;
    let pixmap = renderer
        .render(&gallery(STILL))
        .map_err(|e| e.to_string())?;
    let png = pixmap.encode_png().map_err(|e| e.to_string())?;
    std::io::stdout().write_all(&png).map_err(|e| e.to_string())
}

/// A PNG of four colored quarters and a white dot, drawn by sinteract.
fn badge_png() -> Vec<u8> {
    let (w, h) = (BADGE_SIZE.0 as f32, BADGE_SIZE.1 as f32);
    let mut s = Scene::new(w, h);
    s.add_path(rect(fill(rgb(220, 50, 50)), 0.0, 0.0, w / 2.0, h / 2.0));
    s.add_path(rect(fill(rgb(50, 170, 80)), w / 2.0, 0.0, w / 2.0, h / 2.0));
    s.add_path(rect(fill(rgb(50, 90, 210)), 0.0, h / 2.0, w / 2.0, h / 2.0));
    s.add_path(rect(
        fill(rgb(240, 200, 40)),
        w / 2.0,
        h / 2.0,
        w / 2.0,
        h / 2.0,
    ));
    circle(&mut s, fill(rgb(255, 255, 255)), w / 4.0, h / 4.0, 4.0);
    render_to_pixmap(&s, 1.0)
        .expect("the badge has a size")
        .encode_png()
        .expect("a pixmap encodes as PNG")
}

/// The whole gallery at the time `t`.
fn gallery(t: f32) -> Scene {
    let mut scene = Scene::new(WIDTH, HEIGHT);
    scene.add_path(rect(fill(rgb(250, 248, 240)), 0.0, 0.0, WIDTH, HEIGHT));
    for (i, (name, draw)) in CELLS.iter().enumerate() {
        let x = (i % COLS) as f32 * CELL_W;
        let y = (i / COLS) as f32 * CELL_H;
        scene.add_path(rect(stroke(rgb(200, 200, 200), 1.0), x, y, CELL_W, CELL_H));
        let spec = TextSpec {
            size: 11.0,
            text: (*name).into(),
            ..TextSpec::default()
        };
        if let Some(node) = text(spec, x + CELL_W / 2.0, y + 12.0, 0.0, opaque(90, 90, 90)) {
            scene.text(node);
        }
        draw(&mut scene, x, y + 20.0, t);
    }
    scene
}

fn fill_and_stroke(s: &mut Scene, x: f32, y: f32, _: f32) {
    let both = PathStyle {
        stroke: rgb(20, 40, 120),
        stroke_width: 4.0,
        ..fill(rgb(90, 160, 240))
    };
    s.add_path(rect(both, x + 15.0, y + 15.0, 70.0, 70.0));
    circle(
        s,
        fill(Paint::rgba(230, 60, 60, 0.6)),
        x + 120.0,
        y + 50.0,
        35.0,
    );
    circle(
        s,
        fill(Paint::rgba(60, 200, 90, 0.6)),
        x + 150.0,
        y + 80.0,
        35.0,
    );
    circle(
        s,
        stroke(Paint::rgba(0, 0, 0, 0.5), 2.0),
        x + 135.0,
        y + 65.0,
        50.0,
    );
    // An open path fills as if it were closed.
    s.path(fill(rgb(240, 160, 40)), x + 20.0, y + 125.0)
        .line_to(x + 60.0, y + 95.0)
        .line_to(x + 100.0, y + 125.0);
}

fn segments(s: &mut Scene, x: f32, y: f32, _: f32) {
    let pen = stroke(rgb(40, 40, 40), 3.0);
    s.path(pen.clone(), x + 10.0, y + 20.0)
        .line_to(x + 90.0, y + 20.0);
    s.path(pen.clone(), x + 10.0, y + 60.0)
        .quad_to(x + 50.0, y + 20.0, x + 90.0, y + 60.0);
    s.path(pen.clone(), x + 110.0, y + 20.0).cubic_to(
        x + 130.0,
        y + 80.0,
        x + 170.0,
        y - 20.0,
        x + 190.0,
        y + 40.0,
    );
    s.path(pen, x + 30.0, y + 110.0)
        .arc_to(40.0, 25.0, 0.0, true, true, x + 110.0, y + 110.0);
    // A closed shape of every segment kind.
    let shape = PathStyle {
        closed: true,
        stroke: rgb(120, 40, 140),
        stroke_width: 2.0,
        ..fill(rgb(230, 200, 240))
    };
    s.path(shape, x + 130.0, y + 95.0)
        .line_to(x + 150.0, y + 50.0)
        .quad_to(x + 170.0, y + 40.0, x + 185.0, y + 60.0)
        .cubic_to(
            x + 195.0,
            y + 80.0,
            x + 170.0,
            y + 85.0,
            x + 180.0,
            y + 95.0,
        )
        .arc_to(25.0, 25.0, 0.0, false, true, x + 130.0, y + 95.0);
}

fn caps(s: &mut Scene, x: f32, y: f32, _: f32) {
    let guide = stroke(rgb(230, 60, 60), 1.0);
    for (i, cap) in [LineCap::Butt, LineCap::Round, LineCap::Square]
        .into_iter()
        .enumerate()
    {
        let row = y + 25.0 + i as f32 * 35.0;
        let style = PathStyle {
            line_cap: cap,
            ..stroke(rgb(30, 110, 180), 16.0)
        };
        s.path(style, x + 40.0, row).line_to(x + 160.0, row);
        s.path(guide.clone(), x + 40.0, row).line_to(x + 160.0, row);
    }
    for left in [40.0, 160.0] {
        s.path(guide.clone(), x + left, y + 5.0)
            .line_to(x + left, y + 115.0);
    }
}

fn joins(s: &mut Scene, x: f32, y: f32, _: f32) {
    for (i, join) in [LineJoin::Miter, LineJoin::Round, LineJoin::Bevel]
        .into_iter()
        .enumerate()
    {
        let left = x + 15.0 + i as f32 * 62.0;
        let style = PathStyle {
            line_join: join,
            ..stroke(rgb(200, 120, 30), 12.0)
        };
        chevron(s, style, left, y + 35.0, 50.0, 75.0);
    }
}

fn miter_limit(s: &mut Scene, x: f32, y: f32, _: f32) {
    // The miter of this angle is about 5.8 stroke widths long, so a limit
    // of 1 and the default of 4 bevel it and a limit of 6 keeps the point.
    for (i, limit) in [1.0, DEFAULT_MITER_LIMIT, 6.0].into_iter().enumerate() {
        let style = PathStyle {
            miter_limit: limit,
            ..stroke(rgb(60, 140, 60), 8.0)
        };
        chevron(s, style, x + 30.0 + i as f32 * 55.0, y + 30.0, 30.0, 85.0);
    }
}

fn fill_rules(s: &mut Scene, x: f32, y: f32, _: f32) {
    for (i, rule) in [FillRule::NonZero, FillRule::EvenOdd]
        .into_iter()
        .enumerate()
    {
        let style = PathStyle {
            fill_rule: rule,
            closed: true,
            stroke: rgb(0, 0, 0),
            stroke_width: 1.5,
            ..fill(rgb(240, 180, 40))
        };
        // A star crosses itself.
        let (cx, cy) = (x + 50.0 + i as f32 * 100.0, y + 35.0);
        let point = |k: usize| {
            let angle = (k as f32 * 144.0 - 90.0).to_radians();
            (cx + 32.0 * angle.cos(), cy + 32.0 * angle.sin())
        };
        let (x0, y0) = point(0);
        let mut star = s.path(style.clone(), x0, y0);
        for k in 1..5 {
            let (px, py) = point(k);
            star.line_to(px, py);
        }
        drop(star);
        // Two sub-paths in the same direction make a ring. `closed` closes
        // both of them.
        let (cy, r) = (y + 100.0, 26.0);
        s.path(style, cx - r, cy)
            .arc_to(r, r, 0.0, false, true, cx + r, cy)
            .arc_to(r, r, 0.0, false, true, cx - r, cy)
            .move_to(cx - r / 2.0, cy)
            .arc_to(r / 2.0, r / 2.0, 0.0, false, true, cx + r / 2.0, cy)
            .arc_to(r / 2.0, r / 2.0, 0.0, false, true, cx - r / 2.0, cy);
    }
}

fn dashes(s: &mut Scene, x: f32, y: f32, t: f32) {
    let ants = PathStyle {
        dash: Dash::new([8.0, 4.0], t * 24.0).map(Box::new),
        ..stroke(rgb(20, 20, 20), 2.0)
    };
    s.add_path(rect(ants, x + 15.0, y + 15.0, 80.0, 90.0));
    let dots = PathStyle {
        dash: Dash::new([0.0, 12.0], -t * 24.0).map(Box::new),
        line_cap: LineCap::Round,
        ..stroke(rgb(200, 40, 90), 7.0)
    };
    circle(s, dots, x + 145.0, y + 60.0, 38.0);
    // An odd array repeats itself, so this is 12 4 4 12 4 4, and a
    // gradient paints it.
    let odd = PathStyle {
        dash: Dash::new([12.0, 4.0, 4.0], 0.0).map(Box::new),
        ..stroke(
            Paint::linear(
                x,
                y,
                x + CELL_W,
                y,
                stops(&[(0.0, (40, 120, 200)), (1.0, (200, 40, 90))]),
            ),
            3.0,
        )
    };
    s.path(odd, x + 15.0, y + 120.0)
        .line_to(x + 185.0, y + 120.0);
}

fn linear_spread(s: &mut Scene, x: f32, y: f32, _: f32) {
    for (i, spread) in [SpreadMode::Pad, SpreadMode::Reflect, SpreadMode::Repeat]
        .into_iter()
        .enumerate()
    {
        // The axis covers 50 of the 180 pixels, so the spread fills the rest.
        let paint = Paint::linear(
            x + 75.0,
            0.0,
            x + 125.0,
            0.0,
            stops(&[
                (0.0, (230, 60, 60)),
                (0.5, (250, 220, 60)),
                (1.0, (40, 90, 200)),
            ]),
        )
        .with_spread(spread);
        s.add_path(rect(
            fill(paint),
            x + 10.0,
            y + 5.0 + i as f32 * 40.0,
            180.0,
            32.0,
        ));
    }
}

fn radial_spread(s: &mut Scene, x: f32, y: f32, _: f32) {
    for (i, spread) in [SpreadMode::Pad, SpreadMode::Reflect, SpreadMode::Repeat]
        .into_iter()
        .enumerate()
    {
        let (cx, cy) = (x + 35.0 + i as f32 * 65.0, y + 50.0);
        let paint = Paint::radial(
            cx,
            cy,
            12.0,
            stops(&[(0.0, (255, 255, 255)), (1.0, (120, 40, 160))]),
        )
        .with_spread(spread);
        circle(s, fill(paint), cx, cy, 30.0);
    }
    // The stops fade out, so the circles show through the band.
    let green = opaque(20, 160, 120);
    let fade = Paint::radial(
        x + 100.0,
        y + 90.0,
        90.0,
        vec![
            Stop {
                offset: 0.0,
                color: green,
            },
            Stop {
                offset: 1.0,
                color: Rgba { a: 0.0, ..green },
            },
        ],
    );
    s.add_path(rect(fill(fade), x + 10.0, y + 65.0, 180.0, 50.0));
}

fn gradient_stroke(s: &mut Scene, x: f32, y: f32, _: f32) {
    let rainbow = Paint::linear(
        x + 10.0,
        y,
        x + 190.0,
        y,
        stops(&[
            (0.0, (230, 40, 40)),
            (0.33, (240, 200, 40)),
            (0.66, (40, 180, 90)),
            (1.0, (40, 90, 220)),
        ]),
    );
    let pen = PathStyle {
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..stroke(rainbow, 12.0)
    };
    s.path(pen, x + 20.0, y + 100.0)
        .line_to(x + 60.0, y + 25.0)
        .line_to(x + 100.0, y + 100.0)
        .line_to(x + 140.0, y + 25.0)
        .line_to(x + 180.0, y + 100.0);
}

fn rotated_clip(s: &mut Scene, x: f32, y: f32, t: f32) {
    let window = RotatedRect {
        cx: x + 100.0,
        cy: y + 62.0,
        w: 100.0,
        h: 64.0,
        angle_deg: 20.0 + t * 30.0,
    };
    let mut clip = s.clip(window);
    for i in 0..12 {
        let color = if i % 2 == 0 {
            rgb(40, 60, 160)
        } else {
            rgb(240, 200, 60)
        };
        clip.add_path(rect(fill(color), x + i as f32 * 18.0, y, 18.0, 125.0));
    }
    // A text and a bitmap cross the edge of the clip.
    let spec = TextSpec {
        size: 34.0,
        weight: 700,
        text: "clipped".into(),
        ..TextSpec::default()
    };
    if let Some(node) = text(spec, x + 100.0, y + 45.0, 0.0, opaque(230, 50, 50)) {
        clip.text(node);
    }
    clip.bitmap(badge(x + 135.0, y + 85.0, 2.0, 0.0));
}

fn nested_clip(s: &mut Scene, x: f32, y: f32, _: f32) {
    // Two circles under the even-odd rule make a ring.
    let (cx, cy) = (x + 100.0, y + 62.0);
    let ring = ClipPath::builder(FillRule::EvenOdd, cx - 55.0, cy)
        .arc_to(55.0, 55.0, 0.0, false, true, cx + 55.0, cy)
        .arc_to(55.0, 55.0, 0.0, false, true, cx - 55.0, cy)
        .move_to(cx - 30.0, cy)
        .arc_to(30.0, 30.0, 0.0, false, true, cx + 30.0, cy)
        .arc_to(30.0, 30.0, 0.0, false, true, cx - 30.0, cy)
        .build();
    let mut outer = s.clip(ring);
    let sweep = Paint::linear(
        x + 40.0,
        y,
        x + 160.0,
        y + 125.0,
        stops(&[(0.0, (250, 120, 40)), (1.0, (120, 30, 160))]),
    );
    outer.add_path(rect(fill(sweep), x, y, CELL_W, 125.0));
    // The right half of the ring, inside the ring.
    let half = RotatedRect {
        cx: cx + 40.0,
        cy,
        w: 80.0,
        h: 130.0,
        angle_deg: 0.0,
    };
    let mut inner = outer.clip(half);
    for i in 0..10 {
        let top = y + i as f32 * 13.0;
        inner.add_path(rect(
            fill(Paint::rgba(255, 255, 255, 0.7)),
            cx,
            top,
            80.0,
            5.0,
        ));
    }
}

fn text_styles(s: &mut Scene, x: f32, y: f32, _: f32) {
    let lines = [
        (600, FontStyle::Normal, "Semibold 600"),
        (400, FontStyle::Normal, "Regular 400"),
        (700, FontStyle::Normal, "Bold 700"),
        (400, FontStyle::Italic, "Italic"),
        (700, FontStyle::Oblique, "Bold oblique"),
    ];
    for (i, (weight, style, line)) in lines.into_iter().enumerate() {
        let spec = TextSpec {
            size: 16.0,
            weight,
            style,
            text: line.into(),
            ..TextSpec::default()
        };
        if let Some(node) = text(
            spec,
            x + 60.0,
            y + 12.0 + i as f32 * 24.0,
            0.0,
            opaque(30, 30, 30),
        ) {
            s.text(node);
        }
    }
    let spec = TextSpec {
        size: 16.0,
        text: "Under".into(),
        ..TextSpec::default()
    };
    if let Some(node) = text(spec, x + 155.0, y + 20.0, 0.0, opaque(30, 30, 30)) {
        s.text(Text {
            underline: true,
            ..node
        });
    }
    let spec = TextSpec {
        size: 16.0,
        text: "Faint".into(),
        ..TextSpec::default()
    };
    if let Some(node) = text(
        spec,
        x + 155.0,
        y + 50.0,
        0.0,
        Rgba {
            a: 0.4,
            ..opaque(30, 30, 30)
        },
    ) {
        s.text(node);
    }
}

fn text_transforms(s: &mut Scene, x: f32, y: f32, t: f32) {
    let spec = |line: &str, size| TextSpec {
        size,
        text: line.into(),
        ..TextSpec::default()
    };
    if let Some(node) = text(
        spec("turning", 20.0),
        x + 50.0,
        y + 45.0,
        20.0 + t * 60.0,
        opaque(200, 40, 40),
    ) {
        s.text(node);
    }
    // A negative width mirrors the text, and a negative height flips it.
    let mirror = spec("mirror", 20.0).fit(RotatedRect {
        cx: x + 150.0,
        cy: y + 10.0,
        w: -70.0,
        h: 23.0,
        angle_deg: 0.0,
    });
    if let Some(node) = mirror {
        s.text(Text {
            fill: opaque(40, 90, 200),
            ..node
        });
    }
    let flip = spec("flip", 20.0).fit(RotatedRect {
        cx: x + 150.0,
        cy: y + 38.0,
        w: 40.0,
        h: -23.0,
        angle_deg: 0.0,
    });
    if let Some(node) = flip {
        s.text(Text {
            fill: opaque(40, 90, 200),
            ..node
        });
    }
    // A box of another shape stretches the text.
    let wide = spec("wide", 20.0).fit(RotatedRect {
        cx: x + 50.0,
        cy: y + 115.0,
        w: 80.0,
        h: 16.0,
        angle_deg: 0.0,
    });
    if let Some(node) = wide {
        s.text(Text {
            fill: opaque(40, 140, 60),
            ..node
        });
    }
    let outline = TextSpec {
        weight: 700,
        ..spec("Outline", 30.0)
    };
    if let Some(node) = text(outline, x + 140.0, y + 75.0, 0.0, opaque(250, 220, 60)) {
        s.text(Text {
            stroke: opaque(20, 20, 20),
            stroke_width: 1.2,
            ..node
        });
    }
    // A stroke with no fill.
    if let Some(node) = text(
        spec("hollow", 18.0),
        x + 150.0,
        y + 108.0,
        0.0,
        Rgba::default(),
    ) {
        s.text(Text {
            stroke: opaque(20, 20, 20),
            stroke_width: 0.8,
            ..node
        });
    }
}

fn bitmaps(s: &mut Scene, x: f32, y: f32, t: f32) {
    s.bitmap(badge(x + 40.0, y + 20.0, 1.2, 0.0));
    s.bitmap(badge(x + 130.0, y + 60.0, 2.5, 20.0 + t * 45.0));
    // A negative width mirrors the image, and a negative height flips it.
    let (w, h) = (BADGE_SIZE.0 as f32 * 1.2, BADGE_SIZE.1 as f32 * 1.2);
    let mirrored = RotatedRect {
        cx: x + 40.0,
        cy: y + 62.0,
        w: -w,
        h,
        angle_deg: 0.0,
    };
    s.bitmap(Bitmap::fit(BADGE, BADGE_SIZE.0, BADGE_SIZE.1, mirrored));
    let flipped = RotatedRect {
        cx: x + 40.0,
        cy: y + 104.0,
        w,
        h: -h,
        angle_deg: 0.0,
    };
    s.bitmap(Bitmap::fit(BADGE, BADGE_SIZE.0, BADGE_SIZE.1, flipped));
}

fn families(s: &mut Scene, x: f32, y: f32, _: f32) {
    let lines = [
        ("", "Sans default"),
        ("serif", "Serif"),
        ("mono", "Mono\ttab"),
        ("Liberation Serif", "By full name"),
    ];
    for (i, (family, line)) in lines.into_iter().enumerate() {
        let spec = TextSpec {
            size: 18.0,
            family: family.into(),
            text: line.into(),
            ..TextSpec::default()
        };
        if let Some(node) = text(
            spec,
            x + 100.0,
            y + 15.0 + i as f32 * 28.0,
            0.0,
            opaque(30, 30, 30),
        ) {
            s.text(node);
        }
    }
}

/// The badge, `scale` times its size, centred on `(cx, cy)` and rotated by
/// `angle_deg`.
fn badge(cx: f32, cy: f32, scale: f32, angle_deg: f32) -> Bitmap {
    let (w, h) = BADGE_SIZE;
    let rect = RotatedRect {
        cx,
        cy,
        w: w as f32 * scale,
        h: h as f32 * scale,
        angle_deg,
    };
    Bitmap::fit(BADGE, w, h, rect)
}

/// A text at its natural size in `fill`, centred on `(cx, cy)` and rotated
/// by `angle_deg`.
fn text(spec: TextSpec, cx: f32, cy: f32, angle_deg: f32, fill: Rgba) -> Option<Text> {
    let m = sinteract::text::measure(&spec.family, spec.weight, spec.style, spec.size, &spec.text)?;
    let node = spec.fit(RotatedRect {
        cx,
        cy,
        w: m.width(),
        h: m.height(),
        angle_deg,
    })?;
    Some(Text { fill, ..node })
}

/// Two strokes that meet at the top in a join, inside the box at
/// `(left, top)`.
fn chevron(s: &mut Scene, style: PathStyle, left: f32, top: f32, w: f32, h: f32) {
    s.path(style, left, top + h)
        .line_to(left + w / 2.0, top)
        .line_to(left + w, top + h);
}

fn rect(style: PathStyle, x: f32, y: f32, w: f32, h: f32) -> Path {
    Path::builder(
        PathStyle {
            closed: true,
            ..style
        },
        x,
        y,
    )
    .line_to(x + w, y)
    .line_to(x + w, y + h)
    .line_to(x, y + h)
    .build()
}

fn circle(s: &mut Scene, style: PathStyle, cx: f32, cy: f32, r: f32) {
    s.path(
        PathStyle {
            closed: true,
            ..style
        },
        cx - r,
        cy,
    )
    .arc_to(r, r, 0.0, false, true, cx + r, cy)
    .arc_to(r, r, 0.0, false, true, cx - r, cy);
}

fn fill(paint: Paint) -> PathStyle {
    PathStyle {
        fill: paint,
        ..PathStyle::default()
    }
}

fn stroke(paint: Paint, width: f32) -> PathStyle {
    PathStyle {
        stroke: paint,
        stroke_width: width,
        ..PathStyle::default()
    }
}

fn rgb(r: u8, g: u8, b: u8) -> Paint {
    Paint::Solid(opaque(r, g, b))
}

fn opaque(r: u8, g: u8, b: u8) -> Rgba {
    Rgba { r, g, b, a: 1.0 }
}

fn stops(list: &[(f32, (u8, u8, u8))]) -> Vec<Stop> {
    list.iter()
        .map(|&(offset, (r, g, b))| Stop {
            offset,
            color: opaque(r, g, b),
        })
        .collect()
}
