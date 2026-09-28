//! The value types of a [`Scene`] and the builders that make them.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::asset::ImageError;

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

/// A color ramp along an axis. The stops rise by offset, in [0, 1].
///
/// [`Paint::linear`] and [`Paint::radial`] build one, and they resolve a
/// gradient with no extent, so an unresolved one cannot be built and then
/// put into a [`Paint`]:
///
/// ```compile_fail,E0599
/// use sinteract::scene::Gradient;
/// let g = Gradient::linear(0.0, 0.0, 10.0, 0.0, Vec::new());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    geom: GradientGeom,
    stops: Vec<Stop>,
    spread: SpreadMode,
}

impl Gradient {
    /// The only constructor, which the wire decoder uses because it reads
    /// the three parts apart. It raises a stop that is below the one before
    /// it, and clamps every offset to [0, 1], as SVG and Skia do, and
    /// shortens an axis too long to measure.
    pub(crate) fn new(geom: GradientGeom, mut stops: Vec<Stop>, spread: SpreadMode) -> Self {
        let mut prev = 0.0;
        for stop in &mut stops {
            stop.offset = stop.offset.clamp(prev, 1.0);
            prev = stop.offset;
        }
        Self {
            geom: measurable(geom),
            stops,
            spread,
        }
    }

    pub fn geom(&self) -> GradientGeom {
        self.geom
    }

    pub fn stops(&self) -> &[Stop] {
        &self.stops
    }

    pub fn spread(&self) -> SpreadMode {
        self.spread
    }
}

/// Scales a linear axis too long for an f32 about the origin. tiny-skia
/// measures the axis in f32, finds it infinite and paints the first stop,
/// where the svg and the pdf paint the ramp. The scaling moves the color
/// under a path by less than a step of an f32. A coordinate that is not
/// finite passes through, and the scene drops the element.
fn measurable(geom: GradientGeom) -> GradientGeom {
    let GradientGeom::Linear { x0, y0, x1, y1 } = geom else {
        return geom;
    };
    let length = (x1 as f64 - x0 as f64).hypot(y1 as f64 - y0 as f64);
    if !(length > f32::MAX as f64 && length.is_finite()) {
        return geom;
    }
    // Half of the longest an f32 measures, so the length still has room after
    // the four products round.
    let k = (0.5 * f32::MAX as f64 / length) as f32;
    GradientGeom::Linear {
        x0: x0 * k,
        y0: y0 * k,
        x1: x1 * k,
        y1: y1 * k,
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

    /// A ramp along the line from `(x0, y0)` to `(x1, y1)`, with
    /// [`SpreadMode::Pad`] outside it.
    pub fn linear(x0: f32, y0: f32, x1: f32, y1: f32, stops: Vec<Stop>) -> Self {
        Self::gradient(Gradient::new(
            GradientGeom::Linear { x0, y0, x1, y1 },
            stops,
            SpreadMode::Pad,
        ))
    }

    /// A ramp outward from `(cx, cy)` to `radius`, with [`SpreadMode::Pad`]
    /// outside it.
    pub fn radial(cx: f32, cy: f32, radius: f32, stops: Vec<Stop>) -> Self {
        Self::gradient(Gradient::new(
            GradientGeom::Radial { cx, cy, radius },
            stops,
            SpreadMode::Pad,
        ))
    }

    /// The paint with `spread` outside its ramp. The spread does not decide
    /// whether a gradient has extent, so a solid color, which is what one
    /// with no extent became, comes back as it is.
    pub fn with_spread(mut self, spread: SpreadMode) -> Self {
        if let Self::Gradient(g) = &mut self {
            g.spread = spread;
        }
        self
    }

    /// A gradient paint, or the solid color of the last stop when the
    /// gradient has no extent, a radius or an axis of 2^-15 or less. SVG
    /// paints the last stop in that case, and tiny-skia and the pdf each did
    /// something else. The wire decoder calls it, because it reads the three
    /// parts apart.
    pub(crate) fn gradient(g: Gradient) -> Self {
        // The threshold of tiny-skia, which cannot tell a gradient below it
        // from one of no extent at all, so the scene decides here and the
        // three backends agree.
        const NO_EXTENT: f32 = 1.0 / (1 << 15) as f32;
        let no_extent = match g.geom {
            GradientGeom::Linear { x0, y0, x1, y1 } => (x1 - x0).hypot(y1 - y0) <= NO_EXTENT,
            GradientGeom::Radial { radius, .. } => radius <= NO_EXTENT,
        };
        match g.stops.last() {
            Some(last) if no_extent => Self::Solid(last.color),
            _ => Self::Gradient(Box::new(g)),
        }
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

    /// Returns `true` if every float of the paint is finite, `false`
    /// otherwise.
    pub(crate) fn is_finite(&self) -> bool {
        match self {
            Self::Solid(c) => c.a.is_finite(),
            Self::Gradient(g) => {
                let geom = match g.geom {
                    GradientGeom::Linear { x0, y0, x1, y1 } => all_finite(&[x0, y0, x1, y1]),
                    GradientGeom::Radial { cx, cy, radius } => all_finite(&[cx, cy, radius]),
                };
                geom && g.stops.iter().all(|s| all_finite(&[s.offset, s.color.a]))
            }
        }
    }
}

/// A dash pattern. `array` alternates on and off lengths in path units and
/// repeats, and the pattern starts `offset` units in. The array has an even
/// length, its lengths are not negative and their sum is positive and
/// finite, and the offset is finite.
#[derive(Clone, Debug, PartialEq)]
pub struct Dash {
    array: Box<[f32]>,
    offset: f32,
}

impl Dash {
    /// Repeats an odd array to an even one, as SVG and PDF do. Returns
    /// `None`, the solid stroke of `PathStyle::dash`, for an array that SVG
    /// draws solid: empty, with a negative length or with lengths that sum
    /// to zero. A length or an offset that is not finite is `None` too.
    pub fn new(array: impl Into<Vec<f32>>, offset: f32) -> Option<Self> {
        let mut array = array.into();
        if array.len() % 2 == 1 {
            array.extend_from_within(..);
        }
        let sum: f32 = array.iter().sum();
        let valid = array.iter().all(|v| *v >= 0.0) && sum > 0.0 && sum.is_finite();
        (valid && offset.is_finite()).then(|| Self {
            array: array.into(),
            offset,
        })
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
    /// Closes every sub-path, so the stroke joins at the start of each one.
    /// A fill closes them either way.
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

    /// Raises a miter limit below 1 to 1. The limit compares a ratio that
    /// is never below 1, so the two say the same thing, and SVG rejects a
    /// smaller one and falls back to [`DEFAULT_MITER_LIMIT`] instead. The
    /// scene and the decoder normalize, so a renderer writes the limit as
    /// it is.
    pub(crate) fn normalize(&mut self) {
        self.miter_limit = self.miter_limit.max(1.0);
    }

    /// Returns `true` if every float of the style is finite, `false`
    /// otherwise. A [`Dash`] is always finite.
    pub(crate) fn is_finite(&self) -> bool {
        self.fill.is_finite()
            && self.stroke.is_finite()
            && all_finite(&[self.stroke_width, self.miter_limit])
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
/// line back to its start. `fill_rule` decides what is inside. A clip with
/// no area, such as one with no segments, hides everything it holds.
#[derive(Clone, Debug, Default)]
pub struct ClipPath {
    segs: Vec<Segment>,
    pub fill_rule: FillRule,
}

impl ClipPath {
    /// Begin a clip at `(x, y)`.
    pub fn builder(fill_rule: FillRule, x: f32, y: f32) -> ClipPathBuilder {
        ClipPathBuilder::new(fill_rule, x, y)
    }

    /// For the wire decoder, which refills a clip in place.
    pub(crate) fn segments_mut(&mut self) -> &mut Vec<Segment> {
        &mut self.segs
    }

    pub fn segments(&self) -> Segments<'_> {
        Segments(self.segs.iter())
    }

    /// Returns `true` if every coordinate of the clip is finite, `false`
    /// otherwise.
    pub(crate) fn is_finite(&self) -> bool {
        self.segs.iter().all(|s| s.is_finite())
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

/// A rectangle of `w` by `h` centred on `(cx, cy)` and rotated by
/// `angle_deg`. A negative `w` mirrors horizontally and a negative `h`
/// vertically.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RotatedRect {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle_deg: f32,
}

impl RotatedRect {
    /// The affine that maps a box of `natural_w` by `natural_h`, centred on
    /// the origin, onto this rectangle. A box with no area gets a
    /// translation to the center.
    fn affine(self, natural_w: f32, natural_h: f32) -> [f32; 6] {
        if natural_w <= 0.0 || natural_h <= 0.0 {
            return translate(self.cx, self.cy);
        }
        rotate_scale_at(
            self.w / natural_w,
            self.h / natural_h,
            self.angle_deg,
            self.cx,
            self.cy,
        )
    }
}

impl From<RotatedRect> for ClipPath {
    /// The outline of `rect`. A rectangle covers the same area under either
    /// fill rule, so the clip takes the default.
    fn from(rect: RotatedRect) -> Self {
        let m = rect.affine(1.0, 1.0);
        let p0 = apply_affine(m, -0.5, -0.5);
        let p1 = apply_affine(m, 0.5, -0.5);
        let p2 = apply_affine(m, 0.5, 0.5);
        let p3 = apply_affine(m, -0.5, 0.5);
        ClipPath::builder(FillRule::default(), p0.0, p0.1)
            .line_to(p1.0, p1.1)
            .line_to(p2.0, p2.1)
            .line_to(p3.0, p3.1)
            .build()
    }
}

/// A text run. The glyphs are laid out in text space, as [`TextSpec`] says.
/// `transform` maps them to the canvas in the convention of the PDF `cm`
/// operator:
///
/// ```text
/// x' = transform[0] * x + transform[2] * y + transform[4]
/// y' = transform[1] * x + transform[3] * y + transform[5]
/// ```
///
/// [`TextSpec::fit`] puts the fit to a box, the rotation and the mirroring
/// in the matrix. A gradient of `fill` or `stroke` is in canvas space, as
/// the gradient of a [`Path`], so it does not turn with the text.
#[derive(Clone, Debug)]
pub struct Text {
    pub fill: Paint,
    pub stroke: Paint,
    pub stroke_width: f32,
    pub transform: [f32; 6],
    pub spec: TextSpec,
    pub underline: bool,
}

impl Text {
    /// Returns `true` if the fill marks the canvas, `false` otherwise.
    pub fn draws_fill(&self) -> bool {
        self.fill.is_visible()
    }

    /// Returns `true` if the stroke marks the canvas, `false` otherwise. A
    /// zero width draws nothing, whatever the paint.
    pub fn draws_stroke(&self) -> bool {
        self.stroke.is_visible() && self.stroke_width > 0.0
    }

    /// Returns `true` if every float of the text is finite, `false`
    /// otherwise.
    pub(crate) fn is_finite(&self) -> bool {
        self.fill.is_finite()
            && self.stroke.is_finite()
            && all_finite(&[self.stroke_width, self.spec.size])
            && all_finite(&self.transform)
    }
}

impl Default for Text {
    fn default() -> Self {
        Self {
            fill: Paint::default(),
            stroke: Paint::default(),
            stroke_width: 0.0,
            transform: translate(0.0, 0.0),
            spec: TextSpec::default(),
            underline: false,
        }
    }
}

/// The fields that set the measure of a [`Text`]. The text space has `size`
/// units to the em and the origin at the center of a box that spans the
/// advance of the text and the ascender to the descender of the face.
///
/// `text` draws on one line. A tab advances by the width of eight spaces of
/// the face. Any other control character draws nothing, so a newline does
/// not break the line.
///
/// `family` is [`crate::text::TextMetrics::family`], the family after
/// fallback, so a view lays the text out as the engine did. An empty
/// family is the default Sans. `weight` is the CSS weight, 400 for Regular
/// and 700 for Bold.
#[derive(Clone, Debug)]
pub struct TextSpec {
    pub size: f32,
    /// A `Box<str>` saves 8 bytes over a `String`, and [`Text`] is the
    /// largest variant of [`Element`].
    pub family: Box<str>,
    pub weight: u16,
    pub style: FontStyle,
    pub text: String,
}

impl Default for TextSpec {
    fn default() -> Self {
        Self {
            size: 0.0,
            family: Box::default(),
            weight: 400,
            style: FontStyle::Normal,
            text: String::new(),
        }
    }
}

impl TextSpec {
    /// A [`Text`] that draws this text into `rect`. The family becomes the
    /// family after fallback, so a view measures with the face of the
    /// engine. The text has no fill and no stroke until the caller sets
    /// them. Returns `None` when the text draws nothing, because
    /// [`crate::text::measure`] returns `None` or the text measures zero
    /// wide.
    pub fn fit(self, rect: RotatedRect) -> Option<Text> {
        let metrics =
            crate::text::measure(&self.family, self.weight, self.style, self.size, &self.text)?;
        if metrics.width() <= 0.0 {
            return None;
        }
        Some(Text {
            transform: rect.affine(metrics.width(), metrics.height()),
            spec: Self {
                family: metrics.family().into(),
                ..self
            },
            ..Text::default()
        })
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

fn all_finite(values: &[f32]) -> bool {
    values.iter().all(|v| v.is_finite())
}

/// The file of a PNG, a JPEG, a GIF or a WebP of at most
/// [`MAX_IMAGE_PIXELS`](crate::asset::MAX_IMAGE_PIXELS), with its size on
/// the screen. A clone shares the file. Two images are equal when their
/// files are, so a renderer or a session finds an image again by its
/// content.
#[derive(Clone)]
pub struct Image(Arc<ImageData>);

struct ImageData {
    file: Box<[u8]>,
    /// The hash of `file`, so a lookup does not read the file again.
    hash: u64,
    width: u32,
    height: u32,
}

impl Image {
    /// Read the header of `file` and hash it. Returns an error if `file` is
    /// not a PNG, a JPEG, a GIF or a WebP, or has more than
    /// [`MAX_IMAGE_PIXELS`](crate::asset::MAX_IMAGE_PIXELS).
    /// [`crate::asset::fit_image`] shrinks a larger one first. The size is
    /// the one after the EXIF orientation of a JPEG.
    pub fn new(file: Vec<u8>) -> Result<Image, ImageError> {
        let (width, height) = crate::asset::screen_size(&file)?;
        let hash = {
            let mut hasher = std::hash::DefaultHasher::new();
            file.hash(&mut hasher);
            hasher.finish()
        };
        Ok(Image(Arc::new(ImageData {
            file: file.into_boxed_slice(),
            hash,
            width,
            height,
        })))
    }

    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    pub(crate) fn file(&self) -> &[u8] {
        &self.0.file
    }
}

impl PartialEq for Image {
    fn eq(&self, other: &Image) -> bool {
        self.0.hash == other.0.hash
            && (Arc::ptr_eq(&self.0, &other.0) || self.0.file == other.0.file)
    }
}

impl Eq for Image {}

impl Hash for Image {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.0.hash);
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Image({}x{}, {} bytes, {:016x})",
            self.0.width,
            self.0.height,
            self.0.file.len(),
            self.0.hash
        )
    }
}

/// A bitmap. `transform` maps the unit square centred on the origin, which
/// the image fills whatever its size, to the canvas in the convention of
/// [`Text::transform`]. So the place of a bitmap does not depend on the
/// pixels of its image. [`Bitmap::fit`] computes it for a [`RotatedRect`].
#[derive(Clone, Debug)]
pub struct Bitmap {
    pub image: Image,
    pub transform: [f32; 6],
    pub sampling: Sampling,
}

impl Bitmap {
    /// The bitmap of `image`, drawn into `rect`.
    pub fn fit(image: Image, rect: RotatedRect) -> Self {
        Self {
            image,
            transform: rect.affine(1.0, 1.0),
            sampling: Sampling::Smooth,
        }
    }

    /// Returns `true` if every float of the transform is finite, `false`
    /// otherwise.
    pub(crate) fn is_finite(&self) -> bool {
        all_finite(&self.transform)
    }
}

/// How a [`Bitmap`] reads its image between pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Sampling {
    /// Blends the pixels around the point, so a scaled image looks smooth.
    #[default]
    Smooth = 0,
    /// Takes the nearest pixel, so a scaled image keeps hard pixel edges.
    Nearest = 1,
}

/// The verb byte of a [`Segment`] on the wire. Only the codec uses it. The
/// discriminants are the `verbMove`, `verbLine`, `verbQuad` and `verbCubic`
/// values of `schema/scene.capnp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SegmentKind {
    Move = 0,
    Line = 1,
    Quad = 2,
    Cubic = 3,
}

impl SegmentKind {
    /// Returns `None` for an unknown verb.
    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Move),
            1 => Some(Self::Line),
            2 => Some(Self::Quad),
            3 => Some(Self::Cubic),
            _ => None,
        }
    }

    /// How many floats the verb takes from the coord stream.
    pub(crate) fn coords(self) -> usize {
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
    pub(crate) fn kind(self) -> SegmentKind {
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

    /// Returns `true` if every coordinate is finite, `false` otherwise.
    pub(crate) fn is_finite(self) -> bool {
        all_finite(&self.wire_coords())
    }
}

/// An iterator over the segments of a path. They are empty or begin with a
/// [`Segment::Move`], and a segment that is not a move follows every move.
/// The builders take the start point, the decoder begins a path of the wire
/// that lacks one at `(0, 0)`, and both drop a move that no segment follows.
#[must_use = "Segments yields nothing unless iterated"]
pub struct Segments<'a>(std::slice::Iter<'a, Segment>);

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

/// The tolerance of the arc to cubic conversion.
const ARC_TOLERANCE: f64 = 0.1;

/// The most cubics that one turn of an arc becomes.
const ARC_CUBICS_PER_TURN: f64 = 64.0;

/// The geometry half of [`PathBuilder`] and [`ClipPathBuilder`], so the arc
/// expansion is written once. It begins with a move to the start point, so
/// there is always a current point. A move that no segment follows draws
/// nothing, so a move replaces a move before it and [`Self::finish`] drops
/// a move at the end.
struct GeometryBuilder {
    segs: Vec<Segment>,
    last_point: (f32, f32),
}

impl GeometryBuilder {
    fn new(x: f32, y: f32) -> Self {
        Self {
            segs: vec![Segment::Move { x, y }],
            last_point: (x, y),
        }
    }

    fn move_to(&mut self, x: f32, y: f32) {
        push_segment(&mut self.segs, Segment::Move { x, y });
        self.last_point = (x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.segs.push(Segment::Line { x, y });
        self.last_point = (x, y);
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.segs.push(Segment::Quad { cx, cy, x, y });
        self.last_point = (x, y);
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
        self.last_point = (x, y);
    }

    /// Append an SVG endpoint arc as cubics. A degenerate arc becomes a line.
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
        // kurbo turns a float that is not finite into no segment at all. A
        // line to NaN keeps it, so the scene drops the element.
        if !all_finite(&[rx, ry, rotation_deg, x, y]) {
            return self.line_to(f32::NAN, f32::NAN);
        }
        let (x1, y1) = self.last_point;
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
        for el in arc.append_iter(arc_tolerance(arc.radii.x.max(arc.radii.y))) {
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
        self.last_point = (x, y);
    }

    /// Take the segments, with no move at the end.
    fn finish(&mut self) -> Vec<Segment> {
        let mut segs = std::mem::take(&mut self.segs);
        end_segments(&mut segs);
        segs
    }
}

/// The tolerance of the arc to cubic conversion for an arc of `radius`.
/// kurbo splits a turn into `(1.1163 * radius / tolerance)^(1/6)` cubics, so
/// a fixed tolerance turns a radius of 1e38 into millions of them. The
/// tolerance grows so a turn takes at most [`ARC_CUBICS_PER_TURN`] cubics,
/// which only happens for a radius above 6e9, far past what a renderer draws.
fn arc_tolerance(radius: f64) -> f64 {
    ARC_TOLERANCE.max(1.1163 * radius / ARC_CUBICS_PER_TURN.powi(6))
}

/// Append `seg` to `segs`. A move replaces a move that ends `segs`.
pub(crate) fn push_segment(segs: &mut Vec<Segment>, seg: Segment) {
    match (seg, segs.last_mut()) {
        (Segment::Move { .. }, Some(last @ Segment::Move { .. })) => *last = seg,
        _ => segs.push(seg),
    }
}

/// Drop the move that ends `segs`, since no segment follows it.
pub(crate) fn end_segments(segs: &mut Vec<Segment>) {
    if matches!(segs.last(), Some(Segment::Move { .. })) {
        segs.pop();
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
    /// Begin a path at `(x, y)`.
    pub fn builder(style: PathStyle, x: f32, y: f32) -> PathBuilder {
        PathBuilder::new(style, x, y)
    }

    /// For the wire decoder, which refills a path in place.
    pub(crate) fn segments_mut(&mut self) -> &mut Vec<Segment> {
        &mut self.segs
    }

    pub fn segments(&self) -> Segments<'_> {
        Segments(self.segs.iter())
    }

    /// Returns `true` if every float of the style and of the segments is
    /// finite, `false` otherwise.
    pub(crate) fn is_finite(&self) -> bool {
        self.style.is_finite() && self.segs.iter().all(|s| s.is_finite())
    }
}

/// One node of a [`Scene`]. A clip holds the elements it applies to. A
/// renderer may draw nothing for an element that reaches more than 2^28
/// output pixels from the origin.
#[derive(Clone, Debug)]
pub enum Element {
    Path(Path),
    Clipped {
        clip: ClipPath,
        elements: Vec<Element>,
    },
    Text(Text),
    Bitmap(Bitmap),
    /// The elements draw into a transparent layer, and the layer draws with
    /// `opacity`, so two of them that overlap do not darken where they meet.
    /// `opacity` is above 0 and below 1.
    Layer {
        opacity: f32,
        elements: Vec<Element>,
    },
}

impl Element {
    /// Returns `true` if every float of the element, and of what a clip
    /// holds, is finite, `false` otherwise.
    fn is_finite(&self) -> bool {
        match self {
            Self::Path(p) => p.is_finite(),
            Self::Clipped { clip, elements } => {
                clip.is_finite() && elements.iter().all(Self::is_finite)
            }
            Self::Text(t) => t.is_finite(),
            Self::Bitmap(b) => b.is_finite(),
            Self::Layer { opacity, elements } => {
                opacity.is_finite() && elements.iter().all(Self::is_finite)
            }
        }
    }
}

/// What a layer of some opacity does with the elements it holds.
pub(crate) enum LayerOpacity {
    /// They draw nothing, because the opacity is 0 or less, or not finite.
    Hidden,
    /// They draw as if no layer held them, because the opacity is 1 or more.
    Opaque,
    /// They draw in a layer of this opacity.
    Translucent(f32),
}

impl LayerOpacity {
    pub(crate) fn of(opacity: f32) -> Self {
        if !opacity.is_finite() || opacity <= 0.0 {
            Self::Hidden
        } else if opacity >= 1.0 {
            Self::Opaque
        } else {
            Self::Translucent(opacity)
        }
    }
}

/// The draw list a front end builds and a
/// [`Renderer`](crate::renderer::Renderer) replays. [`Self::path`] returns a
/// [`PathScope`] that commits its path on drop, and [`Self::clip`] and
/// [`Self::layer`] wrap the elements that their closure draws into an
/// [`Element::Clipped`] or an [`Element::Layer`], so a clip or a layer cannot
/// be left open. A move that no segment follows draws nothing, so the
/// builders drop it. A `PathScope` with no segment past its moves commits
/// nothing, and [`PathBuilder::build`] returns a path with no segments.
///
/// An arc is stored as cubics, so a renderer sees only move, line, quad and
/// cubic.
///
/// A float that is not finite has no drawing, so the scene drops an element
/// that holds one, and a clip whose path holds one drops with all it holds.
///
/// A clip or a layer inside [`MAX_NESTING`] others drops with all it holds,
/// so a scene has the same drawing on the wire as on a local display.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    width: f32,
    height: f32,
    elements: Vec<Element>,
    /// How many clips and layers hold the elements.
    depth: usize,
}

/// The most clips and layers, one inside another, of a scene. Each one
/// costs the wire three levels of nesting, and a reader of the wire stops
/// at 64.
pub const MAX_NESTING: usize = 16;

impl Scene {
    /// A scene of `width` by `height`. A size that describes no frame, which
    /// is one that is not finite or is negative, is 0. A scene of no width or
    /// no height is empty, as the empty image of a producer is.
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width: frame_size(width),
            height: frame_size(height),
            elements: Vec::new(),
            depth: 0,
        }
    }

    /// For the wire decoder, which skips an element that is not finite as it
    /// reads it, so it does not walk the tree again.
    pub(crate) fn decoded(width: f32, height: f32, elements: Vec<Element>) -> Self {
        debug_assert!(elements.iter().all(Element::is_finite));
        Self {
            elements,
            ..Self::new(width, height)
        }
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    /// The elements in drawing order. None holds a float that is not finite.
    pub fn elements(&self) -> &[Element] {
        &self.elements
    }

    /// Append a [`Path`] built elsewhere. [`Self::path`] builds one in place.
    pub fn add_path(&mut self, mut path: Path) {
        // The finiteness first, so a limit that is not finite still drops
        // the path instead of becoming 1.
        if path.is_finite() {
            path.style.normalize();
            self.elements.push(Element::Path(path));
        }
    }

    /// Begin a path at `(x, y)`. The [`PathScope`] commits it to
    /// [`Self::elements`] on drop, or discards it if no segment follows its
    /// moves.
    pub fn path(&mut self, style: PathStyle, x: f32, y: f32) -> PathScope<'_> {
        PathScope {
            scene: self,
            builder: PathBuilder::new(style, x, y),
        }
    }

    /// Clip what `inside` draws to a [`ClipPath`] or to a [`RotatedRect`].
    /// `inside` draws into an empty scene of this size, and its elements go
    /// into an [`Element::Clipped`] here. Returns what `inside` returns, so a
    /// fallible walk passes its `Result` out.
    pub fn clip<T>(
        &mut self,
        clip: impl Into<ClipPath>,
        inside: impl FnOnce(&mut Scene) -> T,
    ) -> T {
        let clip = clip.into();
        let mut clipped = self.inner();
        let result = inside(&mut clipped);
        if clip.is_finite() && clipped.depth <= MAX_NESTING {
            self.elements.push(Element::Clipped {
                clip,
                elements: clipped.elements,
            });
        }
        result
    }

    /// Draw what `inside` draws with `opacity`, as one layer, so two
    /// elements inside that overlap do not darken where they meet. `inside`
    /// draws into an empty scene of this size. An opacity of 1 or more adds
    /// the elements with no layer, and one of 0 or less, or not finite, drops
    /// them. Returns what `inside` returns.
    pub fn layer<T>(&mut self, opacity: f32, inside: impl FnOnce(&mut Scene) -> T) -> T {
        let mut layer = self.inner();
        let result = inside(&mut layer);
        if layer.depth > MAX_NESTING {
            return result;
        }
        match LayerOpacity::of(opacity) {
            LayerOpacity::Hidden => {}
            LayerOpacity::Opaque => self.elements.append(&mut layer.elements),
            LayerOpacity::Translucent(opacity) => self.elements.push(Element::Layer {
                opacity,
                elements: layer.elements,
            }),
        }
        result
    }

    /// An empty scene of this size, for the elements of a clip or a layer
    /// here.
    fn inner(&self) -> Scene {
        Scene {
            depth: self.depth + 1,
            ..Scene::new(self.width, self.height)
        }
    }

    pub fn add_text(&mut self, text: Text) {
        if text.is_finite() {
            self.elements.push(Element::Text(text));
        }
    }

    pub fn add_bitmap(&mut self, bitmap: Bitmap) {
        if bitmap.is_finite() {
            self.elements.push(Element::Bitmap(bitmap));
        }
    }
}

/// A width or a height of a frame. A size that describes no frame, which is
/// one that is not finite or is not above zero, is 0, the empty frame.
fn frame_size(size: f32) -> f32 {
    if size.is_finite() && size > 0.0 {
        size
    } else {
        0.0
    }
}

/// The path under construction by [`Scene::path`]. The geometry methods take
/// `&mut self`, so a loop can build a path, and drop commits it to the
/// scene, or discards it if no segment follows its moves.
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
        let segs = self.builder.geom.finish();
        if !segs.is_empty() {
            self.scene.add_path(Path {
                style: std::mem::take(&mut self.builder.style),
                segs,
            });
        }
    }
}

/// Builds a [`Path`] by value.
#[must_use = "PathBuilder yields a Path only when build() is called"]
pub struct PathBuilder {
    style: PathStyle,
    geom: GeometryBuilder,
}

impl PathBuilder {
    fn new(style: PathStyle, x: f32, y: f32) -> Self {
        Self {
            style,
            geom: GeometryBuilder::new(x, y),
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

    /// Append an SVG endpoint arc as cubics. A degenerate arc becomes a line.
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

    pub fn build(mut self) -> Path {
        Path {
            style: self.style,
            segs: self.geom.finish(),
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
    fn new(fill_rule: FillRule, x: f32, y: f32) -> Self {
        Self {
            geom: GeometryBuilder::new(x, y),
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

    /// Append an SVG endpoint arc as cubics. A degenerate arc becomes a line.
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

    pub fn build(mut self) -> ClipPath {
        ClipPath {
            segs: self.geom.finish(),
            fill_rule: self.fill_rule,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_append_one_segment_per_call() {
        let clip = ClipPath::builder(FillRule::EvenOdd, 0.0, 0.0)
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
        let clip = ClipPath::builder(FillRule::NonZero, 1.0, 2.0)
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
        let clip = ClipPath::builder(FillRule::NonZero, 0.0, 0.0)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 0.0)
            .build();
        // The tolerance decides how many cubics the arc becomes.
        let kinds: Vec<_> = clip.segments().map(|s| s.kind()).collect();
        assert_eq!(kinds[0], SegmentKind::Move);
        assert!(kinds.len() > 1);
        assert!(kinds[1..].iter().all(|&k| k == SegmentKind::Cubic));
    }

    #[test]
    fn standalone_path_builder_builds_the_segments_it_was_given() {
        let p = Path::builder(PathStyle::default(), 0.0, 0.0)
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
    fn an_arc_of_a_huge_radius_takes_a_bounded_number_of_cubics() {
        // Nearly a whole turn, since the two ends are close on the circle.
        let arc = |r: f32| {
            Path::builder(PathStyle::default(), 0.0, 0.0)
                .arc_to(r, r, 0.0, true, true, r * 1e-3, 0.0)
                .build()
                .segments()
                .len()
                - 1
        };
        assert!(arc(1e38) <= ARC_CUBICS_PER_TURN as usize, "{}", arc(1e38));
        // A radius a renderer draws keeps the fixed tolerance.
        assert_eq!(arc_tolerance(2.7e8), ARC_TOLERANCE);
    }

    #[test]
    fn scene_path_builder_commits_the_expanded_arc() {
        let mut scene = Scene::new(10.0, 10.0);
        scene
            .path(PathStyle::default(), 0.0, 0.0)
            .arc_to(5.0, 5.0, 0.0, false, true, 10.0, 0.0);
        let Element::Path(p) = &scene.elements[0] else {
            panic!("expected a path");
        };
        assert!(p.segments().any(|s| s.kind() == SegmentKind::Cubic));
    }

    #[test]
    fn add_path_appends_prebuilt_path() {
        let mut scene = Scene::new(10.0, 10.0);
        let p = Path::builder(PathStyle::default(), 0.0, 0.0)
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
    fn a_move_replaces_a_move_before_it_and_a_last_move_is_dropped() {
        let p = Path::builder(PathStyle::default(), 1.0, 1.0)
            .move_to(2.0, 2.0)
            .line_to(3.0, 3.0)
            .move_to(4.0, 4.0)
            .build();
        let segs: Vec<_> = p.segments().collect();
        assert_eq!(
            segs,
            [
                Segment::Move { x: 2.0, y: 2.0 },
                Segment::Line { x: 3.0, y: 3.0 }
            ]
        );
    }

    #[test]
    fn a_builder_with_only_a_start_point_builds_no_segments() {
        let p = Path::builder(PathStyle::default(), 1.0, 1.0).build();
        assert_eq!(p.segments().len(), 0);
        let clip = ClipPath::builder(FillRule::NonZero, 1.0, 1.0)
            .move_to(2.0, 2.0)
            .build();
        assert_eq!(clip.segments().len(), 0);
    }

    #[test]
    fn a_path_scope_with_only_moves_commits_nothing() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.path(PathStyle::default(), 1.0, 1.0).move_to(2.0, 2.0);
        assert!(scene.elements.is_empty());
    }

    #[test]
    fn an_odd_dash_array_is_repeated_to_an_even_one() {
        let dash = Dash::new(vec![4.0, 2.0, 1.0], 1.5).unwrap();
        assert_eq!(dash.array(), [4.0, 2.0, 1.0, 4.0, 2.0, 1.0]);
        assert_eq!(dash.offset(), 1.5);
    }

    #[test]
    fn a_dash_that_draws_a_solid_stroke_is_none() {
        let inf = f32::INFINITY;
        for array in [
            vec![],
            vec![0.0],
            vec![0.0, 0.0],
            vec![3.0, -1.0],
            vec![3.0, f32::NAN],
            vec![3.0, inf],
            vec![f32::MAX, f32::MAX],
        ] {
            assert_eq!(Dash::new(array.clone(), 0.0), None, "{array:?}");
        }
        assert_eq!(Dash::new(vec![3.0, 2.0], f32::NAN), None);
        assert_eq!(Dash::new(vec![3.0, 2.0], inf), None);
    }

    #[test]
    fn a_dash_of_zero_lengths_with_gaps_is_valid() {
        let dash = Dash::new(vec![0.0, 2.0], -1.0).unwrap();
        assert_eq!(dash.array(), [0.0, 2.0]);
        assert_eq!(dash.offset(), -1.0);
    }

    #[test]
    fn clip_wraps_only_elements_drawn_inside() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.add_path(Path::builder(PathStyle::default(), 0.0, 0.0).build());
        scene.clip(
            ClipPath::builder(FillRule::NonZero, 0.0, 0.0).build(),
            |c| {
                c.add_path(
                    Path::builder(PathStyle::default(), 1.0, 1.0)
                        .line_to(2.0, 2.0)
                        .build(),
                );
            },
        );
        scene.add_path(Path::builder(PathStyle::default(), 2.0, 2.0).build());

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
        assert_eq!(
            segs,
            [
                Segment::Move { x: 1.0, y: 1.0 },
                Segment::Line { x: 2.0, y: 2.0 }
            ]
        );
    }

    #[test]
    fn clip_returns_what_its_closure_returns() {
        let mut scene = Scene::new(20.0, 20.0);
        let failed: Result<(), &str> = scene.clip(a_unit_rect(0.0), |clip| {
            clip.add_path(a_line(PathStyle::default(), 5.0, 5.0));
            Err("stop")
        });
        assert_eq!(failed, Err("stop"));
        // The elements drawn before the error stay in the clip.
        let [Element::Clipped { elements, .. }] = &scene.elements[..] else {
            panic!("expected one clip, got {:?}", scene.elements);
        };
        assert_eq!(elements.len(), 1);
    }

    #[test]
    fn a_layer_holds_what_its_closure_draws_only_when_it_is_translucent() {
        let mut scene = Scene::new(20.0, 20.0);
        for opacity in [0.5, 1.0, 2.0, 0.0, -1.0, f32::NAN, f32::INFINITY] {
            let drawn = scene.layer(opacity, |layer| {
                layer.add_path(a_line(PathStyle::default(), 5.0, 5.0));
                opacity
            });
            assert!(drawn.to_bits() == opacity.to_bits());
        }
        // 1 and 2 add the path with no layer, and the rest drop it.
        let [
            Element::Layer { opacity, elements },
            Element::Path(_),
            Element::Path(_),
        ] = &scene.elements[..]
        else {
            panic!("expected a layer and two paths, got {:?}", scene.elements);
        };
        assert_eq!(*opacity, 0.5);
        assert_eq!(elements.len(), 1);
    }

    #[test]
    fn nested_clips_wrap_inside_out() {
        let mut scene = Scene::new(20.0, 20.0);
        scene.clip(
            ClipPath::builder(FillRule::NonZero, 0.0, 0.0).build(),
            |outer| {
                outer.add_path(Path::builder(PathStyle::default(), 1.0, 1.0).build());
                outer.clip(
                    ClipPath::builder(FillRule::NonZero, 0.0, 0.0).build(),
                    |inner| {
                        inner.add_path(Path::builder(PathStyle::default(), 2.0, 2.0).build());
                    },
                );
            },
        );
        // scene = [ Clipped{ outer, [ Path(1,1), Clipped{ inner, [ Path(2,2) ] } ] } ]
        assert_eq!(scene.elements.len(), 1);
        let Element::Clipped { elements, .. } = &scene.elements[0] else {
            panic!("expected outer clip");
        };
        assert_eq!(elements.len(), 2);
        assert!(matches!(elements[0], Element::Path(_)));
        assert!(matches!(elements[1], Element::Clipped { .. }));
    }

    #[test]
    fn fit_puts_the_family_after_fallback_and_the_affine_of_the_rect_in_the_text() {
        let spec = TextSpec {
            size: 20.0,
            family: "ZZZ_nope".into(),
            text: "Hi".into(),
            ..TextSpec::default()
        };
        let rect = RotatedRect {
            cx: 5.0,
            cy: 7.0,
            w: 100.0,
            h: 40.0,
            angle_deg: 30.0,
        };
        let metrics =
            crate::text::measure(&spec.family, spec.weight, spec.style, spec.size, &spec.text)
                .expect("text measures");
        let text = spec.fit(rect).expect("text fits");
        assert_ne!(&*text.spec.family, "ZZZ_nope");
        assert_eq!(&*text.spec.family, metrics.family());
        assert_eq!(
            text.transform,
            rect.affine(metrics.width(), metrics.height())
        );
        assert_eq!((text.spec.size, &*text.spec.text), (20.0, "Hi"));
    }

    #[test]
    fn fit_returns_none_for_a_size_that_cannot_draw() {
        let spec = TextSpec {
            text: "Hi".into(),
            ..TextSpec::default()
        };
        assert!(
            spec.fit(RotatedRect {
                cx: 0.0,
                cy: 0.0,
                w: 10.0,
                h: 10.0,
                angle_deg: 0.0
            })
            .is_none()
        );
    }

    #[test]
    fn two_images_of_the_same_file_are_equal() {
        use std::collections::HashSet;
        let file = crate::asset::png_head(2, 3);
        let a = Image::new(file.clone()).unwrap();
        let b = Image::new(file).unwrap();
        let c = Image::new(crate::asset::png_head(3, 2)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(HashSet::from([a.clone(), b, c]).len(), 2);
        assert_eq!((a.width(), a.height()), (2, 3));
    }

    #[test]
    fn bitmap_fit_puts_the_unit_square_on_the_rect() {
        let rect = RotatedRect {
            cx: 70.0,
            cy: 40.0,
            w: -32.0,
            h: 16.0,
            angle_deg: 90.0,
        };
        let bitmap = Bitmap::fit(crate::asset::png_image(1, 1), rect);
        let m = bitmap.transform;
        assert_eq!(apply_affine(m, 0.0, 0.0), (70.0, 40.0));
        let (x, y) = apply_affine(m, 0.5, 0.5);
        assert!(
            (x - 62.0).abs() < 1e-4 && (y - 24.0).abs() < 1e-4,
            "{x}, {y}"
        );
    }

    fn a_line(style: PathStyle, x: f32, y: f32) -> Path {
        Path::builder(style, 0.0, 0.0).line_to(x, y).build()
    }

    fn a_unit_rect(angle_deg: f32) -> RotatedRect {
        RotatedRect {
            cx: 5.0,
            cy: 5.0,
            w: 10.0,
            h: 10.0,
            angle_deg,
        }
    }

    #[test]
    fn an_element_with_a_non_finite_float_is_not_added() {
        let (nan, inf) = (f32::NAN, f32::INFINITY);
        let stop = |offset, a| Stop {
            offset,
            color: Rgba {
                a,
                ..Rgba::default()
            },
        };
        let styles = [
            PathStyle {
                stroke_width: inf,
                ..PathStyle::default()
            },
            PathStyle {
                miter_limit: nan,
                ..PathStyle::default()
            },
            PathStyle {
                fill: Paint::rgba(0, 0, 0, nan),
                ..PathStyle::default()
            },
            PathStyle {
                stroke: Paint::radial(0.0, 0.0, inf, vec![stop(0.0, 1.0)]),
                ..PathStyle::default()
            },
            PathStyle {
                fill: Paint::linear(0.0, 0.0, 1.0, 1.0, vec![stop(nan, 1.0)]),
                ..PathStyle::default()
            },
            PathStyle {
                fill: Paint::linear(0.0, 0.0, 1.0, 1.0, vec![stop(0.0, inf)]),
                ..PathStyle::default()
            },
        ];
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(a_line(PathStyle::default(), nan, 5.0));
        scene
            .path(PathStyle::default(), 0.0, 0.0)
            .cubic_to(1.0, 2.0, 3.0, 4.0, 5.0, -inf);
        for style in styles {
            scene.path(style.clone(), 0.0, 0.0).line_to(5.0, 5.0);
            scene.add_path(a_line(style, 5.0, 5.0));
        }
        let text = TextSpec {
            size: 12.0,
            text: "Hi".into(),
            ..TextSpec::default()
        }
        .fit(a_unit_rect(0.0))
        .expect("text fits");
        scene.add_text(Text {
            spec: TextSpec {
                size: inf,
                ..text.spec.clone()
            },
            ..text.clone()
        });
        scene.add_text(Text {
            stroke_width: nan,
            ..text.clone()
        });
        scene.add_text(Text {
            fill: Paint::rgba(0, 0, 0, nan),
            ..text.clone()
        });
        scene.add_text(Text {
            transform: [1.0, 0.0, 0.0, 1.0, inf, 0.0],
            ..text
        });
        scene.add_bitmap(Bitmap::fit(crate::asset::png_image(1, 1), a_unit_rect(nan)));
        assert!(scene.elements.is_empty(), "{:?}", scene.elements);
    }

    #[test]
    fn an_arc_with_a_non_finite_float_drops_its_element() {
        let (nan, inf) = (f32::NAN, f32::INFINITY);
        let mut scene = Scene::new(10.0, 10.0);
        for (rx, rotation_deg, x) in [
            (nan, 0.0, 5.0),
            (inf, 0.0, 5.0),
            (5.0, inf, 5.0),
            (5.0, 0.0, nan),
        ] {
            scene
                .path(PathStyle::default(), 0.0, 0.0)
                .arc_to(rx, 5.0, rotation_deg, false, true, x, 0.0)
                .line_to(0.0, 9.0);
            scene.add_path(
                Path::builder(PathStyle::default(), 0.0, 0.0)
                    .arc_to(rx, 5.0, rotation_deg, false, true, x, 0.0)
                    .build(),
            );
            let clip = ClipPath::builder(FillRule::NonZero, 0.0, 0.0)
                .arc_to(rx, 5.0, rotation_deg, false, true, x, 0.0)
                .build();
            scene.clip(clip, |c| c.add_path(a_line(PathStyle::default(), 5.0, 5.0)));
        }
        assert!(scene.elements.is_empty(), "{:?}", scene.elements);
    }

    #[test]
    fn a_clip_with_a_non_finite_float_drops_what_it_holds() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.clip(a_unit_rect(f32::NAN), |clip| {
            clip.add_path(a_line(PathStyle::default(), 5.0, 5.0));
        });
        assert!(scene.elements.is_empty(), "{:?}", scene.elements);
    }

    #[test]
    fn a_clip_or_a_layer_past_max_nesting_drops_what_it_holds() {
        fn nest(scene: &mut Scene, depth: usize, layer: bool) {
            if depth == 0 {
                scene.add_path(a_line(PathStyle::default(), 5.0, 5.0));
            } else if layer {
                scene.layer(0.5, |s| nest(s, depth - 1, layer));
            } else {
                scene.clip(a_unit_rect(0.0), |s| nest(s, depth - 1, layer));
            }
        }
        /// How many clips or layers hold the innermost element, if any.
        fn depth(elements: &[Element]) -> Option<usize> {
            match elements {
                [Element::Clipped { elements, .. } | Element::Layer { elements, .. }] => {
                    Some(depth(elements)? + 1)
                }
                [Element::Path(_)] => Some(0),
                _ => None,
            }
        }
        for layer in [false, true] {
            let mut scene = Scene::new(10.0, 10.0);
            nest(&mut scene, MAX_NESTING, layer);
            assert_eq!(depth(&scene.elements), Some(MAX_NESTING));
            let mut scene = Scene::new(10.0, 10.0);
            nest(&mut scene, MAX_NESTING + 1, layer);
            assert_eq!(depth(&scene.elements), None);
        }
    }

    #[test]
    fn a_non_finite_element_inside_a_clip_drops_alone() {
        let mut scene = Scene::new(10.0, 10.0);
        scene.clip(a_unit_rect(0.0), |clip| {
            clip.add_path(a_line(PathStyle::default(), 5.0, 5.0));
            clip.add_path(a_line(PathStyle::default(), f32::NAN, 5.0));
        });
        let [Element::Clipped { elements, .. }] = &scene.elements[..] else {
            panic!("expected one clip, got {:?}", scene.elements);
        };
        assert_eq!(elements.len(), 1);
    }

    #[test]
    fn a_gradient_with_no_extent_paints_its_last_stop() {
        let stop = |offset, r| Stop {
            offset,
            color: Rgba {
                r,
                g: 0,
                b: 0,
                a: 1.0,
            },
        };
        let stops = vec![stop(0.0, 10), stop(1.0, 200)];
        let last = Paint::Solid(stop(1.0, 200).color);
        for geom in [
            GradientGeom::Radial {
                cx: 5.0,
                cy: 5.0,
                radius: 0.0,
            },
            GradientGeom::Radial {
                cx: 5.0,
                cy: 5.0,
                radius: -3.0,
            },
            GradientGeom::Linear {
                x0: 5.0,
                y0: 5.0,
                x1: 5.0,
                y1: 5.0,
            },
            // Below the threshold of a rasterizer, so it has no extent too.
            GradientGeom::Radial {
                cx: 5.0,
                cy: 5.0,
                radius: 1e-6,
            },
            GradientGeom::Linear {
                x0: 5.0,
                y0: 5.0,
                x1: 5.000001,
                y1: 5.0,
            },
        ] {
            let g = Gradient {
                geom,
                stops: stops.clone(),
                spread: SpreadMode::Repeat,
            };
            assert_eq!(Paint::gradient(g), last, "{geom:?}");
        }
        assert!(matches!(
            Paint::radial(5.0, 5.0, 1.0, stops),
            Paint::Gradient(_)
        ));
    }

    #[test]
    fn a_gradient_axis_too_long_to_measure_shrinks_about_the_origin() {
        let stops = vec![Stop::default(), Stop::default()];
        let axis = |p: Paint| match p {
            Paint::Gradient(g) => match g.geom() {
                GradientGeom::Linear { x0, y0, x1, y1 } => (x0, y0, x1, y1),
                radial => panic!("expected a line, got {radial:?}"),
            },
            solid => panic!("expected a gradient, got {solid:?}"),
        };
        // A length a rasterizer can measure passes through as it is.
        let short = (-1e38, 0.0, 1e38, 0.0);
        assert_eq!(
            axis(Paint::linear(
                short.0,
                short.1,
                short.2,
                short.3,
                stops.clone()
            )),
            short
        );
        let (x0, y0, x1, y1) = axis(Paint::linear(-3e38, 0.0, 3e38, 0.0, stops));
        assert!((x1 - x0).hypot(y1 - y0).is_finite(), "{x0} to {x1}");
        // The ends keep their direction from the origin, so the color under a
        // path near it stays where it was.
        assert_eq!((y0, y1, x0 / x1), (0.0, 0.0, -1.0));
    }

    #[test]
    fn a_stop_before_the_one_before_it_moves_up_to_it() {
        let stop = |offset| Stop {
            offset,
            color: Rgba::default(),
        };
        let offsets = |p: Paint| match p {
            Paint::Gradient(g) => g.stops().iter().map(|s| s.offset).collect::<Vec<_>>(),
            solid => panic!("expected a gradient, got {solid:?}"),
        };
        let out_of_order = vec![stop(0.8), stop(0.2), stop(1.0)];
        assert_eq!(
            offsets(Paint::radial(0.0, 0.0, 5.0, out_of_order.clone())),
            [0.8, 0.8, 1.0]
        );
        assert_eq!(
            offsets(Paint::linear(0.0, 0.0, 5.0, 5.0, out_of_order)),
            [0.8, 0.8, 1.0]
        );
        let out_of_range = vec![stop(-0.5), stop(0.5), stop(2.0)];
        assert_eq!(
            offsets(Paint::linear(0.0, 0.0, 5.0, 5.0, out_of_range)),
            [0.0, 0.5, 1.0]
        );
    }

    #[test]
    fn a_miter_limit_below_1_rises_to_1() {
        let style = |miter_limit| PathStyle {
            miter_limit,
            ..PathStyle::default()
        };
        let mut scene = Scene::new(10.0, 10.0);
        for limit in [0.25, -2.0, 8.0] {
            scene.add_path(a_line(style(limit), 5.0, 5.0));
        }
        let limits: Vec<f32> = scene
            .elements()
            .iter()
            .map(|e| match e {
                Element::Path(p) => p.style.miter_limit,
                other => panic!("expected a Path, got {other:?}"),
            })
            .collect();
        assert_eq!(limits, [1.0, 1.0, 8.0]);
    }

    #[test]
    fn a_size_that_describes_no_frame_is_zero() {
        for (width, height) in [
            (-5.0, 10.0),
            (10.0, f32::NEG_INFINITY),
            (f32::NAN, -0.0),
            (f32::INFINITY, 10.0),
        ] {
            let scene = Scene::new(width, height);
            assert!(
                scene.width() >= 0.0 && scene.height() >= 0.0,
                "{width} by {height} gave {} by {}",
                scene.width(),
                scene.height()
            );
        }
        let scene = Scene::new(0.5, 10.0);
        assert_eq!((scene.width(), scene.height()), (0.5, 10.0));
    }
}
