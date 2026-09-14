//! Text measurement and glyph outlines.
//!
//! The crate embeds Liberation Sans, Serif and Mono in Regular, Bold, Italic
//! and BoldItalic. An alias such as `sans-serif`, `serif`, `monospace` or
//! `mono` maps to an embedded family. Any other name goes to a `fontdb`
//! query over the fonts installed on the system and falls back to Liberation
//! Sans, so a text in an embedded family measures the same on every target.
//!
//! The `native-fonts` feature carries that query. It is on by default, and
//! wasm32 leaves it out even so, since a browser has no font directory.
//!
//! A size counts the units of the caller to the em. It is not a device
//! pixel. A [`crate::scene::TextNode`] measures in text space, and its
//! `transform` maps that space to the canvas.
//!
//! A measurement is an offset from the center of the text box. The text
//! spans (-width/2, -height/2) to (width/2, height/2), and the caller places
//! it with `translate(cx, cy) * rotate(angle) * scale(sx, sy)`.

#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
use std::sync::Mutex;
use std::sync::OnceLock;

use ttf_parser::{Face, GlyphId};

use crate::scene::{FontStyle, TextNode};

// ---------------------------------------------------------------------------
// Embedded fonts
// ---------------------------------------------------------------------------

/// An embedded TTF, parsed once per process.
struct EmbeddedFont {
    name: &'static str,
    bytes: &'static [u8],
    face: OnceLock<Face<'static>>,
}

impl EmbeddedFont {
    fn face(&self) -> &Face<'static> {
        self.face
            .get_or_init(|| Face::parse(self.bytes, 0).expect("embedded font is valid"))
    }
}

/// The four variants of a family in `fonts/`, in the order of
/// [`variant_index`].
macro_rules! embed_family {
    (@variant $name:literal, $file:literal, $variant:literal) => {
        EmbeddedFont {
            name: $name,
            bytes: include_bytes!(concat!("../fonts/", $file, "-", $variant, ".ttf")),
            face: OnceLock::new(),
        }
    };
    ($name:literal, $file:literal) => {
        [
            embed_family!(@variant $name, $file, "Regular"),
            embed_family!(@variant $name, $file, "Bold"),
            embed_family!(@variant $name, $file, "Italic"),
            embed_family!(@variant $name, $file, "BoldItalic"),
        ]
    };
}

static SANS: [EmbeddedFont; 4] = embed_family!("Liberation Sans", "LiberationSans");
static SERIF: [EmbeddedFont; 4] = embed_family!("Liberation Serif", "LiberationSerif");
static MONO: [EmbeddedFont; 4] = embed_family!("Liberation Mono", "LiberationMono");

/// A CSS weight at or above this picks the bold face.
const BOLD_THRESHOLD: u16 = 600;

fn variant_index(weight: u16, style: FontStyle) -> usize {
    let bold = weight >= BOLD_THRESHOLD;
    let italic = !matches!(style, FontStyle::Normal);
    match (bold, italic) {
        (false, false) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (true, true) => 3,
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The result of resolving a request. A host sends `family` on the wire, so a
/// client measures with the same face as the server.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedFont {
    /// `"Liberation Sans"`, `"Liberation Serif"`, `"Liberation Mono"`, or
    /// the name fontdb reports.
    pub family: &'static str,
    /// `true` if the face came from a system lookup, `false` otherwise.
    pub from_system: bool,
    face: &'static Face<'static>,
}

impl ResolvedFont {
    pub fn face(&self) -> &'static Face<'static> {
        self.face
    }
}

/// Resolve a family, a weight and a style to a face.
///
/// The name loses its surrounding space first. An empty family is Liberation
/// Sans. An alias (`sans-serif`, `serif`,
/// `monospace`, `mono`, or an embedded family name, in any case) is the
/// embedded family. Any other name goes to a `fontdb` query, and to
/// Liberation Sans when the query finds nothing or when the crate carries no
/// system lookup.
pub fn resolve(family: &str, weight: u16, style: FontStyle) -> ResolvedFont {
    let v = variant_index(weight, style);

    // This runs once per text node per frame, so the comparison allocates
    // nothing.
    let key = family.trim();
    let is = |names: &[&str]| names.iter().any(|n| key.eq_ignore_ascii_case(n));
    let alias = if is(&["", "sans-serif", "sans", "liberation sans"]) {
        Some(&SANS)
    } else if is(&["serif", "liberation serif"]) {
        Some(&SERIF)
    } else if is(&["monospace", "mono", "liberation mono"]) {
        Some(&MONO)
    } else {
        None
    };
    if let Some(family_arr) = alias {
        return embedded(&family_arr[v]);
    }

    #[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
    if let Some(font) = system_font(key, weight, style) {
        return font;
    }

    embedded(&SANS[v])
}

fn embedded(f: &'static EmbeddedFont) -> ResolvedFont {
    ResolvedFont {
        family: f.name,
        from_system: false,
        face: f.face(),
    }
}

// ---------------------------------------------------------------------------
// System font lookup. The native-fonts feature carries it, and wasm32 drops
// it in any case, since there is no font directory to read.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
fn font_db() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// The system lookups already done. `requests` keeps the answer to each
/// family, weight and style, a miss included, so a node that names a system
/// family does not scan fontdb every frame. `faces` keeps each parsed face by
/// fontdb id, leaked so it has the `'static` lifetime of an embedded face. A
/// process touches few distinct fonts, so the leak is bounded.
#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
#[derive(Default)]
struct SystemCache {
    requests: Vec<(FontRequest, Option<ResolvedFont>)>,
    faces: Vec<(fontdb::ID, ResolvedFont)>,
}

/// A family, a weight and a style, as `resolve` receives them.
#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
type FontRequest = (Box<str>, u16, FontStyle);

#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
fn system_cache() -> &'static Mutex<SystemCache> {
    static CACHE: OnceLock<Mutex<SystemCache>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
fn system_font(family: &str, weight: u16, style: FontStyle) -> Option<ResolvedFont> {
    // One lock for the lookup and for the insert, so two threads that ask for
    // the same family do not both parse a face and leak it.
    let mut cache = system_cache().lock().ok()?;
    let answered = cache
        .requests
        .iter()
        .find(|((f, w, s), _)| &**f == family && *w == weight && *s == style);
    if let Some((_, font)) = answered {
        return *font;
    }
    let font = query_system_font(&mut cache.faces, family, weight, style);
    cache.requests.push(((family.into(), weight, style), font));
    font
}

#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
fn query_system_font(
    faces: &mut Vec<(fontdb::ID, ResolvedFont)>,
    family: &str,
    weight: u16,
    style: FontStyle,
) -> Option<ResolvedFont> {
    let db = font_db();
    let style_db = match style {
        FontStyle::Normal => fontdb::Style::Normal,
        FontStyle::Italic => fontdb::Style::Italic,
        FontStyle::Oblique => fontdb::Style::Oblique,
    };
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight(weight),
        stretch: fontdb::Stretch::Normal,
        style: style_db,
    };
    let id = db.query(&query)?;
    if let Some((_, f)) = faces.iter().find(|(c, _)| *c == id) {
        return Some(*f);
    }

    let face_data = db.with_face_data(id, |bytes, index| -> Option<ResolvedFont> {
        // The face borrows the bytes, so both are leaked.
        let static_bytes: &'static [u8] = bytes.to_vec().leak();
        let face = Face::parse(static_bytes, index).ok()?;
        let face_static: &'static Face<'static> = Box::leak(Box::new(face));
        // The name fontdb reports, so a client resolves the same face.
        let canonical = db
            .face(id)
            .and_then(|info| info.families.first())
            .map_or(family, |(n, _)| n.as_str());
        Some(ResolvedFont {
            family: canonical.to_owned().leak(),
            from_system: true,
            face: face_static,
        })
    })??;

    faces.push((id, face_data));
    Some(face_data)
}

// ---------------------------------------------------------------------------
// Measurement and outlines
// ---------------------------------------------------------------------------

/// Receives the outline of a glyph. Coordinates are box-local with y down,
/// as in the `measure_*` functions.
pub trait OutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32);
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32);
    fn close(&mut self);
}

/// Turns every quadratic into a cubic for an [`OutlineBuilder`] that has no
/// quadratic operator. It tracks the current point itself, and `close`
/// returns the point to the start of the subpath, so the backend does not
/// reconstruct it.
pub struct ElevateQuads<'a, B: ?Sized> {
    inner: &'a mut B,
    start: Option<(f32, f32)>,
    last: Option<(f32, f32)>,
}

impl<'a, B: OutlineBuilder + ?Sized> ElevateQuads<'a, B> {
    pub fn new(inner: &'a mut B) -> Self {
        Self {
            inner,
            start: None,
            last: None,
        }
    }
}

impl<B: OutlineBuilder + ?Sized> OutlineBuilder for ElevateQuads<'_, B> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = Some((x, y));
        self.last = Some((x, y));
        self.inner.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.last = Some((x, y));
        self.inner.line_to(x, y);
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        // A contour that opens on a quadratic has no start point, so the
        // quadratic is dropped.
        let Some(p0) = self.last else { return };
        let (c1x, c1y, c2x, c2y) = crate::scene::quad_to_cubic(p0, cx, cy, x, y);
        self.last = Some((x, y));
        self.inner.cubic_to(c1x, c1y, c2x, c2y, x, y);
    }

    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.last = Some((x, y));
        self.inner.cubic_to(cx1, cy1, cx2, cy2, x, y);
    }

    fn close(&mut self) {
        self.last = self.start;
        self.inner.close();
    }
}

/// Returns `true` if a text at `size` can draw, `false` otherwise. NaN and
/// infinity are out, since either one puts non-finite coordinates in the
/// outline.
pub(crate) fn drawable_size(size: f32) -> bool {
    size.is_finite() && size > 0.0
}

/// The width of a tab, in spaces of the face. A tab advances by the same
/// width at any column, as the `text` of Racket's `2htdp/image` does.
const TAB_SPACES: usize = 8;

/// Each glyph that `text` draws, with its advance in font units. A tab draws
/// [`TAB_SPACES`] spaces, another control character draws nothing, and a
/// character that the face lacks draws the `.notdef` box.
fn glyphs<'a>(face: &'a Face<'_>, text: &'a str) -> impl Iterator<Item = (GlyphId, f64)> + 'a {
    text.chars()
        .filter_map(|c| match c {
            '\t' => Some((' ', TAB_SPACES)),
            c if c.is_control() => None,
            c => Some((c, 1)),
        })
        .flat_map(move |(c, n)| {
            let gid = face.glyph_index(c).unwrap_or(GlyphId(0));
            let advance = f64::from(face.glyph_hor_advance(gid).unwrap_or(0));
            std::iter::repeat_n((gid, advance), n)
        })
}

/// The text-space length of one font unit of `face` at `size`.
fn em_scale(face: &Face<'_>, size: f32) -> f64 {
    f64::from(size) / f64::from(face.units_per_em())
}

/// Total horizontal advance of `text` rendered at `size` in `face`.
pub fn measure_width_with(face: &Face<'_>, text: &str, size: f32) -> f64 {
    if text.is_empty() || !drawable_size(size) {
        return 0.0;
    }
    let total: f64 = glyphs(face, text).map(|(_, advance)| advance).sum();
    total * em_scale(face, size)
}

pub fn measure_height_with(face: &Face<'_>, size: f32) -> f64 {
    if !drawable_size(size) {
        return 0.0;
    }
    let h = f64::from(face.ascender()) - f64::from(face.descender());
    h * em_scale(face, size)
}

pub fn measure_y_offset_with(face: &Face<'_>, size: f32) -> f64 {
    if !drawable_size(size) {
        return 0.0;
    }
    (f64::from(face.ascender()) + f64::from(face.descender())) / 2.0 * em_scale(face, size)
}

// ---------------------------------------------------------------------------
// Layout of a text node, shared by the renderers
// ---------------------------------------------------------------------------

/// The face and the box-local metrics of one [`TextNode`].
pub struct TextLayout {
    pub face: &'static Face<'static>,
    /// The em of the text space, from `TextNode::size`.
    pub size: f32,
    /// The horizontal advance.
    pub width: f32,
    /// The ascender minus the descender of the face.
    pub height: f32,
    /// The baseline, box-local with y down, as in the `measure_*` functions.
    pub baseline_y: f32,
}

impl TextLayout {
    /// The left edge of the text, box-local.
    pub fn x_left(&self) -> f32 {
        -self.width / 2.0
    }
}

/// Resolve and measure a text node. Returns `None` when the node draws
/// nothing, because the size is not a positive finite number, the text is
/// empty, or the width or the height measures zero or overflows.
pub fn layout_text(node: &TextNode) -> Option<TextLayout> {
    layout(&node.family, node.weight, node.style, node.size, &node.text)
}

/// [`layout_text`] for the fields of a node, so a producer that has no node
/// yet measures the text as a renderer will.
pub(crate) fn layout(
    family: &str,
    weight: u16,
    style: FontStyle,
    size: f32,
    text: &str,
) -> Option<TextLayout> {
    if !drawable_size(size) || text.is_empty() {
        return None;
    }
    let face = resolve(family, weight, style).face();
    let width = measure_width_with(face, text, size) as f32;
    let height = measure_height_with(face, size) as f32;
    // A huge size overflows the measurement, and a non-finite width or height
    // would reach the renderer as a coordinate and the producer as a scale.
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some(TextLayout {
        face,
        size,
        width,
        height,
        baseline_y: measure_y_offset_with(face, size) as f32,
    })
}

/// Glyph outlines for a node measured by [`layout_text`].
pub fn outline_layout(layout: &TextLayout, text: &str, out: &mut dyn OutlineBuilder) {
    let face = layout.face;
    let scale = em_scale(face, layout.size);
    let x_left = layout.x_left();
    let mut adapter = OutlineAdapter {
        out,
        scale: scale as f32,
        origin_x: x_left,
        baseline_y: layout.baseline_y,
    };
    let mut pen_x: f64 = 0.0;
    for (gid, advance) in glyphs(face, text) {
        adapter.origin_x = x_left + (pen_x * scale) as f32;
        let _ = face.outline_glyph(gid, &mut adapter);
        pen_x += advance;
    }
}

/// The underline of a laid-out node as a closed contour.
pub fn outline_underline(layout: &TextLayout, out: &mut dyn OutlineBuilder) {
    let u = underline_rect(layout);
    out.move_to(u.x_l, u.y_top);
    out.line_to(u.x_r, u.y_top);
    out.line_to(u.x_r, u.y_bot);
    out.line_to(u.x_l, u.y_bot);
    out.close();
}

/// Axis-aligned underline rectangle in box-local text space.
pub struct UnderlineRect {
    pub x_l: f32,
    pub x_r: f32,
    pub y_top: f32,
    pub y_bot: f32,
}

// The underline of a face that carries no `post` table, as a fraction of the
// em. These are the PostScript FontInfo defaults, -100 and 50 in a 1000-unit
// em.
const FALLBACK_UNDERLINE_POS: f32 = -0.1;
const FALLBACK_UNDERLINE_THICKNESS: f32 = 0.05;

/// The underline rectangle of a laid-out node, from the underline metrics of
/// its face.
pub fn underline_rect(layout: &TextLayout) -> UnderlineRect {
    let face_units = layout.face.units_per_em() as f32;
    let scale = em_scale(layout.face, layout.size) as f32;
    let (pos_units, thickness_units) = layout.face.underline_metrics().map_or(
        (
            FALLBACK_UNDERLINE_POS * face_units,
            FALLBACK_UNDERLINE_THICKNESS * face_units,
        ),
        |m| (f32::from(m.position), f32::from(m.thickness)),
    );
    let underline_pos = -pos_units * scale; // font y is up, box y is down
    let thickness = thickness_units * scale;
    let y_top = layout.baseline_y + underline_pos - thickness / 2.0;
    UnderlineRect {
        x_l: layout.x_left(),
        x_r: layout.x_left() + layout.width,
        y_top,
        y_bot: y_top + thickness,
    }
}

struct OutlineAdapter<'a> {
    out: &'a mut dyn OutlineBuilder,
    scale: f32,
    origin_x: f32,
    baseline_y: f32,
}

impl<'a> OutlineAdapter<'a> {
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.origin_x + x * self.scale,
            self.baseline_y - y * self.scale,
        )
    }
}

impl<'a> ttf_parser::OutlineBuilder for OutlineAdapter<'a> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (mx, my) = self.map(x, y);
        self.out.move_to(mx, my);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (mx, my) = self.map(x, y);
        self.out.line_to(mx, my);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (cx, cy) = self.map(x1, y1);
        let (ex, ey) = self.map(x, y);
        self.out.quad_to(cx, cy, ex, ey);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1x, c1y) = self.map(x1, y1);
        let (c2x, c2y) = self.map(x2, y2);
        let (ex, ey) = self.map(x, y);
        self.out.cubic_to(c1x, c1y, c2x, c2y, ex, ey);
    }
    fn close(&mut self) {
        self.out.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Liberation Sans Regular, the face of a node that names no family.
    fn sans() -> &'static Face<'static> {
        SANS[0].face()
    }

    /// Outline a node as a renderer does.
    fn outline_node(node: &TextNode, out: &mut dyn OutlineBuilder) {
        if let Some(layout) = layout_text(node) {
            outline_layout(&layout, &node.text, out);
        }
    }

    /// Records the ops, so a test asserts them exactly.
    #[derive(Default)]
    struct Recorder {
        ops: Vec<String>,
    }

    impl Recorder {
        /// The number of ops of one kind, by its letter.
        fn count(&self, op: char) -> usize {
            self.ops.iter().filter(|o| o.starts_with(op)).count()
        }
    }

    impl OutlineBuilder for Recorder {
        fn move_to(&mut self, x: f32, y: f32) {
            self.ops.push(format!("M {x} {y}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.ops.push(format!("L {x} {y}"));
        }
        fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
            self.ops.push(format!("Q {cx} {cy} {x} {y}"));
        }
        fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
            self.ops.push(format!("C {c1x} {c1y} {c2x} {c2y} {x} {y}"));
        }
        fn close(&mut self) {
            self.ops.push("Z".to_string());
        }
    }

    #[test]
    fn elevate_quads_tracks_the_point_across_close() {
        // After `close` the current point is the start of the subpath, not
        // the end of the last op.
        let mut sink = Recorder::default();
        {
            let mut out = ElevateQuads::new(&mut sink);
            out.move_to(0.0, 0.0);
            out.line_to(6.0, 0.0);
            out.close();
            out.quad_to(3.0, 3.0, 6.0, 0.0);
        }
        assert_eq!(
            sink.ops,
            vec![
                "M 0 0".to_string(),
                "L 6 0".to_string(),
                "Z".to_string(),
                // anchored at (0, 0), the start of the subpath
                "C 2 2 4 2 6 0".to_string(),
            ]
        );
    }

    #[test]
    fn elevate_quads_drops_a_contour_opening_on_a_quad() {
        let mut sink = Recorder::default();
        {
            let mut out = ElevateQuads::new(&mut sink);
            out.quad_to(3.0, 3.0, 6.0, 0.0);
            out.move_to(1.0, 1.0);
        }
        assert_eq!(sink.ops, vec!["M 1 1".to_string()]);
    }

    #[test]
    fn measure_width_empty_is_zero() {
        assert_eq!(measure_width_with(sans(), "", 20.0), 0.0);
    }

    #[test]
    fn measure_width_grows_with_chars() {
        let one = measure_width_with(sans(), "h", 20.0);
        let many = measure_width_with(sans(), "hhhh", 20.0);
        assert!(many > one * 3.5, "{many} should be roughly 4x {one}");
    }

    #[test]
    fn measure_height_uses_font_metrics() {
        let h = measure_height_with(sans(), 20.0);
        // Liberation Sans at 20px. (1854 + 434) * 20 / 2048 is about 22.34.
        assert!(h > 18.0 && h < 26.0, "unexpected height: {h}");
    }

    #[test]
    fn y_offset_is_within_box() {
        let h = measure_height_with(sans(), 20.0);
        let y = measure_y_offset_with(sans(), 20.0);
        assert!(y > -h / 2.0 && y < h / 2.0);
    }

    #[test]
    fn outline_emits_some_commands_for_letters() {
        let mut b = Recorder::default();
        outline_node(&node(30.0, "Ag"), &mut b);
        assert!(b.count('M') > 0, "no moves emitted");
        assert!(
            b.count('L') > 0 || b.count('Q') > 0,
            "no draw segments emitted"
        );
        assert!(b.count('Z') > 0, "outline did not close");
    }

    #[test]
    fn outline_space_only_advances_pen_no_glyphs() {
        let mut b = Recorder::default();
        outline_node(&node(30.0, "   "), &mut b);
        assert!(b.ops.is_empty(), "{:?}", b.ops);
        assert!(measure_width_with(sans(), "   ", 30.0) > 0.0);
    }

    #[test]
    fn a_control_character_draws_nothing() {
        let mut plain = Recorder::default();
        outline_node(&node(30.0, "AB"), &mut plain);
        for s in [
            "A\nB", "A\r\nB", "A\u{0}B", "A\u{1b}B", "A\u{7f}B", "A\u{9f}B",
        ] {
            assert_eq!(
                measure_width_with(sans(), s, 30.0),
                measure_width_with(sans(), "AB", 30.0),
                "{s:?}"
            );
            let mut with = Recorder::default();
            outline_node(&node(30.0, s), &mut with);
            assert_eq!(with.ops, plain.ops, "{s:?}");
        }
    }

    #[test]
    fn a_tab_draws_eight_spaces_of_the_face() {
        let mut widths = Vec::new();
        for family in ["sans-serif", "monospace"] {
            let face = resolve(family, 400, FontStyle::Normal).face();
            let tab = measure_width_with(face, "A\tB", 30.0);
            assert_eq!(
                tab,
                measure_width_with(face, "A        B", 30.0),
                "{family}"
            );
            let mut with_tab = Recorder::default();
            outline_node(
                &TextNode {
                    family: family.into(),
                    ..node(30.0, "A\tB")
                },
                &mut with_tab,
            );
            let mut with_spaces = Recorder::default();
            outline_node(
                &TextNode {
                    family: family.into(),
                    ..node(30.0, "A        B")
                },
                &mut with_spaces,
            );
            assert_eq!(with_tab.ops, with_spaces.ops, "{family}");
            widths.push(tab);
        }
        assert_ne!(
            widths[0], widths[1],
            "the width does not come from the face"
        );
    }

    #[test]
    fn a_character_the_face_lacks_draws_the_notdef_box() {
        let face = sans();
        let emoji = '\u{1f600}';
        assert!(face.glyph_index(emoji).is_none(), "the face covers {emoji}");
        let notdef = f64::from(face.glyph_hor_advance(GlyphId(0)).unwrap());
        let expected = notdef * 30.0 / f64::from(face.units_per_em());
        assert_eq!(measure_width_with(sans(), "\u{1f600}", 30.0), expected);
        let mut b = Recorder::default();
        outline_node(&node(30.0, "\u{1f600}"), &mut b);
        assert!(
            b.count('M') > 0 && b.count('Z') > 0,
            "the box has no contour"
        );
    }

    #[test]
    fn portuguese_chars_have_glyphs() {
        let s = "ção";
        let w = measure_width_with(sans(), s, 20.0);
        assert!(w > 0.0);
        let mut b = Recorder::default();
        outline_node(&node(30.0, s), &mut b);
        assert!(b.count('M') > 0);
    }

    #[test]
    fn resolve_empty_family_picks_sans_regular() {
        let f = resolve("", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
        assert!(!f.from_system);
    }

    #[test]
    fn resolve_serif_alias_picks_serif() {
        let f = resolve("serif", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Serif");
    }

    #[test]
    fn resolve_mono_alias_picks_mono() {
        for name in ["mono", "monospace", "Liberation Mono"] {
            let f = resolve(name, 400, FontStyle::Normal);
            assert_eq!(f.family, "Liberation Mono", "name={name}");
        }
    }

    #[test]
    fn resolve_ignores_the_space_around_the_family() {
        for name in ["  serif  ", "\tserif\n", "   "] {
            let f = resolve(name, 400, FontStyle::Normal);
            let want = if name.trim().is_empty() {
                "Liberation Sans"
            } else {
                "Liberation Serif"
            };
            assert_eq!(f.family, want, "name={name:?}");
        }
    }

    #[test]
    fn resolve_is_case_insensitive() {
        let f = resolve("SANS-SERIF", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
    }

    #[test]
    fn bold_picks_a_different_face_than_regular() {
        let regular = resolve("", 400, FontStyle::Normal);
        let bold = resolve("", 700, FontStyle::Normal);
        let w_reg = measure_width_with(regular.face(), "Hello", 20.0);
        let w_bold = measure_width_with(bold.face(), "Hello", 20.0);
        assert!(
            w_bold > w_reg,
            "expected bold wider than regular: {w_bold} vs {w_reg}"
        );
    }

    #[test]
    fn italic_resolves_to_italic_face() {
        // The italic 'a' has a different outline from the regular one.
        let mut b1 = Recorder::default();
        outline_node(&node(30.0, "a"), &mut b1);
        let mut b2 = Recorder::default();
        outline_node(
            &TextNode {
                style: FontStyle::Italic,
                ..node(30.0, "a")
            },
            &mut b2,
        );
        assert_ne!(
            b1.ops, b2.ops,
            "italic and regular outlined identically — variant probably not picked"
        );
    }

    #[test]
    fn unknown_family_falls_back_to_sans_when_not_in_fontdb() {
        // No system has a font with this name, so resolve falls through to
        // Liberation Sans.
        let f = resolve("ZZZ_NonexistentFontXyzzy_ZZZ", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
    }

    fn node(size: f32, text: &str) -> TextNode {
        TextNode {
            size,
            text: text.to_string(),
            ..TextNode::default()
        }
    }

    #[test]
    fn layout_text_returns_none_when_the_node_draws_nothing() {
        assert!(layout_text(&node(20.0, "")).is_none(), "empty text");
        assert!(layout_text(&node(0.0, "Hi")).is_none(), "zero size");
        assert!(layout_text(&node(-4.0, "Hi")).is_none(), "negative size");
        assert!(layout_text(&node(f32::NAN, "Hi")).is_none(), "NaN size");
        assert!(
            layout_text(&node(f32::INFINITY, "Hi")).is_none(),
            "infinite size"
        );
        assert!(
            layout_text(&node(f32::NEG_INFINITY, "Hi")).is_none(),
            "size of minus infinity"
        );
        assert!(
            layout_text(&node(f32::MAX, "Hello, world")).is_none(),
            "size that overflows the measured width"
        );
        assert!(
            layout_text(&node(20.0, "\r\n")).is_none(),
            "only control characters"
        );
        // U+200B is a zero-width space, so the text has chars and no width.
        assert!(
            layout_text(&node(20.0, "\u{200b}")).is_none(),
            "zero measured width"
        );
    }

    #[test]
    fn layout_text_draws_below_one_unit_of_size() {
        let small = layout_text(&node(0.9, "Hi")).expect("node draws");
        let tenth = layout_text(&node(0.09, "Hi")).expect("node draws");
        assert!(small.width > 0.0);
        assert!(
            (small.width / tenth.width - 10.0).abs() < 1e-2,
            "width {} over {} is not the ratio of the sizes",
            small.width,
            tenth.width
        );
    }

    #[test]
    fn underline_spans_the_text_and_centers_below_the_baseline() {
        let layout = layout_text(&node(24.0, "Hello")).expect("node draws");
        let u = underline_rect(&layout);
        assert_eq!(u.x_l, -layout.width / 2.0);
        assert!((u.x_r - layout.width / 2.0).abs() < 1e-4);
        assert!(u.y_bot > u.y_top, "the underline has no thickness");
        let center = (u.y_top + u.y_bot) / 2.0;
        assert!(
            center > layout.baseline_y,
            "the underline centre sits above the baseline"
        );
    }

    #[test]
    fn underline_thickness_stays_proportional_at_a_tiny_size() {
        let small = layout_text(&node(2.0, "Hi")).expect("node draws");
        let big = layout_text(&node(64.0, "Hi")).expect("node draws");
        let t_small = underline_rect(&small).y_bot - underline_rect(&small).y_top;
        let t_big = underline_rect(&big).y_bot - underline_rect(&big).y_top;
        assert!(t_small > 0.0);
        assert!(
            (t_big / t_small - 32.0).abs() < 1e-3,
            "thickness {t_big} over {t_small} is not the ratio of the sizes"
        );
    }

    #[test]
    fn text_box_affine_scales_exactly_when_layout_text_draws() {
        // Past this size the height overflows while the width of an "i"
        // stays finite.
        let tall = f32::MAX / 1.1;
        for (size, text) in [
            (20.0, "Hi"),
            (20.0, ""),
            (0.0, "Hi"),
            (f32::NAN, "Hi"),
            (20.0, "\u{200b}"),
            (f32::MAX, "Hello, world"),
            (tall, "i"),
        ] {
            let m = crate::scene::text_box_affine(
                "",
                400,
                FontStyle::Normal,
                size,
                text,
                5.0,
                7.0,
                100.0,
                40.0,
                0.0,
            );
            let scaled = m != [1.0, 0.0, 0.0, 1.0, 5.0, 7.0];
            let draws = layout_text(&node(size, text)).is_some();
            assert_eq!(scaled, draws, "size {size}, text {text:?}");
        }
    }

    /// The device rectangle of the underline of a node fitted to a box.
    fn underline_in_box(size: f32) -> [f32; 4] {
        let text = "Hello";
        let m = crate::scene::text_box_affine(
            "",
            400,
            FontStyle::Normal,
            size,
            text,
            0.0,
            0.0,
            100.0,
            40.0,
            0.0,
        );
        let layout = layout_text(&node(size, text)).expect("node draws");
        let u = underline_rect(&layout);
        let map = |x: f32, y: f32| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        let (x_l, y_top) = map(u.x_l, u.y_top);
        let (x_r, y_bot) = map(u.x_r, u.y_bot);
        [x_l, y_top, x_r, y_bot]
    }

    #[test]
    fn the_underline_of_a_fitted_box_does_not_move_with_the_size() {
        // The fit divides by the measurement, so a node in a box lands in the
        // same place whatever size it was measured at. The glyphs always did.
        // The underline only does once it shares that size.
        let whole = underline_in_box(24.0);
        let fraction = underline_in_box(24.9);
        for (a, b) in whole.iter().zip(fraction.iter()) {
            assert!((a - b).abs() < 1e-3, "{whole:?} against {fraction:?}");
        }
    }

    #[test]
    fn outline_underline_emits_the_rect_as_a_closed_contour() {
        let layout = layout_text(&node(24.0, "Hello")).expect("node draws");
        let u = underline_rect(&layout);
        let mut r = Recorder::default();
        outline_underline(&layout, &mut r);
        assert_eq!(
            r.ops,
            [
                format!("M {} {}", u.x_l, u.y_top),
                format!("L {} {}", u.x_r, u.y_top),
                format!("L {} {}", u.x_r, u.y_bot),
                format!("L {} {}", u.x_l, u.y_bot),
                "Z".to_string(),
            ]
        );
    }
}
