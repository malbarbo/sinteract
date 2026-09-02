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

/// Where a gradient's color ramp is swept, in path-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientGeom {
    /// Along the line from (x0, y0) to (x1, y1).
    Linear { x0: f32, y0: f32, x1: f32, y1: f32 },
    /// Outward from (cx, cy) to `radius`.
    Radial { cx: f32, cy: f32, radius: f32 },
}

/// A color ramp and the geometry it is swept along. `stops` are pre-sorted by
/// offset.
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    pub geom: GradientGeom,
    pub stops: Vec<Stop>,
    pub spread: SpreadMode,
}

impl Gradient {
    pub fn linear(x0: f32, y0: f32, x1: f32, y1: f32, stops: Vec<Stop>) -> Self {
        Self {
            geom: GradientGeom::Linear { x0, y0, x1, y1 },
            stops,
            spread: SpreadMode::Pad,
        }
    }

    pub fn radial(cx: f32, cy: f32, radius: f32, stops: Vec<Stop>) -> Self {
        Self {
            geom: GradientGeom::Radial { cx, cy, radius },
            stops,
            spread: SpreadMode::Pad,
        }
    }

    /// Override the [`SpreadMode::Pad`] the constructors default to.
    pub fn with_spread(mut self, spread: SpreadMode) -> Self {
        self.spread = spread;
        self
    }
}

/// Fill or stroke paint — solid color or gradient. The gradient is boxed:
/// solid is the overwhelmingly common case, and inlining a `Gradient` here
/// would widen every `PathStyle`, and so every `Element`, by 40 bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Solid(Rgba),
    Gradient(Box<Gradient>),
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

    /// Gradient paint, boxing for the caller.
    pub fn gradient(g: Gradient) -> Self {
        Self::Gradient(Box::new(g))
    }

    /// Whether this paint would draw at least one visible pixel. Used by
    /// renderers as a fast cull (skip both the fill and stroke when neither
    /// would mark the canvas).
    pub fn is_visible(&self) -> bool {
        match self {
            Self::Solid(c) => c.a > 0.0,
            Self::Gradient(g) => !g.stops.is_empty(),
        }
    }

    /// First stop's color for gradients, or the solid color. Used as the
    /// fallback when a renderer cannot honor gradients (or as a tint for
    /// effects keyed on a single color).
    pub fn primary_color(&self) -> Rgba {
        match self {
            Self::Solid(c) => *c,
            Self::Gradient(g) => g.stops.first().map(|s| s.color).unwrap_or_default(),
        }
    }
}

/// A stroke dash pattern: on/off lengths in path units, repeating once
/// consumed, started `offset` units in. Constructed only through
/// [`Dash::new`], which rejects an empty pattern — a dash with no lengths is
/// a solid stroke, and that case belongs in the `Option`, not in this type.
#[derive(Clone, Debug, PartialEq)]
pub struct Dash {
    array: Box<[f32]>,
    offset: f32,
}

impl Dash {
    pub fn new(array: impl Into<Box<[f32]>>, offset: f32) -> Option<Self> {
        let array = array.into();
        (!array.is_empty()).then_some(Self { array, offset })
    }

    pub fn array(&self) -> &[f32] {
        &self.array
    }

    pub fn offset(&self) -> f32 {
        self.offset
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
    /// `None` = solid stroke. Boxed: dashing is rare and a `Dash` inline
    /// would cost every style 16 bytes it almost never uses.
    pub dash: Option<Box<Dash>>,
}

impl PathStyle {
    /// Whether the fill would mark the canvas.
    pub fn draws_fill(&self) -> bool {
        self.fill.is_visible()
    }

    /// Whether the stroke would mark the canvas — a zero width draws nothing
    /// however visible the paint is.
    pub fn draws_stroke(&self) -> bool {
        self.stroke.is_visible() && self.stroke_width > 0.0
    }
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
            dash: None,
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
/// The verb and coord streams are private and always agree in length:
/// [`ClipPath::builder`] pushes verbs and coords together, and the wire
/// decoder's only entry point validates the arity, so a mismatch cannot be
/// built either way. Read the geometry back via [`ClipPath::segments`].
#[derive(Clone, Debug, Default)]
pub struct ClipPath {
    geom: Geometry,
    pub fill_rule: FillRule,
}

impl ClipPath {
    /// Start building a clip with the given fill rule. Each geometry method
    /// pushes a verb and its coordinates together, so the pair can never fall
    /// out of sync.
    pub fn builder(fill_rule: FillRule) -> ClipPathBuilder {
        ClipPathBuilder::new(fill_rule)
    }

    /// Wire decode into an existing clip, reusing its allocations. See
    /// [`Geometry::refill_from_wire`] for the arity contract.
    pub(crate) fn refill_from_wire<E>(
        &mut self,
        fill_rule: FillRule,
        verbs: impl Iterator<Item = Result<SegmentKind, E>>,
        coords: impl Iterator<Item = f32>,
    ) -> Result<bool, E> {
        self.fill_rule = fill_rule;
        self.geom.refill_from_wire(verbs, coords)
    }

    /// Raw wire verbs, kept in lock-step with [`Self::coords`]. Crate-internal
    /// — only the wire codec touches the raw streams; read geometry through
    /// [`Self::segments`].
    pub(crate) fn verbs(&self) -> &[SegmentKind] {
        &self.geom.verbs
    }

    /// Raw coordinate stream. Crate-internal; read [`Self::segments`] for a
    /// typed walk.
    pub(crate) fn coords(&self) -> &[f32] {
        &self.geom.coords
    }

    /// Typed segment walk over the verb/coord streams.
    pub fn segments(&self) -> Segments<'_> {
        self.geom.segments()
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
    /// Boxed rather than a `String`: it is only ever read as `&str`, and the
    /// 8 bytes saved keep [`Element`] the size of its `Path` variant.
    pub family: Box<str>,
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
            transform: translate(0.0, 0.0),
            size: 0.0,
            family: Box::default(),
            weight: 400,
            style: FontStyle::Normal,
            underline: false,
            text: String::new(),
        }
    }
}

/// The `cm` / `matrix(...)` affine that scales by `(sx, sy)`, rotates by
/// `angle_deg`, then translates to `(e, f)`.
fn rotate_scale_at(sx: f32, sy: f32, angle_deg: f32, e: f32, f: f32) -> [f32; 6] {
    let (st, ct) = angle_deg.to_radians().sin_cos();
    [sx * ct, sx * st, -sy * st, sy * ct, e, f]
}

/// The identity affine translated to `(e, f)` — the degenerate-input fallback
/// shared by the `*_box_affine` helpers.
fn translate(e: f32, f: f32) -> [f32; 6] {
    [1.0, 0.0, 0.0, 1.0, e, f]
}

/// Map `(x, y)` through a `cm` / `matrix(...)` affine.
fn apply_affine(m: [f32; 6], x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
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
        return translate(cx, cy);
    }
    let font = crate::text::resolve(family, weight, style);
    let face = font.face();
    let orig_w = crate::text::measure_width_with(face, text, size_i) as f32;
    let orig_h = crate::text::measure_height_with(face, text, size_i) as f32;
    if orig_w <= 0.0 || orig_h <= 0.0 {
        return translate(cx, cy);
    }
    rotate_scale_at(bw / orig_w, bh / orig_h, angle_deg, cx, cy)
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
            transform: translate(0.0, 0.0),
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
        return translate(cx, cy);
    }
    // Centre on (cx,cy): M = T(cx,cy) · R(theta) · S(sx,sy) · T(-iw/2, -ih/2),
    // i.e. offset the rotated-scaled image centre back onto (cx, cy).
    let m = rotate_scale_at(w / img_w as f32, h / img_h as f32, angle_deg, 0.0, 0.0);
    let (ox, oy) = apply_affine(m, img_w as f32 * 0.5, img_h as f32 * 0.5);
    [m[0], m[1], m[2], m[3], cx - ox, cy - oy]
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

    /// This walk with quadratics elevated to cubics. See [`Cubics`].
    pub fn cubics(self) -> Cubics<'a> {
        Cubics {
            inner: self,
            last: None,
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

/// Promote a quadratic Bézier — current point `p0`, control `(cx, cy)`,
/// endpoint `(x, y)` — to a cubic's two control points.
pub(crate) fn quad_to_cubic(
    p0: (f32, f32),
    cx: f32,
    cy: f32,
    x: f32,
    y: f32,
) -> (f32, f32, f32, f32) {
    let (p0x, p0y) = p0;
    (
        p0x + 2.0 / 3.0 * (cx - p0x),
        p0y + 2.0 / 3.0 * (cy - p0y),
        x + 2.0 / 3.0 * (cx - x),
        y + 2.0 / 3.0 * (cy - y),
    )
}

/// [`Segments`] with every quadratic elevated to a cubic, for backends that
/// have no quadratic operator. Tracking the current point is the whole job,
/// and doing it here means doing it once — the scene owns the format, so it
/// owns this expansion the way it already owns arc→cubic.
#[must_use = "Cubics yields nothing unless iterated"]
pub struct Cubics<'a> {
    inner: Segments<'a>,
    last: Option<(f32, f32)>,
}

impl Iterator for Cubics<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        loop {
            let seg = self.inner.next()?;
            match seg {
                Segment::Move { x, y } | Segment::Line { x, y } => {
                    self.last = Some((x, y));
                    return Some(seg);
                }
                Segment::Cubic { x, y, .. } => {
                    self.last = Some((x, y));
                    return Some(seg);
                }
                Segment::Quad { cx, cy, x, y } => {
                    // Geometry that opens on a quad has no anchor to elevate
                    // from; skip it and leave the current point unset.
                    let Some(p0) = self.last else { continue };
                    let (c1x, c1y, c2x, c2y) = quad_to_cubic(p0, cx, cy, x, y);
                    self.last = Some((x, y));
                    return Some(Segment::Cubic {
                        c1x,
                        c1y,
                        c2x,
                        c2y,
                        x,
                        y,
                    });
                }
            }
        }
    }
}

/// A verb stream paired with its coordinates — the geometry half of both
/// [`Path`] and [`ClipPath`]. Fields are private and only [`GeometryBuilder`]
/// appends to them, pushing a verb and its coords together, so the two streams
/// cannot fall out of sync.
#[derive(Clone, Debug, Default)]
pub(crate) struct Geometry {
    verbs: Vec<SegmentKind>,
    coords: Vec<f32>,
}

impl Geometry {
    fn segments(&self) -> Segments<'_> {
        Segments::new(&self.verbs, &self.coords)
    }

    /// Refill from raw wire streams, reusing the allocations already here, and
    /// validate that `coords` holds exactly the verbs' total arity — the
    /// invariant the private fields exist to protect. `Ok(false)` reports a
    /// disagreement, which the codec maps to
    /// [`wire::Error::PathLengthMismatch`](crate::wire::Error). On either
    /// failure the geometry is emptied rather than left half-decoded, so it is
    /// consistent whatever the caller does with the error.
    pub(crate) fn refill_from_wire<E>(
        &mut self,
        verbs: impl Iterator<Item = Result<SegmentKind, E>>,
        coords: impl Iterator<Item = f32>,
    ) -> Result<bool, E> {
        self.verbs.clear();
        self.coords.clear();
        for v in verbs {
            match v {
                Ok(v) => self.verbs.push(v),
                Err(e) => {
                    self.verbs.clear();
                    return Err(e);
                }
            }
        }
        self.coords.extend(coords);
        let needed: usize = self.verbs.iter().map(|v| v.coords()).sum();
        if needed != self.coords.len() {
            self.verbs.clear();
            self.coords.clear();
            return Ok(false);
        }
        Ok(true)
    }
}

/// Accumulator behind [`PathBuilder`] and [`ClipPathBuilder`]. Owns the only
/// code that appends geometry, so a new verb is added here once instead of
/// once per builder, and the kurbo arc expansion lives in a single place.
#[derive(Default)]
struct GeometryBuilder {
    geom: Geometry,
    last_point: Option<(f32, f32)>,
}

impl GeometryBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.geom.verbs.push(SegmentKind::Move);
        self.geom.coords.extend([x, y]);
        self.last_point = Some((x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.geom.verbs.push(SegmentKind::Line);
        self.geom.coords.extend([x, y]);
        self.last_point = Some((x, y));
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.geom.verbs.push(SegmentKind::Quad);
        self.geom.coords.extend([cx, cy, x, y]);
        self.last_point = Some((x, y));
    }

    fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.geom.verbs.push(SegmentKind::Cubic);
        self.geom.coords.extend([c1x, c1y, c2x, c2y, x, y]);
        self.last_point = Some((x, y));
    }

    /// Append an SVG endpoint arc, pre-expanding to cubics. With no current
    /// point it degrades to `move_to(x, y)`; a degenerate arc collapses to a
    /// line.
    #[allow(clippy::too_many_arguments)]
    fn arc_to(
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
            return self.move_to(x, y);
        };
        let svg_arc = kurbo::SvgArc {
            from: kurbo::Point::new(x1 as f64, y1 as f64),
            to: kurbo::Point::new(x as f64, y as f64),
            radii: kurbo::Vec2::new(rx as f64, ry as f64),
            x_rotation: (rotation_deg as f64).to_radians(),
            large_arc,
            sweep,
        };
        let Some(arc) = kurbo::Arc::from_svg_arc(&svg_arc) else {
            return self.line_to(x, y);
        };
        for el in arc.append_iter(ARC_TOLERANCE) {
            if let kurbo::PathEl::CurveTo(p1, p2, p3) = el {
                self.geom.verbs.push(SegmentKind::Cubic);
                self.geom.coords.extend([
                    p1.x as f32,
                    p1.y as f32,
                    p2.x as f32,
                    p2.y as f32,
                    p3.x as f32,
                    p3.y as f32,
                ]);
            }
        }
        self.last_point = Some((x, y));
    }
}

/// A materialized 2D path: a style plus a flat verb stream and its
/// floating-point arguments.
///
/// `verbs[i]` pulls 2 (move/line), 4 (quad), or 6 (cubic) floats from
/// `coords` in order. Both streams are private and always agree in length: a
/// [`PathBuilder`] (via [`Path::builder`] or the [`Scene::path`] scope) pushes
/// verbs and coords together, and the wire decoder's only entry point
/// validates the arity, so a mismatch cannot be built either way. Read the
/// geometry back via [`Path::segments`].
#[derive(Clone, Debug, Default)]
pub struct Path {
    pub style: PathStyle,
    geom: Geometry,
}

impl Path {
    /// Start building a path with the given style. Each geometry method pushes
    /// a verb and its coordinates together, so the pair can never fall out of
    /// sync.
    pub fn builder(style: PathStyle) -> PathBuilder {
        PathBuilder::new(style)
    }

    /// Wire decode into an existing path, reusing its allocations. See
    /// [`Geometry::refill_from_wire`] for the arity contract.
    pub(crate) fn refill_from_wire<E>(
        &mut self,
        style: PathStyle,
        verbs: impl Iterator<Item = Result<SegmentKind, E>>,
        coords: impl Iterator<Item = f32>,
    ) -> Result<bool, E> {
        self.style = style;
        self.geom.refill_from_wire(verbs, coords)
    }

    /// Raw wire verbs, kept in lock-step with [`Self::coords`]. Crate-internal
    /// — only the wire codec touches the raw streams; read geometry through
    /// [`Self::segments`].
    pub(crate) fn verbs(&self) -> &[SegmentKind] {
        &self.geom.verbs
    }

    /// Raw coordinate stream. Crate-internal; read [`Self::segments`] for a
    /// typed walk.
    pub(crate) fn coords(&self) -> &[f32] {
        &self.geom.coords
    }

    /// Typed segment walk over the verb/coord streams.
    pub fn segments(&self) -> Segments<'_> {
        self.geom.segments()
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
    Text(TextNode),
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

    /// Whether any element in the tree is a bitmap, clip subtrees included.
    /// A host asks this to tell the user once that the backend it picked
    /// renders the frame without them — the backends themselves cannot say
    /// it, since a diagnostic is a property of the session, not of the draw.
    pub fn has_bitmaps(&self) -> bool {
        fn walk(elements: &[Element]) -> bool {
            elements.iter().any(|e| match e {
                Element::Bitmap(_) => true,
                Element::Clipped { elements, .. } => walk(elements),
                Element::Path(_) | Element::Text(_) => false,
            })
        }
        walk(&self.elements)
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
        let m = rotate_scale_at(1.0, 1.0, angle_deg, cx, cy);
        let corner = |x: f32, y: f32| apply_affine(m, x, y);
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
        self.elements.push(Element::Text(node));
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
        self.builder.geom.move_to(x, y);
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.builder.geom.line_to(x, y);
        self
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        self.builder.geom.quad_to(cx, cy, x, y);
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
        self.builder.geom.cubic_to(c1x, c1y, c2x, c2y, x, y);
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
        self.builder
            .geom
            .arc_to(rx, ry, rotation_deg, large_arc, sweep, x, y);
        self
    }
}

impl Drop for PathScope<'_> {
    fn drop(&mut self) {
        let builder = std::mem::take(&mut self.builder);
        if !builder.geom.geom.verbs.is_empty() {
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
/// `Default` is derived only so [`PathScope`]'s `Drop` can `mem::take` the
/// builder out of the scope; geometry always arrives through [`Path::builder`].
#[derive(Default)]
#[must_use = "PathBuilder yields a Path only when build() is called"]
pub struct PathBuilder {
    style: PathStyle,
    geom: GeometryBuilder,
}

impl PathBuilder {
    fn new(style: PathStyle) -> Self {
        Self {
            style,
            geom: GeometryBuilder::default(),
        }
    }

    pub fn move_to(mut self, x: f32, y: f32) -> Self {
        self.geom.move_to(x, y);
        self
    }

    pub fn line_to(mut self, x: f32, y: f32) -> Self {
        self.geom.line_to(x, y);
        self
    }

    pub fn quad_to(mut self, cx: f32, cy: f32, x: f32, y: f32) -> Self {
        self.geom.quad_to(cx, cy, x, y);
        self
    }

    pub fn cubic_to(mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> Self {
        self.geom.cubic_to(c1x, c1y, c2x, c2y, x, y);
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
        self.geom
            .arc_to(rx, ry, rotation_deg, large_arc, sweep, x, y);
        self
    }

    pub fn build(self) -> Path {
        Path {
            style: self.style,
            geom: self.geom.geom,
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
    geom: GeometryBuilder,
    fill_rule: FillRule,
}

impl ClipPathBuilder {
    fn new(fill_rule: FillRule) -> Self {
        Self {
            geom: GeometryBuilder::default(),
            fill_rule,
        }
    }

    pub fn move_to(mut self, x: f32, y: f32) -> Self {
        self.geom.move_to(x, y);
        self
    }

    pub fn line_to(mut self, x: f32, y: f32) -> Self {
        self.geom.line_to(x, y);
        self
    }

    pub fn quad_to(mut self, cx: f32, cy: f32, x: f32, y: f32) -> Self {
        self.geom.quad_to(cx, cy, x, y);
        self
    }

    pub fn cubic_to(mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) -> Self {
        self.geom.cubic_to(c1x, c1y, c2x, c2y, x, y);
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
        self.geom
            .arc_to(rx, ry, rotation_deg, large_arc, sweep, x, y);
        self
    }

    pub fn build(self) -> ClipPath {
        ClipPath {
            geom: self.geom.geom,
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
    fn cubics_elevates_quads_against_the_current_point() {
        let path = Path::builder(PathStyle::default())
            .move_to(0.0, 0.0)
            .quad_to(3.0, 3.0, 6.0, 0.0)
            .build();
        let segs: Vec<_> = path.segments().cubics().collect();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], Segment::Move { x: 0.0, y: 0.0 });
        // Controls sit 2/3 of the way from each endpoint toward the quad's.
        assert_eq!(
            segs[1],
            Segment::Cubic {
                c1x: 2.0,
                c1y: 2.0,
                c2x: 4.0,
                c2y: 2.0,
                x: 6.0,
                y: 0.0,
            }
        );
    }

    #[test]
    fn cubics_drops_a_quad_with_no_anchor() {
        // A stream opening on a quad has no current point to elevate from.
        let path = Path::builder(PathStyle::default())
            .quad_to(3.0, 3.0, 6.0, 0.0)
            .line_to(9.0, 0.0)
            .build();
        let segs: Vec<_> = path.segments().cubics().collect();
        assert_eq!(segs, vec![Segment::Line { x: 9.0, y: 0.0 }]);
    }

    #[test]
    fn has_bitmaps_sees_through_clip_subtrees() {
        let mut scene = Scene::new(10.0, 10.0);
        {
            let mut p = scene.path(PathStyle::default());
            p.move_to(0.0, 0.0);
            p.line_to(5.0, 5.0);
        }
        assert!(!scene.has_bitmaps());

        let clip = ClipPath::builder(FillRule::NonZero)
            .move_to(0.0, 0.0)
            .line_to(10.0, 10.0)
            .build();
        scene.clip(clip).bitmap(Bitmap::default());
        assert!(scene.has_bitmaps());
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
