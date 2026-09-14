//! The value types of a [`Scene`] and the builders that make them.

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

/// How a gradient continues past its axis, as the SVG `spreadMethod`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum SpreadMode {
    /// Repeat the color of the last stop.
    #[default]
    Pad = 0,
    /// Mirror the ramp at each end.
    Reflect = 1,
    /// Repeat the ramp.
    Repeat = 2,
}

/// The axis of a gradient, in path coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientGeom {
    /// Along the line from (x0, y0) to (x1, y1).
    Linear { x0: f32, y0: f32, x1: f32, y1: f32 },
    /// Outward from (cx, cy) to `radius`.
    Radial { cx: f32, cy: f32, radius: f32 },
}

/// A color ramp along an axis. `stops` are sorted by offset.
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

    pub fn with_spread(mut self, spread: SpreadMode) -> Self {
        self.spread = spread;
        self
    }
}

/// A fill or a stroke. The gradient is boxed because a solid color is the
/// common case, and a `Gradient` inline would add 40 bytes to every
/// `PathStyle` and so to every `Element`.
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
    pub fn rgba(r: u8, g: u8, b: u8, a: f32) -> Self {
        Self::Solid(Rgba { r, g, b, a })
    }

    pub fn gradient(g: Gradient) -> Self {
        Self::Gradient(Box::new(g))
    }

    /// Returns `true` if the paint marks at least one pixel, `false`
    /// otherwise. A renderer skips a fill or a stroke that does not.
    pub fn is_visible(&self) -> bool {
        match self {
            Self::Solid(c) => c.a > 0.0,
            Self::Gradient(g) => !g.stops.is_empty(),
        }
    }

    /// The solid color, or the color of the first stop of a gradient, for a
    /// renderer that cannot draw a gradient.
    pub fn primary_color(&self) -> Rgba {
        match self {
            Self::Solid(c) => *c,
            Self::Gradient(g) => g.stops.first().map(|s| s.color).unwrap_or_default(),
        }
    }
}

/// A dash pattern. `array` alternates on and off lengths in path units and
/// repeats, and the pattern starts `offset` units in. [`Dash::new`] rejects
/// an empty array, because a dash with no lengths is a solid stroke, and
/// that case is the `None` of `PathStyle::dash`.
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

/// The SVG default of `stroke-miterlimit`. A join whose miter length, in
/// stroke widths, is above the limit becomes a bevel.
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
    /// `None` is a solid stroke. Boxed because a dash is rare and a `Dash`
    /// inline would add 16 bytes to every style.
    pub dash: Option<Box<Dash>>,
}

impl PathStyle {
    /// Returns `true` if the fill marks the canvas, `false` otherwise.
    pub fn draws_fill(&self) -> bool {
        self.fill.is_visible()
    }

    /// Returns `true` if the stroke marks the canvas, `false` otherwise. A
    /// zero width draws nothing, whatever the paint.
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

/// A clip region. A sub-path closes implicitly, so the caller does not add a
/// line back to its start. `fill_rule` decides what is inside.
#[derive(Clone, Debug, Default)]
pub struct ClipPath {
    segs: Vec<Segment>,
    pub fill_rule: FillRule,
}

impl ClipPath {
    pub fn builder(fill_rule: FillRule) -> ClipPathBuilder {
        ClipPathBuilder::new(fill_rule)
    }

    /// For the wire decoder, which refills a clip in place.
    pub(crate) fn segments_mut(&mut self) -> &mut Vec<Segment> {
        &mut self.segs
    }

    pub fn segments(&self) -> Segments<'_> {
        Segments(self.segs.iter())
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

/// A text run. The glyphs are laid out in text space, with the origin at
/// the left of the baseline and `size` units to the em, and `transform` maps
/// them to the canvas in the convention of the PDF `cm` operator:
///
/// ```text
/// x' = transform[0] * x + transform[2] * y + transform[4]
/// y' = transform[1] * x + transform[3] * y + transform[5]
/// ```
///
/// `text` draws on one line. A tab advances by the width of eight spaces of
/// the face. Any other control character draws nothing, so a newline does
/// not break the line.
///
/// The producer puts the fit to a box, the rotation and the mirroring in
/// the matrix, with [`text_box_affine`]. `family` is the family the
/// producer measured with, after fallback, so a client lays the text out
/// as the server did. An empty family is the default Sans. `weight` is the
/// CSS weight, 400 for Regular and 700 for Bold.
#[derive(Clone, Debug)]
pub struct TextNode {
    pub fill: Rgba,
    pub stroke: Rgba,
    pub stroke_width: f32,
    pub transform: [f32; 6],
    pub size: f32,
    /// A `Box<str>` saves 8 bytes over a `String`, which keeps [`Element`]
    /// the size of its `Path` variant.
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

/// The affine that scales by `(sx, sy)`, rotates by `angle_deg` and then
/// translates to `(e, f)`.
fn rotate_scale_at(sx: f32, sy: f32, angle_deg: f32, e: f32, f: f32) -> [f32; 6] {
    let (st, ct) = angle_deg.to_radians().sin_cos();
    [sx * ct, sx * st, -sy * st, sy * ct, e, f]
}

fn translate(e: f32, f: f32) -> [f32; 6] {
    [1.0, 0.0, 0.0, 1.0, e, f]
}

fn apply_affine(m: [f32; 6], x: f32, y: f32) -> (f32, f32) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// The affine that fits the text into a box of `bw` by `bh` centred on
/// `(cx, cy)` and rotated by `angle_deg`. The text is measured with
/// [`crate::text`], the same code as the renderers, so the producer and the
/// backend agree. A negative `bw` mirrors horizontally and a negative `bh`
/// vertically. Text that measures zero gets a translation to `(cx, cy)`,
/// and the renderer draws nothing for it anyway.
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
    if !crate::text::drawable_size(size) || text.is_empty() {
        return translate(cx, cy);
    }
    let font = crate::text::resolve(family, weight, style);
    let face = font.face();
    let orig_w = crate::text::measure_width_with(face, text, size) as f32;
    let orig_h = crate::text::measure_height_with(face, size) as f32;
    // A huge size overflows the measurement, and the scale below would put
    // a non-finite number in the matrix.
    if !orig_w.is_finite() || !orig_h.is_finite() || orig_w <= 0.0 || orig_h <= 0.0 {
        return translate(cx, cy);
    }
    rotate_scale_at(bw / orig_w, bh / orig_h, angle_deg, cx, cy)
}

/// A bitmap. `id` names an asset uploaded before, with `Message::Asset` on
/// the wire, and the renderer resolves it to pixels. `transform` maps the
/// image pixels, `(0..img_w, 0..img_h)`, to the canvas, in the convention
/// of [`TextNode::transform`]. [`bitmap_box_affine`] computes it for a box.
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

/// The affine that fits an image of `img_w` by `img_h` pixels into a box of
/// `w` by `h` centred on `(cx, cy)` and rotated by `angle_deg`. A negative
/// `w` mirrors horizontally and a negative `h` vertically. An empty image
/// gets a translation to `(cx, cy)`.
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
    // Move the centre of the rotated and scaled image onto (cx, cy).
    let m = rotate_scale_at(w / img_w as f32, h / img_h as f32, angle_deg, 0.0, 0.0);
    let (ox, oy) = apply_affine(m, img_w as f32 * 0.5, img_h as f32 * 0.5);
    [m[0], m[1], m[2], m[3], cx - ox, cy - oy]
}

/// The verb byte of a [`Segment`] on the wire. Only the codec uses it. The
/// discriminants are the `verbMove`, `verbLine`, `verbQuad` and `verbCubic`
/// values of `schema/scene.capnp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SegmentKind {
    Move = 0,
    Line = 1,
    Quad = 2,
    Cubic = 3,
}

impl SegmentKind {
    /// Returns `None` for an unknown verb.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Move),
            1 => Some(Self::Line),
            2 => Some(Self::Quad),
            3 => Some(Self::Cubic),
            _ => None,
        }
    }

    /// How many floats the verb takes from the coord stream.
    pub fn coords(self) -> usize {
        match self {
            Self::Move | Self::Line => 2,
            Self::Quad => 4,
            Self::Cubic => 6,
        }
    }
}

/// One segment of a [`Path`] or a [`ClipPath`]. A verb and its coordinates
/// are one value, so only the codec keeps a verb stream and a coord
/// stream in step.
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

impl Segment {
    pub fn kind(self) -> SegmentKind {
        match self {
            Self::Move { .. } => SegmentKind::Move,
            Self::Line { .. } => SegmentKind::Line,
            Self::Quad { .. } => SegmentKind::Quad,
            Self::Cubic { .. } => SegmentKind::Cubic,
        }
    }

    /// The coordinates in wire order. Only the first [`SegmentKind::coords`]
    /// slots mean anything. A fixed array spares the encoder an allocation
    /// per segment.
    pub(crate) fn wire_coords(self) -> [f32; 6] {
        match self {
            Self::Move { x, y } | Self::Line { x, y } => [x, y, 0.0, 0.0, 0.0, 0.0],
            Self::Quad { cx, cy, x, y } => [cx, cy, x, y, 0.0, 0.0],
            Self::Cubic {
                c1x,
                c1y,
                c2x,
                c2y,
                x,
                y,
            } => [c1x, c1y, c2x, c2y, x, y],
        }
    }
}

/// An iterator over the segments of a path.
#[must_use = "Segments yields nothing unless iterated"]
pub struct Segments<'a>(std::slice::Iter<'a, Segment>);

impl<'a> Segments<'a> {
    /// The same walk with every quadratic elevated to a cubic.
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
        self.0.next().copied()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for Segments<'_> {}

/// The two control points of the cubic equal to the quadratic from `p0`
/// through the control `(cx, cy)` to `(x, y)`.
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

/// [`Segments`] with every quadratic elevated to a cubic, for a backend
/// that has no quadratic operator. The elevation needs the current point,
/// and tracking it here keeps it out of every backend.
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
                    // A quad with no current point has nothing to elevate from.
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

/// The geometry half of [`PathBuilder`] and [`ClipPathBuilder`], so the arc
/// expansion is written once.
#[derive(Default)]
struct GeometryBuilder {
    segs: Vec<Segment>,
    last_point: Option<(f32, f32)>,
}

impl GeometryBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.segs.push(Segment::Move { x, y });
        self.last_point = Some((x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.segs.push(Segment::Line { x, y });
        self.last_point = Some((x, y));
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.segs.push(Segment::Quad { cx, cy, x, y });
        self.last_point = Some((x, y));
    }

    fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.segs.push(Segment::Cubic {
            c1x,
            c1y,
            c2x,
            c2y,
            x,
            y,
        });
        self.last_point = Some((x, y));
    }

    /// Append an SVG endpoint arc as cubics. With no current point the arc
    /// becomes a move to `(x, y)`, and a degenerate arc becomes a line.
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
                self.segs.push(Segment::Cubic {
                    c1x: p1.x as f32,
                    c1y: p1.y as f32,
                    c2x: p2.x as f32,
                    c2y: p2.y as f32,
                    x: p3.x as f32,
                    y: p3.y as f32,
                });
            }
        }
        self.last_point = Some((x, y));
    }
}

/// A style and the [`Segment`]s it applies to. [`Path::builder`] and
/// [`Scene::path`] build one, and [`Path::segments`] reads it back.
#[derive(Clone, Debug, Default)]
pub struct Path {
    pub style: PathStyle,
    segs: Vec<Segment>,
}

impl Path {
    pub fn builder(style: PathStyle) -> PathBuilder {
        PathBuilder::new(style)
    }

    /// For the wire decoder, which refills a path in place.
    pub(crate) fn segments_mut(&mut self) -> &mut Vec<Segment> {
        &mut self.segs
    }

    pub fn segments(&self) -> Segments<'_> {
        Segments(self.segs.iter())
    }
}

/// One node of a [`Scene`]. A clip holds the elements it applies to.
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

/// The draw list a front end builds and a [`Renderer`](crate::renderer::Renderer)
/// replays. [`Self::path`] returns a [`PathScope`] that commits its path on
/// drop, and [`Self::clip`] and [`Self::clip_rect`] return a [`ClipScope`]
/// that wraps the elements drawn while it lives into an
/// [`Element::Clipped`] on drop, so a clip cannot be left open. A
/// `PathScope` with no geometry commits nothing. Only [`Self::add_path`]
/// can add an empty path.
///
/// An arc is stored as cubics, so a renderer sees only move, line, quad and
/// cubic.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub width: f32,
    pub height: f32,
    pub elements: Vec<Element>,
}

/// The tolerance of the arc to cubic conversion.
const ARC_TOLERANCE: f64 = 0.1;

impl Scene {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            elements: Vec::new(),
        }
    }

    /// Returns `true` if any element, inside a clip or not, is a bitmap,
    /// `false` otherwise. A host uses it to tell the user once that the
    /// backend draws the frame without them.
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

    /// Append a [`Path`] built elsewhere. [`Self::path`] builds one in place.
    pub fn add_path(&mut self, path: Path) {
        self.elements.push(Element::Path(path));
    }

    /// Begin a path. The [`PathScope`] commits it to [`Self::elements`] on
    /// drop, or discards it if no geometry was added.
    pub fn path(&mut self, style: PathStyle) -> PathScope<'_> {
        PathScope {
            scene: self,
            builder: PathBuilder::new(style),
        }
    }

    /// Begin a clip. The [`ClipScope`] derefs to this scene, and on drop it
    /// wraps the elements added since into an [`Element::Clipped`]. A nested
    /// clip is a [`Self::clip`] through the deref.
    pub fn clip(&mut self, clip: ClipPath) -> ClipScope<'_> {
        let mark = self.elements.len();
        ClipScope {
            scene: self,
            clip,
            mark,
        }
    }

    /// Begin a clip of `w` by `h` centred on `(cx, cy)` and rotated by
    /// `angle_deg`.
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

/// The path under construction by [`Scene::path`]. The geometry methods take
/// `&mut self`, so a loop can build a path, and drop commits it to the
/// scene, or discards it if it has no geometry.
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
        if !builder.geom.segs.is_empty() {
            self.scene.elements.push(Element::Path(builder.build()));
        }
    }
}

/// The clip under construction by [`Scene::clip`] or [`Scene::clip_rect`].
/// It derefs to the scene, and `mark` is where its elements begin. On drop
/// it moves `elements[mark..]` into an [`Element::Clipped`] at `mark`. A
/// nested scope has a later mark and drops first, so the tree is well formed.
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

/// Builds a [`Path`] by value. `Default` exists so that the `Drop` of
/// [`PathScope`] can take the builder out with `mem::take`.
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

    /// Append an SVG endpoint arc as cubics. With no current point the arc
    /// becomes a move to `(x, y)`, and a degenerate arc becomes a line.
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
            segs: self.geom.segs,
        }
    }
}

/// Builds a [`ClipPath`] by value. A sub-path closes implicitly, so no
/// closing line is needed.
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

    /// Append an SVG endpoint arc as cubics. With no current point the arc
    /// becomes a move to `(x, y)`, and a degenerate arc becomes a line.
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
            segs: self.geom.segs,
            fill_rule: self.fill_rule,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubics_elevates_quads_against_the_current_point() {
        let path = Path::builder(PathStyle::default())
            .move_to(0.0, 0.0)
            .quad_to(3.0, 3.0, 6.0, 0.0)
            .build();
        let segs: Vec<_> = path.segments().cubics().collect();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0], Segment::Move { x: 0.0, y: 0.0 });
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
    fn builders_append_one_segment_per_call() {
        let clip = ClipPath::builder(FillRule::EvenOdd)
            .move_to(0.0, 0.0)
            .line_to(10.0, 0.0)
            .quad_to(15.0, 5.0, 10.0, 10.0)
            .cubic_to(8.0, 8.0, 4.0, 6.0, 0.0, 10.0)
            .build();
        let kinds: Vec<_> = clip.segments().map(|s| s.kind()).collect();
        assert_eq!(
            kinds,
            [
                SegmentKind::Move,
                SegmentKind::Line,
                SegmentKind::Quad,
                SegmentKind::Cubic
            ]
        );
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
    fn clip_builder_arc_to_expands_to_cubics() {
        let clip = ClipPath::builder(FillRule::NonZero)
            .move_to(0.0, 0.0)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 0.0)
            .build();
        // The tolerance decides how many cubics the arc becomes.
        let kinds: Vec<_> = clip.segments().map(|s| s.kind()).collect();
        assert_eq!(kinds[0], SegmentKind::Move);
        assert!(kinds.len() > 1);
        assert!(kinds[1..].iter().all(|&k| k == SegmentKind::Cubic));
    }

    #[test]
    fn clip_builder_arc_to_without_current_point_moves() {
        let clip = ClipPath::builder(FillRule::NonZero)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 10.0)
            .build();
        let segs: Vec<_> = clip.segments().collect();
        assert_eq!(segs, [Segment::Move { x: 10.0, y: 10.0 }]);
    }

    #[test]
    fn standalone_path_builder_builds_the_segments_it_was_given() {
        let p = Path::builder(PathStyle::default())
            .move_to(0.0, 0.0)
            .line_to(10.0, 0.0)
            .quad_to(15.0, 5.0, 10.0, 10.0)
            .build();
        let segs: Vec<_> = p.segments().collect();
        assert_eq!(
            segs,
            [
                Segment::Move { x: 0.0, y: 0.0 },
                Segment::Line { x: 10.0, y: 0.0 },
                Segment::Quad {
                    cx: 15.0,
                    cy: 5.0,
                    x: 10.0,
                    y: 10.0
                },
            ]
        );
    }

    #[test]
    fn scene_path_builder_commits_the_expanded_arc() {
        let mut scene = Scene::new(10.0, 10.0);
        scene
            .path(PathStyle::default())
            .move_to(0.0, 0.0)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 0.0);
        let Element::Path(p) = &scene.elements[0] else {
            panic!("expected a path");
        };
        assert!(p.segments().any(|s| s.kind() == SegmentKind::Cubic));
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
        let kinds: Vec<_> = p.segments().map(|s| s.kind()).collect();
        assert_eq!(kinds, [SegmentKind::Move, SegmentKind::Line]);
    }

    #[test]
    fn clip_wraps_only_elements_drawn_inside() {
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
        let segs: Vec<_> = p.segments().collect();
        assert_eq!(segs, [Segment::Move { x: 1.0, y: 1.0 }]);
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
