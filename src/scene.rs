//! Value types shared by the [`crate::renderer::Renderer`] trait and its
//! implementations.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

/// One color stop in a gradient. `offset` is in [0, 1].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stop {
    pub offset: f32,
    pub color: Rgba,
}

/// How a gradient extends past its defined axis (CSS/SVG `spreadMethod`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum SpreadMode {
    /// Hold the boundary stop colors past the axis.
    #[default]
    Pad = 0,
    /// Mirror the gradient around each axis end.
    Reflect = 1,
    /// Tile the gradient periodically.
    Repeat = 2,
}

impl SpreadMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Reflect,
            2 => Self::Repeat,
            _ => Self::Pad,
        }
    }
}

/// Linear gradient from (x0, y0) to (x1, y1) in path-local coordinates.
/// The paint is rendered along the line; `stops` are pre-sorted by offset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinearGradient {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub stops: Vec<Stop>,
    pub spread: SpreadMode,
}

/// Radial gradient centred at (cx, cy) with the given radius.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RadialGradient {
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    pub stops: Vec<Stop>,
    pub spread: SpreadMode,
}

/// Fill or stroke paint — solid color or gradient.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Solid(Rgba),
    Linear(LinearGradient),
    Radial(RadialGradient),
}

impl Default for Paint {
    fn default() -> Self {
        Self::Solid(Rgba::default())
    }
}

impl Paint {
    /// Convenience: solid paint from raw bytes.
    pub fn rgba(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self::Solid(Rgba { r, g, b, a })
    }

    /// Whether this paint would draw at least one visible pixel. Used by
    /// renderers as a fast cull (skip both the fill and stroke when neither
    /// would mark the canvas).
    pub fn is_visible(&self) -> bool {
        match self {
            Self::Solid(c) => c.a > 0.0,
            Self::Linear(g) => !g.stops.is_empty(),
            Self::Radial(g) => !g.stops.is_empty(),
        }
    }

    /// First stop's color for gradients, or the solid color. Used as the
    /// fallback when a renderer cannot honor gradients (or as a tint for
    /// effects keyed on a single color).
    pub fn primary_color(&self) -> Rgba {
        match self {
            Self::Solid(c) => *c,
            Self::Linear(g) => g.stops.first().map(|s| s.color).unwrap_or_default(),
            Self::Radial(g) => g.stops.first().map(|s| s.color).unwrap_or_default(),
        }
    }
}

/// SVG `stroke-miterlimit` default. Joins with computed miter length above
/// this threshold (relative to stroke width) fall back to bevel.
pub const DEFAULT_MITER_LIMIT: f32 = 4.0;

#[derive(Clone, Debug, PartialEq)]
pub struct PathStyle {
    pub fill: Paint,
    pub stroke: Paint,
    pub stroke_width: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub fill_rule: FillRule,
    pub closed: bool,
    pub miter_limit: f32,
    /// Empty = solid stroke. Otherwise on/off lengths in path units; the
    /// pattern repeats when consumed.
    pub dash_array: Vec<f32>,
    pub dash_offset: f32,
}

impl Default for PathStyle {
    fn default() -> Self {
        Self {
            fill: Paint::default(),
            stroke: Paint::default(),
            stroke_width: 0.0,
            line_cap: LineCap::default(),
            line_join: LineJoin::default(),
            fill_rule: FillRule::default(),
            closed: false,
            miter_limit: DEFAULT_MITER_LIMIT,
            dash_array: Vec::new(),
            dash_offset: 0.0,
        }
    }
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

/// Arbitrary clip region described by a verb/coord path (same encoding as
/// [`Path`]). Sub-paths are treated as implicitly closed — callers do not
/// have to add a final line back to the starting point. `fill_rule` decides
/// which sub-regions count as "inside".
///
/// `verbs`/`coords` are private and always agree in length — the only way to
/// build one is [`ClipPath::builder`], which pushes verbs and coords together
/// so a mismatch is structurally impossible. Read the geometry back via
/// [`ClipPath::segments`].
#[derive(Clone, Debug, Default)]
pub struct ClipPath {
    verbs: Vec<SegmentKind>,
    coords: Vec<f32>,
    pub fill_rule: FillRule,
}

impl ClipPath {
    /// Start building a clip with the given fill rule. Each geometry method
    /// pushes a verb and its coordinates together, so the pair can never fall
    /// out of sync.
    pub fn builder(fill_rule: FillRule) -> ClipPathBuilder {
        ClipPathBuilder::new(fill_rule)
    }

    /// Raw wire verbs, kept in lock-step with [`Self::coords`]. Crate-internal
    /// — only the wire codec touches the raw streams; read geometry through
    /// [`Self::segments`].
    pub(crate) fn verbs(&self) -> &[SegmentKind] {
        &self.verbs
    }

    /// Raw coordinate stream. Crate-internal; read [`Self::segments`] for a
    /// typed walk.
    pub(crate) fn coords(&self) -> &[f32] {
        &self.coords
    }

    /// Typed segment walk over the verb/coord streams.
    pub fn segments(&self) -> Segments<'_> {
        Segments::new(&self.verbs, &self.coords)
    }
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

/// Text node fields. Glyphs are drawn in "natural" text space (origin at
/// the baseline-left, units in `size`-pixel font units) and then mapped to
/// canvas pixels by [`Self::transform`]. The transform follows the PDF
/// `cm` / SVG `matrix(...)` convention:
///
/// ```text
/// x' = transform[0] * x + transform[2] * y + transform[4]
/// y' = transform[1] * x + transform[3] * y + transform[5]
/// ```
///
/// The producer is expected to bake "fit to bounding box", rotation, and
/// mirroring into this matrix — see [`text_box_affine`] for the canonical
/// helper that mirrors the legacy `(cx, cy, bw, bh, angle)` API.
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
    pub transform: [f32; 6],
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
            // Identity affine — renders glyphs in their natural orientation
            // at the origin. Callers that want a translated/rotated/fitted
            // text run go through [`text_box_affine`].
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            size: 0.0,
            family: String::new(),
            weight: 400,
            style: FontStyle::Normal,
            underline: false,
            text: String::new(),
        }
    }
}

/// Compose the affine for "fit the rendered text into a rotated bounding
/// box of size `bw × bh` centred on `(cx, cy)`". Measures the text once at
/// `size` via [`crate::text`] (same code the renderers use, so producer
/// and backend agree) and returns `[a, b, c, d, e, f]` in the PDF `cm` /
/// SVG `matrix(...)` convention. Negative `bw` mirrors horizontally;
/// negative `bh` mirrors vertically. Empty or zero-sized text returns the
/// identity-translated-to-`(cx, cy)` matrix — the renderer short-circuits
/// at the same gate, so the choice is cosmetic.
#[allow(clippy::too_many_arguments)]
pub fn text_box_affine(
    family: &str,
    weight: u16,
    style: FontStyle,
    size: f32,
    text: &str,
    cx: f32,
    cy: f32,
    bw: f32,
    bh: f32,
    angle_deg: f32,
) -> [f32; 6] {
    let size_i = size as i32;
    if size_i <= 0 || text.is_empty() {
        return [1.0, 0.0, 0.0, 1.0, cx, cy];
    }
    let font = crate::text::resolve(family, weight, style);
    let face = font.face();
    let orig_w = crate::text::measure_width_with(face, text, size_i) as f32;
    let orig_h = crate::text::measure_height_with(face, text, size_i) as f32;
    if orig_w <= 0.0 || orig_h <= 0.0 {
        return [1.0, 0.0, 0.0, 1.0, cx, cy];
    }
    let sx = bw / orig_w;
    let sy = bh / orig_h;
    let theta = angle_deg * std::f32::consts::PI / 180.0;
    let ct = theta.cos();
    let st = theta.sin();
    [sx * ct, sx * st, -sy * st, sy * ct, cx, cy]
}

/// A bitmap blit. The `id` references a previously-uploaded asset
/// (`Message::Asset` on the wire); the renderer is responsible for
/// resolving it to actual pixels. The 6-float affine maps the bitmap's
/// natural image-pixel coordinates `(0..img_w, 0..img_h)` onto the canvas,
/// same PDF `cm` / SVG `matrix(...)` convention as [`TextNode::transform`].
/// See [`bitmap_box_affine`] for the canonical "fit in a rotated box"
/// helper.
#[derive(Clone, Copy, Debug)]
pub struct Bitmap {
    pub id: u32,
    pub transform: [f32; 6],
}

impl Default for Bitmap {
    fn default() -> Self {
        Self {
            id: 0,
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        }
    }
}

/// Compose the affine for "fit the bitmap into a rotated bounding box of
/// size `w × h` centred on `(cx, cy)`". `img_w`/`img_h` are the asset's
/// natural pixel dimensions. Negative `w` mirrors horizontally; negative
/// `h` mirrors vertically. Zero-sized inputs return the identity-
/// translated-to-`(cx, cy)` matrix.
#[allow(clippy::too_many_arguments)]
pub fn bitmap_box_affine(
    img_w: u32,
    img_h: u32,
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
    angle_deg: f32,
) -> [f32; 6] {
    if img_w == 0 || img_h == 0 {
        return [1.0, 0.0, 0.0, 1.0, cx, cy];
    }
    let sx = w / img_w as f32;
    let sy = h / img_h as f32;
    let theta = angle_deg * std::f32::consts::PI / 180.0;
    let ct = theta.cos();
    let st = theta.sin();
    // Centre on (cx,cy): M = T(cx,cy) · R(theta) · S(sx,sy) · T(-iw/2, -ih/2)
    let a = sx * ct;
    let b = sx * st;
    let c = -sy * st;
    let d = sy * ct;
    let half_w = img_w as f32 * 0.5;
    let half_h = img_h as f32 * 0.5;
    let e = cx - (a * half_w + c * half_h);
    let f = cy - (b * half_w + d * half_h);
    [a, b, c, d, e, f]
}

/// The kind of a path segment — its verb byte on the wire. Each kind consumes
/// a fixed number of floats from the coordinate stream (see [`Self::coords`]);
/// pair one with its coords to get a [`Segment`]. The discriminants are stable: they
/// match the byte values used in the wire format
/// (`verbMove`/`verbLine`/`verbQuad`/`verbCubic` in `schema/frame.capnp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SegmentKind {
    Move = 0,
    Line = 1,
    Quad = 2,
    Cubic = 3,
}

impl SegmentKind {
    /// Decode a wire byte. Returns `None` on an unknown verb — callers at
    /// the wire boundary surface this as `wire::Error::UnknownVerb`.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Move),
            1 => Some(Self::Line),
            2 => Some(Self::Quad),
            3 => Some(Self::Cubic),
            _ => None,
        }
    }

    /// Number of floats this verb pulls from the coord stream.
    pub fn coords(self) -> usize {
        match self {
            Self::Move | Self::Line => 2,
            Self::Quad => 4,
            Self::Cubic => 6,
        }
    }
}

/// One decoded path segment — a verb paired with its coordinates. Yielded by
/// [`Path::segments`] / [`ClipPath::segments`] so backends walk typed geometry
/// instead of indexing parallel `verbs`/`coords` arrays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Segment {
    Move {
        x: f32,
        y: f32,
    },
    Line {
        x: f32,
        y: f32,
    },
    Quad {
        cx: f32,
        cy: f32,
        x: f32,
        y: f32,
    },
    Cubic {
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    },
}

/// Iterator over a verb/coord pair, yielding one [`Segment`] per verb.
/// Constructed from a [`Path`]/[`ClipPath`] whose streams agree in length by
/// construction; even so, [`Self::next`] slices defensively — if the coord
/// stream runs short it stops (`None`) instead of panicking.
#[must_use = "Segments yields nothing unless iterated"]
pub struct Segments<'a> {
    verbs: std::slice::Iter<'a, SegmentKind>,
    coords: &'a [f32],
    i: usize,
}

impl<'a> Segments<'a> {
    fn new(verbs: &'a [SegmentKind], coords: &'a [f32]) -> Self {
        Self {
            verbs: verbs.iter(),
            coords,
            i: 0,
        }
    }
}

impl Iterator for Segments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        let &v = self.verbs.next()?;
        // One bounds check for the whole verb; `c` then has exactly `v.coords()`
        // elements, so the fixed indices below cannot go out of range.
        let c = self.coords.get(self.i..self.i + v.coords())?;
        let seg = match v {
            SegmentKind::Move => Segment::Move { x: c[0], y: c[1] },
            SegmentKind::Line => Segment::Line { x: c[0], y: c[1] },
            SegmentKind::Quad => Segment::Quad {
                cx: c[0],
                cy: c[1],
                x: c[2],
                y: c[3],
            },
            SegmentKind::Cubic => Segment::Cubic {
                c1x: c[0],
                c1y: c[1],
                c2x: c[2],
                c2y: c[3],
                x: c[4],
                y: c[5],
            },
        };
        self.i += v.coords();
        Some(seg)
    }
}

/// Expand an SVG endpoint arc from `(x1, y1)` to `(x, y)` into cubic segments,
/// appending `Cubic` verbs and their coords in lock-step. Returns `false` for
/// a degenerate arc — the caller emits a line instead. Shared by the path and
/// clip builders so the kurbo expansion lives in one place.
#[allow(clippy::too_many_arguments)]
fn push_arc_cubics(
    verbs: &mut Vec<SegmentKind>,
    coords: &mut Vec<f32>,
    x1: f32,
    y1: f32,
    rx: f32,
    ry: f32,
    rotation_deg: f32,
    large_arc: bool,
    sweep: bool,
    x: f32,
    y: f32,
) -> bool {
    let svg_arc = kurbo::SvgArc {
        from: kurbo::Point::new(x1 as f64, y1 as f64),
        to: kurbo::Point::new(x as f64, y as f64),
        radii: kurbo::Vec2::new(rx as f64, ry as f64),
        x_rotation: (rotation_deg as f64).to_radians(),
        large_arc,
        sweep,
    };
    let Some(arc) = kurbo::Arc::from_svg_arc(&svg_arc) else {
        return false;
    };
    for el in arc.append_iter(ARC_TOLERANCE) {
        if let kurbo::PathEl::CurveTo(p1, p2, p3) = el {
            verbs.push(SegmentKind::Cubic);
            coords.extend([
                p1.x as f32,
                p1.y as f32,
                p2.x as f32,
                p2.y as f32,
                p3.x as f32,
                p3.y as f32,
            ]);
        }
    }
    true
}

/// A materialized 2D path: a style plus a flat verb stream and its
/// floating-point arguments.
///
/// `verbs[i]` pulls 2 (move/line), 4 (quad), or 6 (cubic) floats from
/// `coords` in order. `verbs`/`coords` are private and always agree in length
/// — the only way to build one is a [`PathBuilder`] (via [`Path::builder`] or
/// the [`Scene::path`] scope), which pushes verbs and coords together so a
/// mismatch is structurally impossible. Read the geometry back via
/// [`Path::segments`].
#[derive(Clone, Debug, Default)]
pub struct Path {
    pub style: PathStyle,
    verbs: Vec<SegmentKind>,
    coords: Vec<f32>,
}

impl Path {
    /// Start building a path with the given style. Each geometry method pushes
    /// a verb and its coordinates together, so the pair can never fall out of
    /// sync.
    pub fn builder(style: PathStyle) -> PathBuilder {
        PathBuilder::new(style)
    }

    /// Raw wire verbs, kept in lock-step with [`Self::coords`]. Crate-internal
    /// — only the wire codec touches the raw streams; read geometry through
    /// [`Self::segments`].
    pub(crate) fn verbs(&self) -> &[SegmentKind] {
        &self.verbs
    }

    /// Raw coordinate stream. Crate-internal; read [`Self::segments`] for a
    /// typed walk.
    pub(crate) fn coords(&self) -> &[f32] {
        &self.coords
    }

    /// Typed segment walk over the verb/coord streams.
    pub fn segments(&self) -> Segments<'_> {
        Segments::new(&self.verbs, &self.coords)
    }
}

/// One node of a [`Scene`]. A path bundles all its segments; clips wrap the
/// nested elements they apply to; text and bitmap are leaves.
#[derive(Clone, Debug)]
pub enum Element {
    Path(Path),
    Clipped {
        clip: ClipPath,
        elements: Vec<Element>,
    },
    Text(Box<TextNode>),
    Bitmap(Bitmap),
}

/// Materialized event log produced by Python (or any other front end) and
/// consumed by every renderer. Built via [`Self::add_path`] for a ready
/// [`Path`] plus RAII guards — [`Self::path`] returns a [`PathScope`] that
/// commits the path on drop, and [`Self::clip`] / [`Self::clip_rect`] return
/// a [`ClipScope`] that wraps every element drawn during its lifetime into an
/// [`Element::Clipped`] on drop. The list is replayed by a
/// [`Renderer`](crate::renderer::Renderer).
///
/// Because path geometry only flows through a builder and a clip's subtree is
/// exactly the run of elements drawn while its `ClipScope` is alive, several
/// footguns of the older flat API are statically impossible: you cannot append
/// path verbs without an open path, unbalance the clip stack (there is no
/// separate pop), or interleave a clip with a half-built path. A `PathScope`
/// that recorded no geometry also discards itself; only [`Self::add_path`]
/// (the explicit escape hatch for a ready [`Path`]) can enter an empty one.
///
/// Arcs entered via [`PathScope::arc_to`] are pre-expanded to cubics so
/// renderers only see line / quad / cubic primitives.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub width: f32,
    pub height: f32,
    pub elements: Vec<Element>,
}

/// Tolerance for SVG arc → cubic conversion.
const ARC_TOLERANCE: f64 = 0.1;

impl Scene {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            elements: Vec::new(),
        }
    }

    /// Append an already-built [`Path`] as a leaf element. Use this when a
    /// path is assembled away from the scene — decoded off the wire, shared,
    /// or produced by [`Path::builder`]; use [`Self::path`] to build one in
    /// place instead.
    pub fn add_path(&mut self, path: Path) {
        self.elements.push(Element::Path(path));
    }

    /// Begin a new path. Returns a [`PathScope`] whose `move_to` / `line_to`
    /// / `quad_to` / `cubic_to` / `arc_to` methods append verbs; the path is
    /// committed to [`Self::elements`] on drop, or discarded if no geometry was
    /// recorded.
    pub fn path(&mut self, style: PathStyle) -> PathScope<'_> {
        PathScope {
            scene: self,
            builder: PathBuilder::new(style),
        }
    }

    /// Push an arbitrary clip path. Returns a [`ClipScope`] that marks the
    /// current end of [`Self::elements`]; every element drawn through the
    /// scope lands in the scene as usual, and on drop the scope wraps exactly
    /// that trailing run into an [`Element::Clipped`]. The scope `Deref`s to
    /// this same scene, so nested clips just call [`Self::clip`] through the
    /// `Deref` and commit inside-out.
    pub fn clip(&mut self, clip: ClipPath) -> ClipScope<'_> {
        let mark = self.elements.len();
        ClipScope {
            scene: self,
            clip,
            mark,
        }
    }

    /// Push an axis-aligned-or-rotated rectangular clip — the common case.
    /// Builds the 4-corner `ClipPath` (rotated by `angle_deg` around the
    /// centre) and returns the builder.
    pub fn clip_rect(
        &mut self,
        cx: f32,
        cy: f32,
        w: f32,
        h: f32,
        angle_deg: f32,
        fill_rule: FillRule,
    ) -> ClipScope<'_> {
        let hw = w / 2.0;
        let hh = h / 2.0;
        let theta = angle_deg * std::f32::consts::PI / 180.0;
        let (cos, sin) = (theta.cos(), theta.sin());
        let corner =
            |x: f32, y: f32| -> (f32, f32) { (cx + x * cos - y * sin, cy + x * sin + y * cos) };
        let p0 = corner(-hw, -hh);
        let p1 = corner(hw, -hh);
        let p2 = corner(hw, hh);
        let p3 = corner(-hw, hh);
        self.clip(
            ClipPath::builder(fill_rule)
                .move_to(p0.0, p0.1)
                .line_to(p1.0, p1.1)
                .line_to(p2.0, p2.1)
                .line_to(p3.0, p3.1)
                .build(),
        )
    }

    pub fn text(&mut self, node: TextNode) {
        self.elements.push(Element::Text(Box::new(node)));
    }

    pub fn bitmap(&mut self, node: Bitmap) {
        self.elements.push(Element::Bitmap(node));
    }
}

/// Active path scope returned by [`Scene::path`]. Wraps a [`PathBuilder`] and
/// a borrow of the parent [`Scene`]; its `&mut self` geometry methods let a
/// path be built imperatively (across statements and loops), and on drop it
/// commits the finished [`Element::Path`] to the scene (or discards it if no
/// geometry was recorded). Each method just forwards to the wrapped by-value
/// [`PathBuilder`], so the atomic verb/coord push lives only there.
#[must_use = "PathScope commits the path on drop; bind it so geometry methods can run"]
pub struct PathScope<'a> {
    scene: &'a mut Scene,
    builder: PathBuilder,
}

impl PathScope<'_> {
    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.builder = std::mem::take(&mut self.builder).move_to(x, y);
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.builder = std::mem::take(&mut self.builder).line_to(x, y);
        self
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        self.builder = std::mem::take(&mut self.builder).quad_to(cx, cy, x, y);
        self
    }

    pub fn cubic_to(
        &mut self,
        c1x: f32,
        c1y: f32,
        c2x: f32,
        c2y: f32,
        x: f32,
        y: f32,
    ) -> &mut Self {
        self.builder = std::mem::take(&mut self.builder).cubic_to(c1x, c1y, c2x, c2y, x, y);
        self
    }

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
    ) -> &mut Self {
        self.builder =
            std::mem::take(&mut self.builder).arc_to(rx, ry, rotation_deg, large_arc, sweep, x, y);
        self
    }
}

impl Drop for PathScope<'_> {
    fn drop(&mut self) {
        let builder = std::mem::take(&mut self.builder);
        if !builder.verbs.is_empty() {
            self.scene.elements.push(Element::Path(builder.build()));
        }
    }
}

/// Active clip scope returned by [`Scene::clip`] / [`Scene::clip_rect`].
/// Borrows the scene and remembers where its own elements begin (`mark`);
/// `Deref`s to that same scene so draw methods append there directly. On
/// drop, the trailing run `elements[mark..]` is lifted into an
/// [`Element::Clipped`] pushed back in its place — the structure itself
/// guarantees balanced clip nesting (no separate pop).
///
/// Nested clips work the usual way: calling [`Scene::clip`] through the
/// `Deref` returns a child `ClipScope` with a later mark; dropping
/// inside-out leaves a well-formed tree.
#[must_use = "ClipScope commits the clip on drop; bind it where the clip should end"]
pub struct ClipScope<'a> {
    scene: &'a mut Scene,
    clip: ClipPath,
    mark: usize,
}

impl<'a> std::ops::Deref for ClipScope<'a> {
    type Target = Scene;
    fn deref(&self) -> &Scene {
        self.scene
    }
}

impl<'a> std::ops::DerefMut for ClipScope<'a> {
    fn deref_mut(&mut self) -> &mut Scene {
        self.scene
    }
}

impl<'a> Drop for ClipScope<'a> {
    fn drop(&mut self) {
        let clip = std::mem::take(&mut self.clip);
        let elements = self.scene.elements.split_off(self.mark);
        self.scene
            .elements
            .push(Element::Clipped { clip, elements });
    }
}

/// Owned builder for a [`Path`], returned by [`Path::builder`]. Each geometry
/// method consumes and returns `self`, appending a verb and its coordinates
/// together so the verb/coord streams cannot fall out of sync; [`Self::build`]
/// moves the parts into the finished path. Arcs entered via [`Self::arc_to`]
/// are pre-expanded to cubics. [`Scene::path`] drives one to commit straight
/// into a scene.
#[derive(Default)]
#[must_use = "PathBuilder yields a Path only when build() is called"]
pub struct PathBuilder {
    style: PathStyle,
    verbs: Vec<SegmentKind>,
    coords: Vec<f32>,
    last_point: Option<(f32, f32)>,
}

impl PathBuilder {
    fn new(style: PathStyle) -> Self {
        Self {
            style,
            verbs: Vec::new(),
            coords: Vec::new(),
            last_point: None,
        }
    }

    pub fn move_to(mut self, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Move);
        self.coords.extend([x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn line_to(mut self, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Line);
        self.coords.extend([x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn quad_to(mut self, cx: f32, cy: f32, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Quad);
        self.coords.extend([cx, cy, x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn cubic_to(mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Cubic);
        self.coords.extend([c1x, c1y, c2x, c2y, x, y]);
        self.last_point = Some((x, y));
        self
    }

    /// Append an SVG endpoint arc, pre-expanding to cubic segments. Mirrors
    /// the text parser's `A` handling: with no current point, falls back to
    /// `move_to(x, y)`; degenerate arcs collapse to a line.
    #[allow(clippy::too_many_arguments)]
    pub fn arc_to(
        mut self,
        rx: f32,
        ry: f32,
        rotation_deg: f32,
        large_arc: bool,
        sweep: bool,
        x: f32,
        y: f32,
    ) -> Self {
        let Some((x1, y1)) = self.last_point else {
            return self.move_to(x, y);
        };
        if push_arc_cubics(
            &mut self.verbs,
            &mut self.coords,
            x1,
            y1,
            rx,
            ry,
            rotation_deg,
            large_arc,
            sweep,
            x,
            y,
        ) {
            self.last_point = Some((x, y));
            self
        } else {
            self.line_to(x, y)
        }
    }

    pub fn build(self) -> Path {
        Path {
            style: self.style,
            verbs: self.verbs,
            coords: self.coords,
        }
    }
}

/// Owned builder for a [`ClipPath`], returned by [`ClipPath::builder`]. Each
/// geometry method consumes and returns `self`, appending a verb and its
/// coordinates together so the verb/coord streams cannot fall out of sync;
/// [`Self::build`] moves the parts into the finished clip. Sub-paths are
/// implicitly closed by the renderers, so no closing line is required.
#[must_use = "ClipPathBuilder yields a ClipPath only when build() is called"]
pub struct ClipPathBuilder {
    verbs: Vec<SegmentKind>,
    coords: Vec<f32>,
    fill_rule: FillRule,
    last_point: Option<(f32, f32)>,
}

impl ClipPathBuilder {
    fn new(fill_rule: FillRule) -> Self {
        Self {
            verbs: Vec::new(),
            coords: Vec::new(),
            fill_rule,
            last_point: None,
        }
    }

    pub fn move_to(mut self, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Move);
        self.coords.extend([x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn line_to(mut self, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Line);
        self.coords.extend([x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn quad_to(mut self, cx: f32, cy: f32, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Quad);
        self.coords.extend([cx, cy, x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn cubic_to(mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> Self {
        self.verbs.push(SegmentKind::Cubic);
        self.coords.extend([c1x, c1y, c2x, c2y, x, y]);
        self.last_point = Some((x, y));
        self
    }

    /// Append an SVG endpoint arc, pre-expanding to cubics — mirrors
    /// [`PathBuilder::arc_to`]. With no current point, falls back to
    /// `move_to(x, y)`; degenerate arcs collapse to a line.
    #[allow(clippy::too_many_arguments)]
    pub fn arc_to(
        mut self,
        rx: f32,
        ry: f32,
        rotation_deg: f32,
        large_arc: bool,
        sweep: bool,
        x: f32,
        y: f32,
    ) -> Self {
        let Some((x1, y1)) = self.last_point else {
            return self.move_to(x, y);
        };
        if push_arc_cubics(
            &mut self.verbs,
            &mut self.coords,
            x1,
            y1,
            rx,
            ry,
            rotation_deg,
            large_arc,
            sweep,
            x,
            y,
        ) {
            self.last_point = Some((x, y));
            self
        } else {
            self.line_to(x, y)
        }
    }

    pub fn build(self) -> ClipPath {
        ClipPath {
            verbs: self.verbs,
            coords: self.coords,
            fill_rule: self.fill_rule,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Total floats a verb stream consumes — the invariant every builder must
    /// preserve.
    fn coords_arity(verbs: &[SegmentKind]) -> usize {
        verbs.iter().map(|v| v.coords()).sum()
    }

    #[test]
    fn builders_keep_streams_in_lockstep() {
        let clip = ClipPath::builder(FillRule::EvenOdd)
            .move_to(0.0, 0.0)
            .line_to(10.0, 0.0)
            .quad_to(15.0, 5.0, 10.0, 10.0)
            .cubic_to(8.0, 8.0, 4.0, 6.0, 0.0, 10.0)
            .build();
        assert_eq!(
            clip.verbs(),
            [
                SegmentKind::Move,
                SegmentKind::Line,
                SegmentKind::Quad,
                SegmentKind::Cubic
            ]
        );
        assert_eq!(coords_arity(clip.verbs()), clip.coords().len());
        assert_eq!(clip.fill_rule, FillRule::EvenOdd);
    }

    #[test]
    fn segments_decodes_each_verb() {
        let clip = ClipPath::builder(FillRule::NonZero)
            .move_to(1.0, 2.0)
            .line_to(3.0, 4.0)
            .quad_to(5.0, 6.0, 7.0, 8.0)
            .cubic_to(9.0, 10.0, 11.0, 12.0, 13.0, 14.0)
            .build();
        let segs: Vec<_> = clip.segments().collect();
        assert_eq!(
            segs,
            vec![
                Segment::Move { x: 1.0, y: 2.0 },
                Segment::Line { x: 3.0, y: 4.0 },
                Segment::Quad {
                    cx: 5.0,
                    cy: 6.0,
                    x: 7.0,
                    y: 8.0
                },
                Segment::Cubic {
                    c1x: 9.0,
                    c1y: 10.0,
                    c2x: 11.0,
                    c2y: 12.0,
                    x: 13.0,
                    y: 14.0
                },
            ]
        );
    }

    #[test]
    fn segments_stops_on_short_coords() {
        // Degenerate streams (unreachable through the builders, but the iterator
        // must not panic if handed one): the Move decodes, then the Cubic wants
        // six coords and only finds none, so next() ends the walk with None.
        let verbs = [SegmentKind::Move, SegmentKind::Cubic];
        let coords = [1.0, 2.0];
        let mut segs = Segments::new(&verbs, &coords);
        assert_eq!(segs.next(), Some(Segment::Move { x: 1.0, y: 2.0 }));
        assert_eq!(segs.next(), None);
    }

    #[test]
    fn clip_builder_arc_to_expands_to_cubics_in_lockstep() {
        let clip = ClipPath::builder(FillRule::NonZero)
            .move_to(0.0, 0.0)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 0.0)
            .build();
        // Arc after a current point expands to at least one cubic; the streams
        // stay balanced regardless of how many the tolerance produced.
        assert!(clip.verbs().contains(&SegmentKind::Cubic));
        assert_eq!(coords_arity(clip.verbs()), clip.coords().len());
    }

    #[test]
    fn clip_builder_arc_to_without_current_point_moves() {
        // No prior point: arc_to degrades to a bare move, no cubics.
        let clip = ClipPath::builder(FillRule::NonZero)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 10.0)
            .build();
        assert_eq!(clip.verbs(), [SegmentKind::Move]);
        assert_eq!(clip.coords(), [10.0, 10.0]);
    }

    #[test]
    fn standalone_path_builder_builds_agreeing_path() {
        let p = Path::builder(PathStyle::default())
            .move_to(0.0, 0.0)
            .line_to(10.0, 0.0)
            .quad_to(15.0, 5.0, 10.0, 10.0)
            .build();
        assert_eq!(
            p.verbs(),
            [SegmentKind::Move, SegmentKind::Line, SegmentKind::Quad]
        );
        assert_eq!(coords_arity(p.verbs()), p.coords().len());
    }

    #[test]
    fn scene_path_builder_commits_agreeing_geometry() {
        let mut scene = Scene::new(10.0, 10.0);
        scene
            .path(PathStyle::default())
            .move_to(0.0, 0.0)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 0.0);
        let Element::Path(p) = &scene.elements[0] else {
            panic!("expected a path");
        };
        // arc_to expands to cubics; whatever the count, the streams agree.
        assert_eq!(coords_arity(p.verbs()), p.coords().len());
    }

    #[test]
    fn add_path_appends_prebuilt_path() {
        let mut scene = Scene::new(10.0, 10.0);
        let p = Path::builder(PathStyle::default())
            .move_to(0.0, 0.0)
            .line_to(5.0, 5.0)
            .build();
        scene.add_path(p);
        let Element::Path(p) = &scene.elements[0] else {
            panic!("expected a path");
        };
        assert_eq!(p.verbs(), [SegmentKind::Move, SegmentKind::Line]);
    }

    #[test]
    fn clip_wraps_only_elements_drawn_inside() {
        // Draw before, inside, and after the clip: only the middle path is
        // wrapped; the outer two stay as bare siblings.
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_path(
            Path::builder(PathStyle::default())
                .move_to(0.0, 0.0)
                .build(),
        );
        {
            let mut c = scene.clip(
                ClipPath::builder(FillRule::NonZero)
                    .move_to(0.0, 0.0)
                    .build(),
            );
            c.add_path(
                Path::builder(PathStyle::default())
                    .move_to(1.0, 1.0)
                    .build(),
            );
        }
        scene.add_path(
            Path::builder(PathStyle::default())
                .move_to(2.0, 2.0)
                .build(),
        );

        assert!(matches!(scene.elements[0], Element::Path(_)));
        assert!(matches!(scene.elements[2], Element::Path(_)));
        let Element::Clipped { elements, .. } = &scene.elements[1] else {
            panic!("expected the middle element to be clipped");
        };
        assert_eq!(elements.len(), 1);
        let Element::Path(p) = &elements[0] else {
            panic!("expected a path inside the clip");
        };
        assert_eq!(p.coords(), [1.0, 1.0]);
    }

    #[test]
    fn nested_clips_wrap_inside_out() {
        let mut scene = Scene::new(20.0, 20.0);
        {
            let mut outer = scene.clip(
                ClipPath::builder(FillRule::NonZero)
                    .move_to(0.0, 0.0)
                    .build(),
            );
            outer.add_path(
                Path::builder(PathStyle::default())
                    .move_to(1.0, 1.0)
                    .build(),
            );
            {
                let mut inner = outer.clip(
                    ClipPath::builder(FillRule::NonZero)
                        .move_to(0.0, 0.0)
                        .build(),
                );
                inner.add_path(
                    Path::builder(PathStyle::default())
                        .move_to(2.0, 2.0)
                        .build(),
                );
            }
        }
        // scene = [ Clipped{ outer, [ Path(1,1), Clipped{ inner, [ Path(2,2) ] } ] } ]
        assert_eq!(scene.elements.len(), 1);
        let Element::Clipped { elements, .. } = &scene.elements[0] else {
            panic!("expected outer clip");
        };
        assert_eq!(elements.len(), 2);
        assert!(matches!(elements[0], Element::Path(_)));
        assert!(matches!(elements[1], Element::Clipped { .. }));
    }
}
