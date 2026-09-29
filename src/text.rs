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
//! pixel. A [`crate::scene::Text`] measures in text space, and its
//! `transform` maps that space to the canvas.
//!
//! A measurement is taken from the center of the text box. The text spans
//! (-width/2, -height/2) to (width/2, height/2), and
//! [`crate::scene::TextSpec::fit`] maps that box onto a
//! [`crate::scene::RotatedRect`].

use std::sync::OnceLock;

use ttf_parser::{Face, GlyphId};

use crate::outline::PathSink;
use crate::scene::{FontStyle, TextSpec};

// ---------------------------------------------------------------------------
// Text metrics, the public API
// ---------------------------------------------------------------------------

/// Measure a text in the face that `family`, `weight` and `style` pick.
///
/// The family loses its surrounding space first. An empty family is
/// Liberation Sans, and an alias (`sans-serif`, `sans`, `serif`, `monospace`,
/// `mono`, or an embedded family name, in any case) is the embedded family.
/// Any other name goes to the fonts installed on the system, and to
/// Liberation Sans when none matches or when the crate carries no system
/// lookup. In an embedded family, a weight of 600 or more picks the bold
/// face, and an italic or oblique style the italic one.
///
/// An empty text measures zero wide, with the height of the face. Returns
/// `None` when the size is not a positive finite number, when the
/// measurement overflows, or when the height does not come out positive.
pub fn measure(
    family: &str,
    weight: u16,
    style: FontStyle,
    size: f32,
    text: &str,
) -> Option<TextMetrics> {
    ResolvedFont::resolve(family, weight, style).measure(size, text)
}

/// The size of a measured text and the family it measured in, for a producer
/// that fits the text to a box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextMetrics {
    family: &'static str,
    width: f32,
    height: f32,
    baseline_y: f32,
}

impl TextMetrics {
    /// The family after fallback. A [`TextSpec`] carries it, so a view
    /// measures with the same face.
    pub fn family(&self) -> &'static str {
        self.family
    }

    /// The horizontal advance. It is zero when no character of the text
    /// advances, as in an empty text.
    pub fn width(&self) -> f32 {
        self.width
    }

    /// The ascender minus the descender of the face.
    pub fn height(&self) -> f32 {
        self.height
    }

    /// The baseline, from the center of the box with y down.
    pub fn baseline_y(&self) -> f32 {
        self.baseline_y
    }
}

// ---------------------------------------------------------------------------
// Layout of a text node, shared by the renderers
// ---------------------------------------------------------------------------

/// The face, the text and the box-local metrics of one [`TextSpec`].
pub(crate) struct TextLayout<'a> {
    face: &'static Face<'static>,
    text: &'a str,
    /// The em of the text space, from `TextSpec::size`.
    size: f32,
    /// The horizontal advance.
    width: f32,
    /// The baseline, box-local with y down.
    baseline_y: f32,
}

impl<'a> TextLayout<'a> {
    /// Resolve and measure a text node. Returns `None` when the node draws
    /// nothing, because the size is not a positive finite number, the text is
    /// empty, or the width or the height measures zero or overflows.
    pub(crate) fn new(spec: &'a TextSpec) -> Option<Self> {
        let font = ResolvedFont::resolve(&spec.family, spec.weight, spec.style);
        let metrics = font.measure(spec.size, &spec.text)?;
        if metrics.width <= 0.0 {
            return None;
        }
        Some(Self {
            face: font.face,
            text: &spec.text,
            size: spec.size,
            width: metrics.width,
            baseline_y: metrics.baseline_y,
        })
    }

    /// The glyph outlines of the text.
    pub(crate) fn outline(&self, out: &mut dyn PathSink) {
        for (glyph, x) in self.placed_glyphs() {
            glyph.outline(x, self.baseline_y, out);
        }
    }

    /// Each glyph of the text and the box-local x of its origin. Every
    /// origin sits on [`Self::baseline_y`].
    pub(crate) fn placed_glyphs(&self) -> impl Iterator<Item = (Glyph, f32)> + '_ {
        let scale = em_scale(self.face, self.size);
        let x_left = self.x_left();
        let mut pen_x: f64 = 0.0;
        glyphs(self.face, self.text).map(move |(id, advance)| {
            let x = x_left + (pen_x * scale) as f32;
            pen_x += advance;
            let glyph = Glyph {
                face: self.face,
                id,
                size: self.size,
            };
            (glyph, x)
        })
    }

    /// The baseline, box-local with y down.
    pub(crate) fn baseline_y(&self) -> f32 {
        self.baseline_y
    }

    /// The underline as a closed contour.
    pub(crate) fn outline_underline(&self, out: &mut dyn PathSink) {
        let u = self.underline_rect();
        out.move_to(u.left, u.top);
        out.line_to(u.right, u.top);
        out.line_to(u.right, u.bottom);
        out.line_to(u.left, u.bottom);
        out.close();
    }

    /// The underline rectangle, from the underline metrics of the face.
    pub(crate) fn underline_rect(&self) -> UnderlineRect {
        let face_units = self.face.units_per_em() as f32;
        let scale = em_scale(self.face, self.size) as f32;
        let (pos_units, thickness_units) = self.face.underline_metrics().map_or(
            (
                FALLBACK_UNDERLINE_POS * face_units,
                FALLBACK_UNDERLINE_THICKNESS * face_units,
            ),
            |m| (f32::from(m.position), f32::from(m.thickness)),
        );
        // The position in `post` is the top of the underline, with y up, and
        // the box has y down.
        let top = self.baseline_y - pos_units * scale;
        let thickness = thickness_units * scale;
        UnderlineRect {
            left: self.x_left(),
            right: self.x_left() + self.width,
            top,
            bottom: top + thickness,
        }
    }

    /// The left edge of the text, box-local.
    fn x_left(&self) -> f32 {
        -self.width / 2.0
    }
}

/// Axis-aligned underline rectangle in box-local text space.
pub(crate) struct UnderlineRect {
    pub(crate) left: f32,
    pub(crate) right: f32,
    pub(crate) top: f32,
    pub(crate) bottom: f32,
}

/// One glyph of a face at a size. Two equal glyphs have the same outline, so
/// a backend can write the outline once and place it many times.
#[derive(Clone, Copy)]
pub(crate) struct Glyph {
    face: &'static Face<'static>,
    id: GlyphId,
    size: f32,
}

impl Glyph {
    /// The outline with the origin of the glyph at (x, y), box-local, with
    /// the origin at the center of the box and y down.
    pub(crate) fn outline(self, x: f32, y: f32, out: &mut dyn PathSink) {
        let mut adapter = OutlineAdapter {
            out,
            scale: em_scale(self.face, self.size) as f32,
            origin_x: x,
            baseline_y: y,
        };
        let _ = self.face.outline_glyph(self.id, &mut adapter);
    }
}

impl PartialEq for Glyph {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.face, other.face)
            && self.id == other.id
            && self.size.to_bits() == other.size.to_bits()
    }
}

impl Eq for Glyph {}

impl std::hash::Hash for Glyph {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(self.face, state);
        self.id.0.hash(state);
        self.size.to_bits().hash(state);
    }
}

// The underline of a face that carries no `post` table, as a fraction of the
// em. The PostScript FontInfo defaults center a stroke of 50 at -100 in a
// 1000-unit em, so its top sits at -75.
const FALLBACK_UNDERLINE_POS: f32 = -0.075;
const FALLBACK_UNDERLINE_THICKNESS: f32 = 0.05;

/// Maps the outline of one glyph from font units, with y up, to box-local
/// coordinates, with y down, for a [`PathSink`].
struct OutlineAdapter<'a> {
    out: &'a mut dyn PathSink,
    scale: f32,
    origin_x: f32,
    baseline_y: f32,
}

impl OutlineAdapter<'_> {
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.origin_x + x * self.scale,
            self.baseline_y - y * self.scale,
        )
    }
}

impl ttf_parser::OutlineBuilder for OutlineAdapter<'_> {
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

// ---------------------------------------------------------------------------
// Font resolution
// ---------------------------------------------------------------------------

/// The result of resolving a request. [`TextMetrics::family`] hands `family`
/// to the engine, which sends it on the wire, so a view measures with the
/// same face as the engine.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedFont {
    /// `"Liberation Sans"`, `"Liberation Serif"`, `"Liberation Mono"`, or
    /// the name fontdb reports.
    family: &'static str,
    face: &'static Face<'static>,
}

impl ResolvedFont {
    /// Resolve a family, a weight and a style to a face, by the rules that
    /// [`measure`] documents.
    pub(crate) fn resolve(family: &str, weight: u16, style: FontStyle) -> Self {
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
        if let Some(embedded) = alias {
            return embedded.variant(weight, style).resolved();
        }

        if let Some(font) = system::font(key, weight, style) {
            return font;
        }

        SANS.variant(weight, style).resolved()
    }

    fn measure(self, size: f32, text: &str) -> Option<TextMetrics> {
        if !drawable_size(size) {
            return None;
        }
        let face = self.face;
        let scale = em_scale(face, size);
        let advance: f64 = glyphs(face, text).map(|(_, advance)| advance).sum();
        let ascender = f64::from(face.ascender());
        let descender = f64::from(face.descender());
        let width = (advance * scale) as f32;
        let height = ((ascender - descender) * scale) as f32;
        let baseline_y = ((ascender + descender) / 2.0 * scale) as f32;
        // A huge size overflows the measurement, and a non-finite number would
        // reach the renderer as a coordinate and the producer as a scale.
        if !width.is_finite() || !height.is_finite() || !baseline_y.is_finite() || height <= 0.0 {
            return None;
        }
        Some(TextMetrics {
            family: self.family,
            width,
            height,
            baseline_y,
        })
    }
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Returns `true` if a text at `size` can draw, `false` otherwise. NaN and
/// infinity are out, since either one puts non-finite coordinates in the
/// outline.
fn drawable_size(size: f32) -> bool {
    size.is_finite() && size > 0.0
}

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

/// The width of a tab, in spaces of the face. A tab advances by the same
/// width at any column, as the `text` of Racket's `2htdp/image` does.
const TAB_SPACES: usize = 8;

/// The text-space length of one font unit of `face` at `size`.
fn em_scale(face: &Face<'_>, size: f32) -> f64 {
    f64::from(size) / f64::from(face.units_per_em())
}

// ---------------------------------------------------------------------------
// System font lookup, behind the native-fonts feature (see the module doc)
// ---------------------------------------------------------------------------

#[cfg(all(feature = "native-fonts", not(target_arch = "wasm32")))]
mod system;

#[cfg(not(all(feature = "native-fonts", not(target_arch = "wasm32"))))]
mod system {
    /// Without the lookup, no family resolves to a system font.
    pub(super) fn font(
        _family: &str,
        _weight: u16,
        _style: crate::scene::FontStyle,
    ) -> Option<super::ResolvedFont> {
        None
    }
}

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

    fn resolved(&'static self) -> ResolvedFont {
        ResolvedFont {
            family: self.name,
            face: self.face(),
        }
    }
}

/// The four variants of a family in `fonts/`.
struct Family {
    regular: EmbeddedFont,
    bold: EmbeddedFont,
    italic: EmbeddedFont,
    bold_italic: EmbeddedFont,
}

impl Family {
    /// The variant for `weight` and `style`. An oblique style takes the
    /// italic face.
    fn variant(&'static self, weight: u16, style: FontStyle) -> &'static EmbeddedFont {
        let bold = weight >= BOLD_THRESHOLD;
        let italic = !matches!(style, FontStyle::Normal);
        match (bold, italic) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        }
    }
}

macro_rules! embed_family {
    (@variant $name:literal, $file:literal, $variant:literal) => {
        EmbeddedFont {
            name: $name,
            bytes: include_bytes!(concat!("../fonts/", $file, "-", $variant, ".ttf")),
            face: OnceLock::new(),
        }
    };
    ($name:literal, $file:literal) => {
        Family {
            regular: embed_family!(@variant $name, $file, "Regular"),
            bold: embed_family!(@variant $name, $file, "Bold"),
            italic: embed_family!(@variant $name, $file, "Italic"),
            bold_italic: embed_family!(@variant $name, $file, "BoldItalic"),
        }
    };
}

static SANS: Family = embed_family!("Liberation Sans", "LiberationSans");
static SERIF: Family = embed_family!("Liberation Serif", "LiberationSerif");
static MONO: Family = embed_family!("Liberation Mono", "LiberationMono");

/// A CSS weight at or above this picks the bold face.
const BOLD_THRESHOLD: u16 = 600;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::Recorder;
    use crate::scene::RotatedRect;

    /// Liberation Sans Regular, the face of a node that names no family.
    fn sans() -> &'static Face<'static> {
        SANS.regular.face()
    }

    /// The metrics of `text` in `family` at `weight`, in the normal style.
    fn metrics(family: &str, weight: u16, size: f32, text: &str) -> TextMetrics {
        measure(family, weight, FontStyle::Normal, size, text).expect("text measures")
    }

    /// A spec of `text` at `size`, in the default family, weight and style.
    fn text_spec(size: f32, text: &str) -> TextSpec {
        TextSpec {
            size,
            text: text.to_string(),
            ..TextSpec::default()
        }
    }

    /// Outline a spec as a renderer does.
    fn outline_spec(spec: &TextSpec, out: &mut dyn PathSink) {
        if let Some(layout) = TextLayout::new(spec) {
            layout.outline(out);
        }
    }

    #[test]
    fn an_empty_text_measures_zero_wide() {
        assert_eq!(metrics("", 400, 20.0, "").width(), 0.0);
    }

    #[test]
    fn the_width_grows_with_the_characters() {
        let one = metrics("", 400, 20.0, "h").width();
        let many = metrics("", 400, 20.0, "hhhh").width();
        assert!(many > one * 3.5, "{many} should be roughly 4x {one}");
    }

    #[test]
    fn the_height_spans_the_ascender_to_the_descender() {
        let h = metrics("", 400, 20.0, "").height();
        // Liberation Sans has an ascender of 1854 and a descender of -434 in
        // a 2048-unit em, and the division is exact.
        assert_eq!(h, (1854.0 + 434.0) * 20.0 / 2048.0);
    }

    #[test]
    fn the_baseline_is_within_the_box() {
        let m = metrics("", 400, 20.0, "");
        let (h, y) = (m.height(), m.baseline_y());
        assert!(y > -h / 2.0 && y < h / 2.0);
    }

    #[test]
    fn outline_emits_some_commands_for_letters() {
        let mut b = Recorder::default();
        outline_spec(&text_spec(30.0, "Ag"), &mut b);
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
        outline_spec(&text_spec(30.0, "   "), &mut b);
        assert!(b.ops.is_empty(), "{:?}", b.ops);
        assert!(metrics("", 400, 30.0, "   ").width() > 0.0);
    }

    #[test]
    fn a_control_character_draws_nothing() {
        let mut plain = Recorder::default();
        outline_spec(&text_spec(30.0, "AB"), &mut plain);
        for s in [
            "A\nB", "A\r\nB", "A\u{0}B", "A\u{1b}B", "A\u{7f}B", "A\u{9f}B",
        ] {
            assert_eq!(
                metrics("", 400, 30.0, s).width(),
                metrics("", 400, 30.0, "AB").width(),
                "{s:?}"
            );
            let mut with = Recorder::default();
            outline_spec(&text_spec(30.0, s), &mut with);
            assert_eq!(with.ops, plain.ops, "{s:?}");
        }
    }

    #[test]
    fn a_tab_draws_eight_spaces_of_the_face() {
        let mut widths = Vec::new();
        for family in ["sans-serif", "monospace"] {
            let tab = metrics(family, 400, 30.0, "A\tB").width();
            assert_eq!(
                tab,
                metrics(family, 400, 30.0, "A        B").width(),
                "{family}"
            );
            let mut with_tab = Recorder::default();
            outline_spec(
                &TextSpec {
                    family: family.into(),
                    ..text_spec(30.0, "A\tB")
                },
                &mut with_tab,
            );
            let mut with_spaces = Recorder::default();
            outline_spec(
                &TextSpec {
                    family: family.into(),
                    ..text_spec(30.0, "A        B")
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
        let expected = (notdef * 30.0 / f64::from(face.units_per_em())) as f32;
        assert_eq!(metrics("", 400, 30.0, "\u{1f600}").width(), expected);
        let mut b = Recorder::default();
        outline_spec(&text_spec(30.0, "\u{1f600}"), &mut b);
        assert!(
            b.count('M') > 0 && b.count('Z') > 0,
            "the box has no contour"
        );
    }

    #[test]
    fn portuguese_chars_have_glyphs() {
        let s = "ção";
        let w = metrics("", 400, 20.0, s).width();
        assert!(w > 0.0);
        let mut b = Recorder::default();
        outline_spec(&text_spec(30.0, s), &mut b);
        assert!(b.count('M') > 0);
    }

    #[test]
    fn resolve_empty_family_picks_sans_regular() {
        let f = ResolvedFont::resolve("", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
        assert!(std::ptr::eq(f.face, sans()), "not the regular face");
    }

    #[test]
    fn resolve_serif_alias_picks_serif() {
        let f = ResolvedFont::resolve("serif", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Serif");
    }

    #[test]
    fn resolve_sans_alias_picks_sans() {
        for name in ["sans", "sans-serif", "Liberation Sans"] {
            let f = ResolvedFont::resolve(name, 400, FontStyle::Normal);
            assert_eq!(f.family, "Liberation Sans", "name={name}");
        }
    }

    #[test]
    fn resolve_mono_alias_picks_mono() {
        for name in ["mono", "monospace", "Liberation Mono"] {
            let f = ResolvedFont::resolve(name, 400, FontStyle::Normal);
            assert_eq!(f.family, "Liberation Mono", "name={name}");
        }
    }

    #[test]
    fn resolve_ignores_the_space_around_the_family() {
        for name in ["  serif  ", "\tserif\n", "   "] {
            let f = ResolvedFont::resolve(name, 400, FontStyle::Normal);
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
        let f = ResolvedFont::resolve("SANS-SERIF", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
    }

    #[test]
    fn bold_picks_a_different_face_than_regular() {
        let w_reg = metrics("", 400, 20.0, "Hello").width();
        let w_bold = metrics("", 700, 20.0, "Hello").width();
        assert!(
            w_bold > w_reg,
            "expected bold wider than regular: {w_bold} vs {w_reg}"
        );
    }

    #[test]
    fn italic_resolves_to_italic_face() {
        // The italic 'a' has a different outline from the regular one.
        let mut b1 = Recorder::default();
        outline_spec(&text_spec(30.0, "a"), &mut b1);
        let mut b2 = Recorder::default();
        outline_spec(
            &TextSpec {
                style: FontStyle::Italic,
                ..text_spec(30.0, "a")
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
        let f = ResolvedFont::resolve("ZZZ_NonexistentFontXyzzy_ZZZ", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
    }

    #[test]
    fn text_layout_returns_none_when_the_node_draws_nothing() {
        assert!(
            TextLayout::new(&text_spec(20.0, "")).is_none(),
            "empty text"
        );
        assert!(
            TextLayout::new(&text_spec(0.0, "Hi")).is_none(),
            "zero size"
        );
        assert!(
            TextLayout::new(&text_spec(-4.0, "Hi")).is_none(),
            "negative size"
        );
        assert!(
            TextLayout::new(&text_spec(f32::NAN, "Hi")).is_none(),
            "NaN size"
        );
        assert!(
            TextLayout::new(&text_spec(f32::INFINITY, "Hi")).is_none(),
            "infinite size"
        );
        assert!(
            TextLayout::new(&text_spec(f32::NEG_INFINITY, "Hi")).is_none(),
            "size of minus infinity"
        );
        assert!(
            TextLayout::new(&text_spec(f32::MAX, "Hello, world")).is_none(),
            "size that overflows the measured width"
        );
        assert!(
            TextLayout::new(&text_spec(20.0, "\r\n")).is_none(),
            "only control characters"
        );
        // U+200B is a zero-width space, so the text has chars and no width.
        assert!(
            TextLayout::new(&text_spec(20.0, "\u{200b}")).is_none(),
            "zero measured width"
        );
    }

    #[test]
    fn measure_gives_the_height_and_the_family_of_an_empty_text() {
        let m = measure("", 400, FontStyle::Normal, 20.0, "").expect("measures");
        let drawn_spec = text_spec(20.0, "Hi");
        let drawn = TextLayout::new(&drawn_spec).expect("node draws");
        let hi = measure("", 400, FontStyle::Normal, 20.0, "Hi").expect("measures");
        assert_eq!(m.width(), 0.0);
        assert_eq!(m.height(), hi.height());
        assert_eq!(m.baseline_y(), drawn.baseline_y);
        assert_eq!(m.family(), "Liberation Sans");
    }

    #[test]
    fn measure_gives_the_family_after_fallback() {
        let family = |name| measure(name, 400, FontStyle::Normal, 20.0, "Hi").map(|m| m.family());
        assert_eq!(family("serif"), Some("Liberation Serif"));
        assert_eq!(
            family("ZZZ_NonexistentFontXyzzy_ZZZ"),
            Some("Liberation Sans")
        );
    }

    #[test]
    fn measure_returns_none_for_a_size_that_cannot_draw() {
        for size in [0.0, -4.0, f32::NAN, f32::INFINITY, f32::MAX] {
            assert!(
                measure("", 400, FontStyle::Normal, size, "").is_none(),
                "size {size}"
            );
        }
    }

    #[test]
    fn measure_agrees_with_text_layout() {
        let m = measure("", 700, FontStyle::Italic, 24.5, "Olá").expect("measures");
        let spec = TextSpec {
            weight: 700,
            style: FontStyle::Italic,
            ..text_spec(24.5, "Olá")
        };
        let l = TextLayout::new(&spec).expect("node draws");
        assert_eq!((m.width(), m.baseline_y()), (l.width, l.baseline_y));
    }

    #[test]
    fn text_layout_draws_below_one_unit_of_size() {
        let small_spec = text_spec(0.9, "Hi");
        let small = TextLayout::new(&small_spec).expect("node draws");
        let tenth_spec = text_spec(0.09, "Hi");
        let tenth = TextLayout::new(&tenth_spec).expect("node draws");
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
        let spec = text_spec(24.0, "Hello");
        let layout = TextLayout::new(&spec).expect("node draws");
        let u = layout.underline_rect();
        assert_eq!(u.left, -layout.width / 2.0);
        assert!((u.right - layout.width / 2.0).abs() < 1e-4);
        assert!(u.bottom > u.top, "the underline has no thickness");
        let center = (u.top + u.bottom) / 2.0;
        assert!(
            center > layout.baseline_y,
            "the underline centre sits above the baseline"
        );
    }

    #[test]
    fn underline_top_sits_at_the_post_position() {
        // Liberation Sans puts the underline at -67 with a thickness of 150.
        // At a size of one em in font units, a unit of the font is one unit
        // of the box.
        let spec = text_spec(2048.0, "Hi");
        let layout = TextLayout::new(&spec).expect("node draws");
        let u = layout.underline_rect();
        assert_eq!(u.top - layout.baseline_y, 67.0);
        assert_eq!(u.bottom - layout.baseline_y, 217.0);
    }

    #[test]
    fn underline_thickness_stays_proportional_at_a_tiny_size() {
        let small_spec = text_spec(2.0, "Hi");
        let small = TextLayout::new(&small_spec).expect("node draws");
        let big_spec = text_spec(64.0, "Hi");
        let big = TextLayout::new(&big_spec).expect("node draws");
        let t_small = small.underline_rect().bottom - small.underline_rect().top;
        let t_big = big.underline_rect().bottom - big.underline_rect().top;
        assert!(t_small > 0.0);
        assert!(
            (t_big / t_small - 32.0).abs() < 1e-3,
            "thickness {t_big} over {t_small} is not the ratio of the sizes"
        );
    }

    #[test]
    fn fit_returns_a_text_exactly_when_a_text_layout_draws() {
        // Past this size the height overflows while the width of an "i"
        // stays finite.
        let tall = f32::MAX / 1.1;
        let rect = RotatedRect {
            cx: 5.0,
            cy: 7.0,
            w: 100.0,
            h: 40.0,
            angle_deg: 0.0,
        };
        for (size, text) in [
            (20.0, "Hi"),
            (20.0, ""),
            (0.0, "Hi"),
            (f32::NAN, "Hi"),
            (20.0, "\u{200b}"),
            (f32::MAX, "Hello, world"),
            (tall, "i"),
        ] {
            let spec = text_spec(size, text);
            let draws = TextLayout::new(&spec).is_some();
            assert_eq!(
                spec.fit(rect).is_some(),
                draws,
                "size {size}, text {text:?}"
            );
        }
    }

    /// The device rectangle of the underline of a node fitted to a box.
    fn underline_in_box(size: f32) -> [f32; 4] {
        let text = "Hello";
        let rect = RotatedRect {
            cx: 0.0,
            cy: 0.0,
            w: 100.0,
            h: 40.0,
            angle_deg: 0.0,
        };
        let spec = text_spec(size, text);
        let u = TextLayout::new(&spec).expect("node draws").underline_rect();
        let m = spec.fit(rect).expect("text fits").transform;
        let map = |x: f32, y: f32| (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]);
        let (left, top) = map(u.left, u.top);
        let (right, bottom) = map(u.right, u.bottom);
        [left, top, right, bottom]
    }

    #[test]
    fn the_underline_of_a_fitted_box_does_not_move_with_the_size() {
        // The fit divides by the measurement, so a node in a box lands in the
        // same place whatever size it was measured at, the glyphs and the
        // underline alike.
        let whole = underline_in_box(24.0);
        let fraction = underline_in_box(24.9);
        for (a, b) in whole.iter().zip(fraction.iter()) {
            assert!((a - b).abs() < 1e-3, "{whole:?} against {fraction:?}");
        }
    }

    #[test]
    fn outline_underline_emits_the_rect_as_a_closed_contour() {
        let spec = text_spec(24.0, "Hello");
        let layout = TextLayout::new(&spec).expect("node draws");
        let u = layout.underline_rect();
        let mut r = Recorder::default();
        layout.outline_underline(&mut r);
        assert_eq!(
            r.ops,
            [
                format!("M {} {}", u.left, u.top),
                format!("L {} {}", u.right, u.top),
                format!("L {} {}", u.right, u.bottom),
                format!("L {} {}", u.left, u.bottom),
                "Z".to_string(),
            ]
        );
    }
}
