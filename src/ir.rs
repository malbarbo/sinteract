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

/// A bitmap blit. The `id` references a previously-uploaded asset
/// (`Message::Asset` on the wire); the renderer is responsible for
/// resolving it to actual pixels. The geometry is the same as
/// [`ClipBox`] / [`TextNode`]: `(cx, cy)` is the centre, `w`/`h` is the
/// unrotated size, `angle` is in degrees, and `flip_h`/`flip_v` mirror
/// across the local axes.
#[derive(Clone, Copy, Debug, Default)]
pub struct BitmapNode {
    pub id: u32,
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
    pub flip_h: bool,
    pub flip_v: bool,
}

/// Path verb byte. Each verb in [`Path::verbs`] picks how many floats to
/// consume from [`Path::coords`]. Mirrors the wire constants in
/// `schema/frame.capnp`.
pub mod verb {
    pub const MOVE: u8 = 0;
    pub const LINE: u8 = 1;
    pub const QUAD: u8 = 2;
    pub const CUBIC: u8 = 3;
}

/// A materialized 2D path: a style plus a flat verb stream and its
/// floating-point arguments.
///
/// `verbs[i]` pulls 2 (move/line), 4 (quad), or 6 (cubic) floats from
/// `coords` in order. The pair always agrees in length — frontends build
/// it via [`DrawList::path_begin`] / [`DrawList::move_to`] / etc., and the
/// wire decoder rejects mismatched paths.
#[derive(Clone, Debug, Default)]
pub struct Path {
    pub style: PathStyle,
    pub verbs: Vec<u8>,
    pub coords: Vec<f32>,
}

/// One node of a [`DrawList`]. A path bundles all its segments; the rest
/// are leaf operations (clip stack manipulation, a text run, a bitmap blit).
#[derive(Clone, Debug)]
pub enum DrawNode {
    Path(Path),
    ClipPush(ClipBox),
    ClipPop,
    Text(Box<TextNode>),
    Bitmap(BitmapNode),
}

/// Materialized event log produced by Python (or any other front end) and
/// consumed by every renderer. Built incrementally with [`Self::path_begin`]
/// / [`Self::move_to`] / [`Self::line_to`] / etc.; replayed once via
/// [`Self::play_into`].
///
/// A path is opened by [`Self::path_begin`] and committed implicitly by the
/// next path-terminator: a new [`Self::path_begin`], [`Self::clip_push`],
/// [`Self::clip_pop`], [`Self::text`], [`Self::bitmap`], or by
/// [`Self::play_into`] / wire-encoding the list. There is no explicit
/// `path_end`.
///
/// Until a path is committed, it lives in an internal buffer and does **not**
/// appear in [`Self::nodes`]; both [`Self::play_into`] and the wire encoder
/// flush it transparently, so external observers always see a consistent list.
///
/// Arcs entered via [`Self::arc_to`] are pre-expanded to cubics here, so
/// renderers only see line / quad / cubic primitives — same surface as the
/// text-format parser in [`crate::parse`].
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub width: f32,
    pub height: f32,
    /// Committed nodes in draw order. An in-flight path opened by
    /// [`Self::path_begin`] but not yet terminated lives in a private
    /// buffer; it is appended here lazily on the next terminator (or
    /// flushed by [`Self::play_into`] / the wire encoder), so reading
    /// `nodes` directly may not reflect every issued call.
    pub nodes: Vec<DrawNode>,
    open: Option<OpenPath>,
    last_point: Option<(f32, f32)>,
}

#[derive(Clone, Debug)]
struct OpenPath {
    style: PathStyle,
    verbs: Vec<u8>,
    coords: Vec<f32>,
}

/// Tolerance for SVG arc → cubic conversion. Matches [`crate::parse`].
const ARC_TOLERANCE: f64 = 0.1;

impl DrawList {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            nodes: Vec::new(),
            open: None,
            last_point: None,
        }
    }

    pub fn path_begin(&mut self, style: PathStyle) {
        self.commit_open();
        self.open = Some(OpenPath {
            style,
            verbs: Vec::new(),
            coords: Vec::new(),
        });
        self.last_point = None;
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        if let Some(p) = self.open.as_mut() {
            p.verbs.push(verb::MOVE);
            p.coords.extend([x, y]);
        }
        self.last_point = Some((x, y));
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        if let Some(p) = self.open.as_mut() {
            p.verbs.push(verb::LINE);
            p.coords.extend([x, y]);
        }
        self.last_point = Some((x, y));
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        if let Some(p) = self.open.as_mut() {
            p.verbs.push(verb::QUAD);
            p.coords.extend([cx, cy, x, y]);
        }
        self.last_point = Some((x, y));
    }

    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        if let Some(p) = self.open.as_mut() {
            p.verbs.push(verb::CUBIC);
            p.coords.extend([c1x, c1y, c2x, c2y, x, y]);
        }
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
                if let Some(p) = self.open.as_mut() {
                    for el in arc.append_iter(ARC_TOLERANCE) {
                        if let kurbo::PathEl::CurveTo(p1, p2, p3) = el {
                            p.verbs.push(verb::CUBIC);
                            p.coords.extend([
                                p1.x as f32,
                                p1.y as f32,
                                p2.x as f32,
                                p2.y as f32,
                                p3.x as f32,
                                p3.y as f32,
                            ]);
                        }
                    }
                }
                self.last_point = Some((x, y));
            }
            None => {
                self.line_to(x, y);
            }
        }
    }

    pub fn clip_push(&mut self, clip: ClipBox) {
        self.commit_open();
        self.nodes.push(DrawNode::ClipPush(clip));
    }

    pub fn clip_pop(&mut self) {
        self.commit_open();
        self.nodes.push(DrawNode::ClipPop);
    }

    pub fn text(&mut self, node: TextNode) {
        self.commit_open();
        self.nodes.push(DrawNode::Text(Box::new(node)));
    }

    pub fn bitmap(&mut self, node: BitmapNode) {
        self.commit_open();
        self.nodes.push(DrawNode::Bitmap(node));
    }

    fn commit_open(&mut self) {
        if let Some(open) = self.open.take() {
            self.nodes.push(DrawNode::Path(Path {
                style: open.style,
                verbs: open.verbs,
                coords: open.coords,
            }));
        }
    }

    /// Whether a path is currently open (one or more `path_begin` / `*_to`
    /// have happened with no committing terminator yet). Used by the wire
    /// encoder to mirror [`Self::play_into`]'s "trailing open path" handling.
    pub(crate) fn has_open_path(&self) -> bool {
        self.open.is_some()
    }

    /// Borrow the in-flight path's parts, for the wire encoder. Returns
    /// `None` if no path is open.
    pub(crate) fn open_path_parts(&self) -> Option<(&PathStyle, &[u8], &[f32])> {
        self.open
            .as_ref()
            .map(|o| (&o.style, o.verbs.as_slice(), o.coords.as_slice()))
    }

    /// Append a fully-built path. Used by the wire decoder; lets us bypass
    /// the `path_begin / move_to / …` chatter when we've already validated
    /// the verb/coords pair.
    pub(crate) fn push_path(&mut self, path: Path) {
        self.commit_open();
        self.nodes.push(DrawNode::Path(path));
    }

    /// Replay every node into `sink`. Wraps `sink.begin()` and `sink.end()`
    /// around the dispatch loop so callers don't have to. Any path still
    /// open at the time of the call is replayed before `sink.end()`.
    pub fn play_into(&self, sink: &mut dyn crate::sink::DrawSink) {
        sink.begin(self.width, self.height);
        for node in &self.nodes {
            match node {
                DrawNode::Path(p) => play_path(&p.style, &p.verbs, &p.coords, sink),
                DrawNode::ClipPush(b) => sink.clip_push(b),
                DrawNode::ClipPop => sink.clip_pop(),
                DrawNode::Text(t) => sink.text(t),
                DrawNode::Bitmap(b) => sink.bitmap(b),
            }
        }
        if let Some(open) = self.open.as_ref() {
            play_path(&open.style, &open.verbs, &open.coords, sink);
        }
        sink.end();
    }
}

fn play_path(
    style: &PathStyle,
    verbs: &[u8],
    coords: &[f32],
    sink: &mut dyn crate::sink::DrawSink,
) {
    sink.path_begin(style);
    let mut i = 0usize;
    for &v in verbs {
        match v {
            verb::MOVE if coords.len() >= i + 2 => {
                sink.move_to(coords[i], coords[i + 1]);
                i += 2;
            }
            verb::LINE if coords.len() >= i + 2 => {
                sink.line_to(coords[i], coords[i + 1]);
                i += 2;
            }
            verb::QUAD if coords.len() >= i + 4 => {
                sink.quad_to(coords[i], coords[i + 1], coords[i + 2], coords[i + 3]);
                i += 4;
            }
            verb::CUBIC if coords.len() >= i + 6 => {
                sink.cubic_to(
                    coords[i],
                    coords[i + 1],
                    coords[i + 2],
                    coords[i + 3],
                    coords[i + 4],
                    coords[i + 5],
                );
                i += 6;
            }
            // Truncated coords or unknown verb: stop walking; sink already
            // got a path_begin and will get path_end. Decoders reject this
            // up front, so we only reach it on a malformed in-process IR.
            _ => break,
        }
    }
    sink.path_end();
}
