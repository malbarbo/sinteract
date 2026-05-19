//! Value types shared by the [`crate::sink::DrawSink`] trait and its
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
#[derive(Clone, Debug, Default)]
pub struct ClipPath {
    pub verbs: Vec<u8>,
    pub coords: Vec<f32>,
    pub fill_rule: FillRule,
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
///
/// Native-only: WASM frontends measure text via `OffscreenCanvas` (see
/// `text.rs` module docs) and build the affine on the JS side.
#[cfg(not(target_arch = "wasm32"))]
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
pub struct BitmapNode {
    pub id: u32,
    pub transform: [f32; 6],
}

impl Default for BitmapNode {
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
    // Centre the asset on (cx, cy): pre-translate by (-img_w/2, -img_h/2)
    // before the scale/rotate so the asset's centre lands on (cx, cy)
    // after the rest of the transform.
    let sx = w / img_w as f32;
    let sy = h / img_h as f32;
    let theta = angle_deg * std::f32::consts::PI / 180.0;
    let ct = theta.cos();
    let st = theta.sin();
    // M = T(cx,cy) · R(theta) · S(sx,sy) · T(-iw/2, -ih/2)
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
/// it via [`DrawList::begin_path`] and the [`PathBuilder`] returned, and the
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
    ClipPush(ClipPath),
    ClipPop,
    Text(Box<TextNode>),
    Bitmap(BitmapNode),
}

/// Materialized event log produced by Python (or any other front end) and
/// consumed by every renderer. Built via RAII guards — [`Self::begin_path`]
/// returns a [`PathBuilder`] that commits the path on drop, and
/// [`Self::push_clip`] / [`Self::push_clip_rect`] return a [`ClipGuard`] that
/// emits the matching `ClipPop` on drop. The list is replayed once via
/// [`Self::play_into`].
///
/// Because path geometry only flows through `PathBuilder` and clip nesting is
/// scoped by `ClipGuard`, several footguns of the older flat API are
/// statically impossible: you cannot append path verbs without an open path,
/// commit an empty path, unbalance the clip stack, or interleave a clip with
/// a half-built path.
///
/// Arcs entered via [`PathBuilder::arc_to`] are pre-expanded to cubics so
/// renderers only see line / quad / cubic primitives — same surface as the
/// text-format parser in [`crate::parse`].
#[derive(Clone, Debug, Default)]
pub struct DrawList {
    pub width: f32,
    pub height: f32,
    pub nodes: Vec<DrawNode>,
}

/// Tolerance for SVG arc → cubic conversion. Matches [`crate::parse`].
const ARC_TOLERANCE: f64 = 0.1;

impl DrawList {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            nodes: Vec::new(),
        }
    }

    /// Begin a new path. Returns a [`PathBuilder`] whose `move_to` / `line_to`
    /// / `quad_to` / `cubic_to` / `arc_to` methods append verbs; the path is
    /// committed to [`Self::nodes`] on drop, or discarded if no geometry was
    /// recorded.
    pub fn begin_path(&mut self, style: PathStyle) -> PathBuilder<'_> {
        PathBuilder {
            dl: self,
            style,
            verbs: Vec::new(),
            coords: Vec::new(),
            last_point: None,
        }
    }

    /// Push an arbitrary clip path. Returns a [`ClipGuard`] that emits the
    /// matching `ClipPop` on drop; nested clips just call [`Self::push_clip`]
    /// through the guard's `Deref` and pop in the right order.
    pub fn push_clip(&mut self, clip: ClipPath) -> ClipGuard<'_> {
        self.nodes.push(DrawNode::ClipPush(clip));
        ClipGuard { dl: self }
    }

    /// Push an axis-aligned-or-rotated rectangular clip — the common case.
    /// Builds the 4-corner `ClipPath` (rotated by `angle_deg` around the
    /// centre) and returns the guard.
    pub fn push_clip_rect(
        &mut self,
        cx: f32,
        cy: f32,
        w: f32,
        h: f32,
        angle_deg: f32,
        fill_rule: FillRule,
    ) -> ClipGuard<'_> {
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
        self.push_clip(ClipPath {
            verbs: vec![verb::MOVE, verb::LINE, verb::LINE, verb::LINE],
            coords: vec![p0.0, p0.1, p1.0, p1.1, p2.0, p2.1, p3.0, p3.1],
            fill_rule,
        })
    }

    pub fn text(&mut self, node: TextNode) {
        self.nodes.push(DrawNode::Text(Box::new(node)));
    }

    pub fn bitmap(&mut self, node: BitmapNode) {
        self.nodes.push(DrawNode::Bitmap(node));
    }

    /// Append a fully-built path. Used by the wire decoder; lets us bypass
    /// the `begin_path / move_to / …` guard for paths whose verbs/coords
    /// were already validated.
    pub(crate) fn push_path(&mut self, path: Path) {
        self.nodes.push(DrawNode::Path(path));
    }

    /// Replay every node into `sink`. Wraps `sink.begin()` and `sink.end()`
    /// around the dispatch loop so callers don't have to.
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
        sink.end();
    }
}

/// Path geometry accumulator returned by [`DrawList::begin_path`]. Holds the
/// style and the in-flight verb/coord buffers; on drop, commits a
/// [`DrawNode::Path`] to the parent [`DrawList`] (or discards if no geometry
/// was recorded).
#[must_use = "PathBuilder commits the path on drop; bind it so geometry methods can run"]
pub struct PathBuilder<'a> {
    dl: &'a mut DrawList,
    style: PathStyle,
    verbs: Vec<u8>,
    coords: Vec<f32>,
    last_point: Option<(f32, f32)>,
}

impl<'a> PathBuilder<'a> {
    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.verbs.push(verb::MOVE);
        self.coords.extend([x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.verbs.push(verb::LINE);
        self.coords.extend([x, y]);
        self.last_point = Some((x, y));
        self
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) -> &mut Self {
        self.verbs.push(verb::QUAD);
        self.coords.extend([cx, cy, x, y]);
        self.last_point = Some((x, y));
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
        self.verbs.push(verb::CUBIC);
        self.coords.extend([c1x, c1y, c2x, c2y, x, y]);
        self.last_point = Some((x, y));
        self
    }

    /// Append an SVG endpoint arc, pre-expanding to cubic segments. Mirrors
    /// the text parser's `A` handling: with no current point, falls back to
    /// `move_to(x, y)`; degenerate arcs collapse to a line.
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
        match kurbo::Arc::from_svg_arc(&svg_arc) {
            Some(arc) => {
                for el in arc.append_iter(ARC_TOLERANCE) {
                    if let kurbo::PathEl::CurveTo(p1, p2, p3) = el {
                        self.verbs.push(verb::CUBIC);
                        self.coords.extend([
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
                self
            }
            None => self.line_to(x, y),
        }
    }
}

impl<'a> Drop for PathBuilder<'a> {
    fn drop(&mut self) {
        if self.verbs.is_empty() {
            return;
        }
        self.dl.nodes.push(DrawNode::Path(Path {
            style: std::mem::take(&mut self.style),
            verbs: std::mem::take(&mut self.verbs),
            coords: std::mem::take(&mut self.coords),
        }));
    }
}

/// Active clip scope returned by [`DrawList::push_clip`] /
/// [`DrawList::push_clip_rect`]. `Deref`s to the parent [`DrawList`] so all
/// draw methods remain reachable through the guard; on drop, emits the
/// matching `ClipPop`. Nested clips work because each `push_clip` call on the
/// guard creates a fresh `ClipGuard` whose lifetime is contained within the
/// outer one.
#[must_use = "ClipGuard pops the clip on drop; bind it where the clip should end"]
pub struct ClipGuard<'a> {
    dl: &'a mut DrawList,
}

impl<'a> std::ops::Deref for ClipGuard<'a> {
    type Target = DrawList;
    fn deref(&self) -> &DrawList {
        self.dl
    }
}

impl<'a> std::ops::DerefMut for ClipGuard<'a> {
    fn deref_mut(&mut self) -> &mut DrawList {
        self.dl
    }
}

impl<'a> Drop for ClipGuard<'a> {
    fn drop(&mut self) {
        self.dl.nodes.push(DrawNode::ClipPop);
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
