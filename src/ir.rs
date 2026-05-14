//! Value types shared by the [`crate::sink::DrawSink`] trait and its
//! implementations. These mirror the fields of the line-oriented draw-list
//! format, but in typed form so renderers do not each parse strings.

#[derive(Clone, Copy, Debug, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PathStyle {
    pub fill: Rgba,
    pub stroke: Rgba,
    pub stroke_width: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub fill_rule: FillRule,
    pub closed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LineCap {
    #[default]
    Butt = 0,
    Round = 1,
    Square = 2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum LineJoin {
    #[default]
    Miter = 0,
    Round = 1,
    Bevel = 2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum FillRule {
    #[default]
    NonZero = 0,
    EvenOdd = 1,
}

impl LineCap {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Round,
            2 => Self::Square,
            _ => Self::Butt,
        }
    }
}

impl LineJoin {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Round,
            2 => Self::Bevel,
            _ => Self::Miter,
        }
    }
}

impl FillRule {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::EvenOdd,
            _ => Self::NonZero,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClipBox {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum FontStyle {
    #[default]
    Normal = 0,
    Italic = 1,
    Oblique = 2,
}

impl FontStyle {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Italic,
            2 => Self::Oblique,
            _ => Self::Normal,
        }
    }
}

/// Text node fields. The four corners of the bounding box, after applying
/// `angle`, define where the glyph paths land.
///
/// `family` is the resolved font family — the name of the family the
/// renderer that produced this node actually used to measure the glyphs
/// (after fallback). Empty means "use the renderer's default Sans". On the
/// wire (`simage::wire`) clients honor it so layout stays stable across
/// hosts. `weight` follows CSS conventions (400 = Regular, 700 = Bold).
#[derive(Clone, Debug)]
pub struct TextNode {
    pub fill: Rgba,
    pub stroke: Rgba,
    pub stroke_width: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub cx: f32,
    pub cy: f32,
    pub bw: f32,
    pub bh: f32,
    pub angle: f32,
    pub flip_h: bool,
    pub flip_v: bool,
    pub size: f32,
    pub family: String,
    pub weight: u16,
    pub style: FontStyle,
    pub underline: bool,
    pub text: String,
}

impl Default for TextNode {
    fn default() -> Self {
        Self {
            fill: Rgba::default(),
            stroke: Rgba::default(),
            stroke_width: 0.0,
            line_cap: LineCap::default(),
            line_join: LineJoin::default(),
            cx: 0.0,
            cy: 0.0,
            bw: 0.0,
            bh: 0.0,
            angle: 0.0,
            flip_h: false,
            flip_v: false,
            size: 0.0,
            family: String::new(),
            weight: 400,
            style: FontStyle::Normal,
            underline: false,
            text: String::new(),
        }
    }
}

/// A materialized draw command. Variants mirror [`crate::sink::DrawSink`]
/// methods one-to-one; replaying a [`DrawList`] dispatches each command to
/// the matching method. Arcs are pre-expanded to cubics at append time.
#[derive(Clone, Debug)]
pub enum DrawCmd {
    PathBegin(PathStyle),
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    PathEnd,
    ClipPush(ClipBox),
    ClipPop,
    Text(Box<TextNode>),
    Bitmap,
}

/// Materialized event log produced by Python (or any other front end) and
/// consumed by every renderer. Built incrementally with [`Self::path_begin`],
/// [`Self::move_to`], etc.; replayed once via [`Self::play_into`].
///
/// Arcs entered via [`Self::arc_to`] are pre-expanded to cubics here, so
/// renderers only see line / quad / cubic primitives — same surface as the
/// text-format parser in [`crate::parse`].
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub width: f32,
    pub height: f32,
    pub cmds: Vec<DrawCmd>,
    last_point: Option<(f32, f32)>,
}

/// Tolerance for SVG arc → cubic conversion. Matches [`crate::parse`].
const ARC_TOLERANCE: f64 = 0.1;

impl DrawList {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            cmds: Vec::new(),
            last_point: None,
        }
    }

    pub fn path_begin(&mut self, style: PathStyle) {
        self.cmds.push(DrawCmd::PathBegin(style));
        self.last_point = None;
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.cmds.push(DrawCmd::MoveTo(x, y));
        self.last_point = Some((x, y));
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.cmds.push(DrawCmd::LineTo(x, y));
        self.last_point = Some((x, y));
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.cmds.push(DrawCmd::QuadTo(cx, cy, x, y));
        self.last_point = Some((x, y));
    }

    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.cmds.push(DrawCmd::CubicTo(c1x, c1y, c2x, c2y, x, y));
        self.last_point = Some((x, y));
    }

    /// Append an SVG endpoint arc, pre-expanding to cubic segments. Mirrors
    /// the text parser's `A` handling: degenerate arcs collapse to a line.
    #[allow(clippy::too_many_arguments)]
    pub fn arc_to(
        &mut self,
        rx: f32,
        ry: f32,
        rotation_deg: f32,
        large_arc: bool,
        sweep: bool,
        x: f32,
        y: f32,
    ) {
        let Some((x1, y1)) = self.last_point else {
            // No current point — fall back to a move so the renderer is in a
            // valid state. The text parser silently drops this case; we mirror.
            self.move_to(x, y);
            return;
        };
        let svg_arc = kurbo::SvgArc {
            from: kurbo::Point::new(x1 as f64, y1 as f64),
            to: kurbo::Point::new(x as f64, y as f64),
            radii: kurbo::Vec2::new(rx as f64, ry as f64),
            x_rotation: (rotation_deg as f64).to_radians(),
            large_arc,
            sweep,
        };
        match kurbo::Arc::from_svg_arc(&svg_arc) {
            Some(arc) => {
                for el in arc.append_iter(ARC_TOLERANCE) {
                    if let kurbo::PathEl::CurveTo(p1, p2, p3) = el {
                        self.cmds.push(DrawCmd::CubicTo(
                            p1.x as f32,
                            p1.y as f32,
                            p2.x as f32,
                            p2.y as f32,
                            p3.x as f32,
                            p3.y as f32,
                        ));
                    }
                }
                self.last_point = Some((x, y));
            }
            None => {
                self.line_to(x, y);
            }
        }
    }

    pub fn path_end(&mut self) {
        self.cmds.push(DrawCmd::PathEnd);
        self.last_point = None;
    }

    pub fn clip_push(&mut self, clip: ClipBox) {
        self.cmds.push(DrawCmd::ClipPush(clip));
    }

    pub fn clip_pop(&mut self) {
        self.cmds.push(DrawCmd::ClipPop);
    }

    pub fn text(&mut self, node: TextNode) {
        self.cmds.push(DrawCmd::Text(Box::new(node)));
    }

    pub fn bitmap(&mut self) {
        self.cmds.push(DrawCmd::Bitmap);
    }

    /// Replay every command into `sink`. Wraps `sink.begin()` and
    /// `sink.end()` around the dispatch loop so callers don't have to.
    pub fn play_into(&self, sink: &mut dyn crate::sink::DrawSink) {
        sink.begin(self.width, self.height);
        let mut in_path = false;
        for cmd in &self.cmds {
            match cmd {
                DrawCmd::PathBegin(s) => {
                    if in_path {
                        sink.path_end();
                    }
                    sink.path_begin(s);
                    in_path = true;
                }
                DrawCmd::MoveTo(x, y) => sink.move_to(*x, *y),
                DrawCmd::LineTo(x, y) => sink.line_to(*x, *y),
                DrawCmd::QuadTo(cx, cy, x, y) => sink.quad_to(*cx, *cy, *x, *y),
                DrawCmd::CubicTo(c1x, c1y, c2x, c2y, x, y) => {
                    sink.cubic_to(*c1x, *c1y, *c2x, *c2y, *x, *y)
                }
                DrawCmd::PathEnd => {
                    sink.path_end();
                    in_path = false;
                }
                DrawCmd::ClipPush(b) => {
                    if in_path {
                        sink.path_end();
                        in_path = false;
                    }
                    sink.clip_push(b);
                }
                DrawCmd::ClipPop => {
                    if in_path {
                        sink.path_end();
                        in_path = false;
                    }
                    sink.clip_pop();
                }
                DrawCmd::Text(t) => {
                    if in_path {
                        sink.path_end();
                        in_path = false;
                    }
                    sink.text(t);
                }
                DrawCmd::Bitmap => {
                    if in_path {
                        sink.path_end();
                        in_path = false;
                    }
                    sink.bitmap();
                }
            }
        }
        if in_path {
            sink.path_end();
        }
        sink.end();
    }
}
