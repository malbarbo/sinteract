//! Native text measurement and outline extraction.
//!
//! Three font families are embedded — Liberation Sans, Serif, and Mono, in
//! Regular / Bold / Italic / BoldItalic. Aliases (`sans-serif`, `serif`,
//! `monospace`, `mono`, "Liberation X") map to the embedded variants.
//! Anything else falls back to a [`fontdb`] system query, with the
//! ultimate fallback being Liberation Sans.
//!
//! Used by:
//!   - the CLI terminal renderer (`terminal::rasterize_scene`),
//!   - the PDF renderer (`pdf::render_text`),
//!   - tests / hosts that need to lay out text without a renderer.
//!
//! The measurements return offsets relative to the *box center* — text
//! spans (-width/2, -height/2) to (width/2, height/2) in box-local
//! coordinates. The caller composes
//! `translate(cx, cy) * rotate(angle) * scale(sx, sy)` to place the text
//! in world coordinates.
//!
//! WASM targets do not use this module: the JS frontend measures text via
//! `OffscreenCanvas` and supplies metrics through the env imports.

use std::sync::{Mutex, OnceLock};

use ttf_parser::{Face, GlyphId};

use crate::scene::FontStyle;

// ---------------------------------------------------------------------------
// Embedded fonts
// ---------------------------------------------------------------------------

/// One embedded TTF: bytes + a parsed [`Face`] cached with a `OnceLock`. We
/// pay the parse cost once, then reuse for the rest of the process.
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

macro_rules! embed {
    ($name:literal, $path:literal) => {
        EmbeddedFont {
            name: $name,
            bytes: include_bytes!($path),
            face: OnceLock::new(),
        }
    };
}

static SANS: [EmbeddedFont; 4] = [
    embed!("Liberation Sans", "../fonts/LiberationSans-Regular.ttf"),
    embed!("Liberation Sans", "../fonts/LiberationSans-Bold.ttf"),
    embed!("Liberation Sans", "../fonts/LiberationSans-Italic.ttf"),
    embed!("Liberation Sans", "../fonts/LiberationSans-BoldItalic.ttf"),
];
static SERIF: [EmbeddedFont; 4] = [
    embed!("Liberation Serif", "../fonts/LiberationSerif-Regular.ttf"),
    embed!("Liberation Serif", "../fonts/LiberationSerif-Bold.ttf"),
    embed!("Liberation Serif", "../fonts/LiberationSerif-Italic.ttf"),
    embed!(
        "Liberation Serif",
        "../fonts/LiberationSerif-BoldItalic.ttf"
    ),
];
static MONO: [EmbeddedFont; 4] = [
    embed!("Liberation Mono", "../fonts/LiberationMono-Regular.ttf"),
    embed!("Liberation Mono", "../fonts/LiberationMono-Bold.ttf"),
    embed!("Liberation Mono", "../fonts/LiberationMono-Italic.ttf"),
    embed!("Liberation Mono", "../fonts/LiberationMono-BoldItalic.ttf"),
];

/// CSS weight at and above which we treat the request as "bold".
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

/// A resolved font: which family / variant the renderer ended up using.
/// Hosts echo `family` back on the wire so multiplayer clients pick the
/// same variant the server measured against.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedFont {
    /// Display name of the family that was actually used. Always one of
    /// `"Liberation Sans"`, `"Liberation Serif"`, `"Liberation Mono"`, or
    /// the system-discovered name.
    pub family: &'static str,
    /// `true` if the resolution required a system lookup. Tests rely on
    /// this to detect fallback paths.
    pub from_system: bool,
    face: &'static Face<'static>,
}

impl ResolvedFont {
    pub fn face(&self) -> &Face<'static> {
        self.face
    }
}

/// Resolve a `family + weight + style` request into a concrete font.
///
/// The resolution rules, in order:
/// 1. **Empty** family → `Liberation Sans`.
/// 2. **Aliases** (`sans-serif`, `serif`, `monospace`, `mono`,
///    `"Liberation Sans"`, `"Liberation Serif"`, `"Liberation Mono"`,
///    case-insensitive) → the embedded family.
/// 3. **System font** matching `family` via [`fontdb`].
/// 4. **Fallback** → `Liberation Sans`.
pub fn resolve(family: &str, weight: u16, style: FontStyle) -> ResolvedFont {
    let v = variant_index(weight, style);

    if family.is_empty() {
        return embedded(&SANS[v]);
    }

    let key = family.trim().to_ascii_lowercase();
    let alias = match key.as_str() {
        "" | "sans-serif" | "sans" | "liberation sans" => Some(&SANS),
        "serif" | "liberation serif" => Some(&SERIF),
        "monospace" | "mono" | "liberation mono" => Some(&MONO),
        _ => None,
    };
    if let Some(family_arr) = alias {
        return embedded(&family_arr[v]);
    }

    if let Some(font) = system_font(family, weight, style) {
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
// System font lookup (via fontdb)
// ---------------------------------------------------------------------------

fn font_db() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// Mapping from a fontdb face id to the leaked face we've already parsed.
/// Each system font is loaded into memory and parsed once, then leaked so
/// the resulting `Face<'static>` matches the embedded fonts' lifetime
/// shape. The number of unique fonts a process touches is bounded, so the
/// leak is benign.
fn system_cache() -> &'static Mutex<Vec<(fontdb::ID, ResolvedFont)>> {
    static CACHE: OnceLock<Mutex<Vec<(fontdb::ID, ResolvedFont)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn system_font(family: &str, weight: u16, style: FontStyle) -> Option<ResolvedFont> {
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

    {
        let cache = system_cache().lock().ok()?;
        if let Some((_, f)) = cache.iter().find(|(c, _)| *c == id) {
            return Some(*f);
        }
    }

    let face_data = db.with_face_data(id, |bytes, _idx| -> Option<ResolvedFont> {
        // Leak the bytes so the parsed face has a 'static lifetime,
        // matching our embedded-font shape.
        let owned: Box<[u8]> = bytes.to_vec().into_boxed_slice();
        let static_bytes: &'static [u8] = Box::leak(owned);
        let face = Face::parse(static_bytes, 0).ok()?;
        // Also leak the parsed face so we can hand out a `&'static Face`.
        let face_static: &'static Face<'static> = Box::leak(Box::new(face));
        // Echo the resolved canonical family name so multiplayer clients
        // can pick the same one. Leaking the String → &'static str.
        let canonical = db
            .face(id)
            .map(|info| {
                info.families
                    .first()
                    .map(|(n, _)| n.clone())
                    .unwrap_or_else(|| family.to_owned())
            })
            .unwrap_or_else(|| family.to_owned());
        let canonical_static: &'static str = Box::leak(canonical.into_boxed_str());
        Some(ResolvedFont {
            family: canonical_static,
            from_system: true,
            face: face_static,
        })
    })??;

    {
        let mut cache = system_cache().lock().ok()?;
        cache.push((id, face_data));
    }
    Some(face_data)
}

// ---------------------------------------------------------------------------
// Convenience wrappers — preserve the historic API for callers that
// only need the default Sans Regular variant.
// ---------------------------------------------------------------------------

fn default_face() -> &'static Face<'static> {
    SANS[0].face()
}

/// Receiver for outline path commands. Coordinates are in the same space as
/// the values returned by the `measure_*` functions (y increases downward).
pub trait OutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32);
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32);
    fn close(&mut self);
}

/// Total horizontal advance of `text` rendered at `size_px` in `face`.
pub fn measure_width_with(face: &Face<'_>, text: &str, size_px: i32) -> f64 {
    if text.is_empty() || size_px <= 0 {
        return 0.0;
    }
    let scale = f64::from(size_px) / f64::from(face.units_per_em());
    let mut total: f64 = 0.0;
    for c in text.chars() {
        let gid = face.glyph_index(c).unwrap_or(GlyphId(0));
        total += f64::from(face.glyph_hor_advance(gid).unwrap_or(0));
    }
    total * scale
}

pub fn measure_height_with(face: &Face<'_>, _text: &str, size_px: i32) -> f64 {
    if size_px <= 0 {
        return 0.0;
    }
    let scale = f64::from(size_px) / f64::from(face.units_per_em());
    let h = f64::from(face.ascender()) - f64::from(face.descender());
    h * scale
}

pub fn measure_x_offset_with(face: &Face<'_>, text: &str, size_px: i32) -> f64 {
    -measure_width_with(face, text, size_px) / 2.0
}

pub fn measure_y_offset_with(face: &Face<'_>, _text: &str, size_px: i32) -> f64 {
    if size_px <= 0 {
        return 0.0;
    }
    let scale = f64::from(size_px) / f64::from(face.units_per_em());
    (f64::from(face.ascender()) + f64::from(face.descender())) / 2.0 * scale
}

pub fn outline_with(face: &Face<'_>, text: &str, size_px: i32, out: &mut dyn OutlineBuilder) {
    if text.is_empty() || size_px <= 0 {
        return;
    }
    let scale = f64::from(size_px) / f64::from(face.units_per_em());
    let baseline_y = measure_y_offset_with(face, text, size_px) as f32;
    let start_x = measure_x_offset_with(face, text, size_px) as f32;

    let mut pen_x: f64 = 0.0;
    for c in text.chars() {
        let gid = face.glyph_index(c).unwrap_or(GlyphId(0));
        let mut adapter = OutlineAdapter {
            out,
            scale: scale as f32,
            origin_x: start_x + (pen_x * scale) as f32,
            baseline_y,
        };
        let _ = face.outline_glyph(gid, &mut adapter);
        pen_x += f64::from(face.glyph_hor_advance(gid).unwrap_or(0));
    }
}

// Historic `measure_*` API — uses Liberation Sans Regular. Kept for
// callers that have not migrated to the family/weight/style resolver yet.

pub fn measure_width(text: &str, size_px: i32) -> f64 {
    measure_width_with(default_face(), text, size_px)
}

pub fn measure_height(text: &str, size_px: i32) -> f64 {
    measure_height_with(default_face(), text, size_px)
}

pub fn measure_x_offset(text: &str, size_px: i32) -> f64 {
    measure_x_offset_with(default_face(), text, size_px)
}

pub fn measure_y_offset(text: &str, size_px: i32) -> f64 {
    measure_y_offset_with(default_face(), text, size_px)
}

pub fn outline(text: &str, size_px: i32, out: &mut dyn OutlineBuilder) {
    outline_with(default_face(), text, size_px, out)
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

    struct CountingBuilder {
        moves: u32,
        lines: u32,
        quads: u32,
        cubics: u32,
        closes: u32,
    }

    impl OutlineBuilder for CountingBuilder {
        fn move_to(&mut self, _x: f32, _y: f32) {
            self.moves += 1;
        }
        fn line_to(&mut self, _x: f32, _y: f32) {
            self.lines += 1;
        }
        fn quad_to(&mut self, _cx: f32, _cy: f32, _x: f32, _y: f32) {
            self.quads += 1;
        }
        fn cubic_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {
            self.cubics += 1;
        }
        fn close(&mut self) {
            self.closes += 1;
        }
    }

    fn empty_builder() -> CountingBuilder {
        CountingBuilder {
            moves: 0,
            lines: 0,
            quads: 0,
            cubics: 0,
            closes: 0,
        }
    }

    #[test]
    fn measure_width_empty_is_zero() {
        assert_eq!(measure_width("", 20), 0.0);
    }

    #[test]
    fn measure_width_grows_with_size() {
        let small = measure_width("hello", 10);
        let big = measure_width("hello", 20);
        assert!(big > small * 1.5, "{big} should be roughly 2x {small}");
    }

    #[test]
    fn measure_width_grows_with_chars() {
        let one = measure_width("h", 20);
        let many = measure_width("hhhh", 20);
        assert!(many > one * 3.5, "{many} should be roughly 4x {one}");
    }

    #[test]
    fn measure_height_uses_font_metrics() {
        let h = measure_height("anything", 20);
        // Liberation Sans at 20px: ascender 1854, descender -434, em 2048
        // → (1854 - (-434)) * 20 / 2048 ≈ 22.34
        assert!(h > 18.0 && h < 26.0, "unexpected height: {h}");
    }

    #[test]
    fn x_offset_centers_text() {
        let w = measure_width("hi", 20);
        let x = measure_x_offset("hi", 20);
        assert!((x + w / 2.0).abs() < 1e-6);
    }

    #[test]
    fn y_offset_is_within_box() {
        let h = measure_height("hi", 20);
        let y = measure_y_offset("hi", 20);
        assert!(y > -h / 2.0 && y < h / 2.0);
    }

    #[test]
    fn outline_emits_some_commands_for_letters() {
        let mut b = empty_builder();
        outline("Ag", 30, &mut b);
        assert!(b.moves > 0, "no moves emitted");
        assert!(b.lines > 0 || b.quads > 0, "no draw segments emitted");
        assert!(b.closes > 0, "outline did not close");
    }

    #[test]
    fn outline_empty_string_emits_nothing() {
        let mut b = empty_builder();
        outline("", 30, &mut b);
        assert_eq!(b.moves, 0);
        assert_eq!(b.lines, 0);
        assert_eq!(b.closes, 0);
    }

    #[test]
    fn outline_space_only_advances_pen_no_glyphs() {
        let mut b = empty_builder();
        outline("   ", 30, &mut b);
        assert_eq!(b.moves, 0);
        assert_eq!(b.lines, 0);
        assert!(measure_width("   ", 30) > 0.0);
    }

    #[test]
    fn portuguese_chars_have_glyphs() {
        let s = "ção";
        let w = measure_width(s, 20);
        assert!(w > 0.0);
        let mut b = empty_builder();
        outline(s, 30, &mut b);
        assert!(b.moves > 0);
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
    fn resolve_is_case_insensitive() {
        let f = resolve("SANS-SERIF", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
    }

    #[test]
    fn bold_picks_a_different_face_than_regular() {
        let regular = resolve("", 400, FontStyle::Normal);
        let bold = resolve("", 700, FontStyle::Normal);
        let w_reg = measure_width_with(regular.face(), "Hello", 20);
        let w_bold = measure_width_with(bold.face(), "Hello", 20);
        // Bold "Hello" is wider than regular "Hello" in Liberation Sans.
        assert!(
            w_bold > w_reg,
            "expected bold wider than regular: {w_bold} vs {w_reg}"
        );
    }

    #[test]
    fn italic_resolves_to_italic_face() {
        // Glyph 'a' has slightly different metrics in Italic vs Regular —
        // we just check the face actually changed by walking the outlines
        // and counting points.
        let regular = resolve("", 400, FontStyle::Normal);
        let italic = resolve("", 400, FontStyle::Italic);
        let mut b1 = empty_builder();
        outline_with(regular.face(), "a", 30, &mut b1);
        let mut b2 = empty_builder();
        outline_with(italic.face(), "a", 30, &mut b2);
        assert!(
            b1.lines + b1.quads != b2.lines + b2.quads,
            "italic and regular outlined identically — variant probably not picked"
        );
    }

    #[test]
    fn unknown_family_falls_back_to_sans_when_not_in_fontdb() {
        // A name nobody ships should resolve to Liberation Sans (the safety
        // net at the bottom of `resolve`). We don't depend on fontconfig's
        // exact behavior for this name; instead we pick something so
        // implausible that no system would have it.
        let f = resolve("ZZZ_SimageNonexistentFontXyzzy_ZZZ", 400, FontStyle::Normal);
        assert_eq!(f.family, "Liberation Sans");
    }
}
