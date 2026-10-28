//! The drawing of an image on a scene.

use std::fmt;
use std::rc::Rc;

use super::geometry::{ImageBox, Point};
use super::path::{self, RESIDUE, without_residue};
use super::tree::Node;
use super::{Attribute, Color, Font, Image, Length, Style};
use crate::renderer::Renderer;
use crate::renderer::svg::{IdPrefix, SvgRenderer};
use crate::scene::{
    Bitmap, ClipPath, Dash, FillRule, LineCap, LineJoin, MiterLimit, Paint, Path, PathFill,
    PathStroke, PathStyle, Sampling, Scene, StrokeWidth, Text,
};

/// The canvas of the image is too large. Its width or its height is above
/// the largest `f32`, which the scene takes, or, for [`Image::same`], the
/// canvas has more pixels than a pixmap holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLarge;

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the image is too large to draw")
    }
}

impl std::error::Error for TooLarge {}

impl Image {
    /// The width and the height of the canvas that holds `self`, in whole
    /// pixels.
    pub fn canvas_size(&self) -> Result<(f32, f32), TooLarge> {
        let size = |v: Length| {
            let v = whole_pixels(v.get()) as f32;
            if v.is_finite() { Ok(v) } else { Err(TooLarge) }
        };
        let (w, h) = self.dimension();
        Ok((size(w)?, size(h)?))
    }

    /// Adds what `self` draws to `scene`, with the least corner of `self` at
    /// the origin. A crop inside another becomes one clip of the two regions
    /// together, so no clip of the scene holds another, and the nesting
    /// limit of the scene drops nothing.
    pub fn draw(&self, scene: &mut Scene) {
        for (region, leaves) in self.runs() {
            let Some(region) = region else {
                leaves.into_iter().for_each(|leaf| leaf.add_to(scene));
                continue;
            };
            let Some((&(x, y), rest)) = region.split_first() else {
                continue;
            };
            let mut clip = ClipPath::builder(FillRule::default(), x as f32, y as f32);
            for &(x, y) in rest {
                clip = clip.line_to(x as f32, y as f32);
            }
            scene.clip(clip.build(), |inner| {
                leaves.into_iter().for_each(|leaf| leaf.add_to(inner));
            });
        }
    }

    /// The svg of `self` on its canvas, with each id after `s{number}-`. Two
    /// svgs in one HTML document need different numbers, or the clip of one
    /// can take the place of the clip of the other. The svg keeps each
    /// bitmap in the format of its image.
    pub fn to_svg(&self, number: usize) -> Result<String, TooLarge> {
        Ok(Self::svg(&self.to_scene()?, number))
    }

    /// [`Image::to_svg`] as a repl shows it, on a canvas with one more
    /// column and one more row, where an outline on the right or the bottom
    /// edge draws its line. DrRacket shows an image the same way.
    pub fn to_repl_svg(&self, number: usize) -> Result<String, TooLarge> {
        Ok(Self::svg(&self.to_repl_scene()?, number))
    }

    /// A scene of the size of [`Image::canvas_size`] with what `self`
    /// draws.
    pub fn to_scene(&self) -> Result<Scene, TooLarge> {
        let (w, h) = self.canvas_size()?;
        Ok(self.scene(w, h))
    }

    /// [`Image::to_scene`] on the canvas of [`Image::to_repl_svg`].
    pub fn to_repl_scene(&self) -> Result<Scene, TooLarge> {
        let (w, h) = self.repl_canvas_size()?;
        Ok(self.scene(w, h))
    }

    /// The canvas of [`Image::to_repl_svg`], one pixel wider and taller
    /// than [`Image::canvas_size`]. A repl that draws `self` on a scene of
    /// its own uses this size.
    pub fn repl_canvas_size(&self) -> Result<(f32, f32), TooLarge> {
        let (w, h) = self.canvas_size()?;
        Ok((w + 1.0, h + 1.0))
    }

    pub(super) fn scene(&self, width: f32, height: f32) -> Scene {
        let mut scene = Scene::new(width, height);
        self.draw(&mut scene);
        scene
    }

    fn svg(scene: &Scene, number: usize) -> String {
        let prefix = format!("s{number}-");
        let prefix = IdPrefix::new(&prefix).expect("s, digits and a dash make an id prefix");
        let mut renderer = SvgRenderer::with_id_prefix(prefix).keep_image_formats();
        let Ok(_) = renderer.render(scene);
        renderer.into_string()
    }

    /// The leaves of `self`, in runs that share a region, in drawing order.
    /// The nodes wait on a stack of their own, so a deep tree does not
    /// overflow the stack of the thread.
    fn runs(&self) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        let mut pending = vec![(self, Point::ORIGIN, Rc::new(None))];
        while let Some((image, at, region)) = pending.pop() {
            let leaf = match image.node() {
                Node::Combination { parts, .. } => {
                    // The stack gives back the last part first, and the
                    // first part draws first.
                    let parts = parts.iter().rev();
                    pending.extend(parts.map(|(part, p)| (part, at.plus(*p), Rc::clone(&region))));
                    None
                }
                Node::Crop {
                    region: r,
                    image,
                    at: inner,
                } => {
                    let inside = intersect(&region, &corners(affine(*r, at)));
                    pending.push((image, at.plus(*inner), Rc::new(inside)));
                    None
                }
                Node::Path { drawn: None, .. } => None,
                Node::Path {
                    style,
                    commands,
                    closed,
                    ..
                } => {
                    let style = Resolved::read(style);
                    style
                        .path_style(*closed)
                        .and_then(|s| path::draw(commands, s, pixel_point(&style, at)))
                        .map(Leaf::Path)
                }
                Node::Line { style, from, to } => {
                    let style = Resolved::read(style);
                    let point = pixel_point(&style, at);
                    let ((x, y), (to_x, to_y)) = (point(*from), point(*to));
                    style
                        .path_style(false)
                        .map(|s| Leaf::Path(Path::builder(s, x, y).line_to(to_x, to_y).build()))
                }
                Node::Text {
                    style,
                    region,
                    text,
                    font,
                } => draw_text(style, affine(*region, at), text, font).map(Leaf::Text),
                Node::Bitmap { region, image } => Some(Leaf::Bitmap(Bitmap {
                    image: image.clone(),
                    transform: affine(*region, at),
                    sampling: Sampling::Smooth,
                })),
            };
            let Some(leaf) = leaf else {
                continue;
            };
            if region.as_ref().as_ref().is_some_and(Vec::is_empty) {
                continue;
            }
            match runs.last_mut() {
                Some((last, leaves)) if last == region.as_ref() => leaves.push(leaf),
                _ => runs.push((region.as_ref().clone(), vec![leaf])),
            }
        }
        runs
    }
}

/// `v` in whole pixels, as `ceil(round(v, 6))` in Python, so a width that
/// rounding leaves just above a whole number takes no pixel more. As in
/// Python, `v` goes to a decimal with 6 digits after the point, correctly
/// rounded, and back to the nearest `f64`.
fn whole_pixels(v: f64) -> f64 {
    use std::io::Write;
    // 309 digits, the point and 6 decimals hold the largest f64.
    let mut buf = [0u8; 320];
    let mut rest = buf.as_mut_slice();
    write!(rest, "{v:.6}").expect("320 bytes hold a finite f64 with 6 decimals");
    let len = 320 - rest.len();
    let text = buf.get(..len).and_then(|b| std::str::from_utf8(b).ok());
    let rounded: f64 = text
        .and_then(|t| t.parse().ok())
        .expect("a formatted f64 parses");
    rounded.ceil()
}

/// The area that the crops around a leaf leave visible, as the corners of a
/// convex polygon. `None` is no crop, and no corners is an area with nothing
/// in it.
type Region = Option<Vec<(f64, f64)>>;

/// Leaves in a row that share a region.
type Run = (Region, Vec<Leaf>);

/// An element of the scene, before it goes into a clip.
enum Leaf {
    Path(Path),
    Text(Text),
    Bitmap(Bitmap),
}

impl Leaf {
    fn add_to(self, scene: &mut Scene) {
        match self {
            Leaf::Path(path) => scene.add_path(path),
            Leaf::Text(text) => scene.add_text(text),
            Leaf::Bitmap(bitmap) => scene.add_bitmap(bitmap),
        }
    }
}

/// The affine that takes the unit square centred on the origin to `region`
/// moved by `at`.
fn affine(region: ImageBox, at: Point) -> [f32; 6] {
    let v = |v: f64| without_residue(v) as f32;
    let (x, y, c) = (region.x_axis, region.y_axis, region.center.plus(at));
    [v(x.x), v(x.y), v(y.x), v(y.y), v(c.x), v(c.y)]
}

/// The corners of the box of `m`, in the convention of
/// [`ClipPath::from_transform`].
fn corners(m: [f32; 6]) -> [(f64, f64); 4] {
    let [a, b, c, d, e, f] = m.map(f64::from);
    [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)]
        .map(|(u, v)| (a * u + c * v + e, b * u + d * v + f))
}

/// How far a point may be outside a box and still count as inside, so a box
/// that matches the region up to rounding does not cut it.
const INSIDE_TOLERANCE: f64 = 1.0e-3;

/// The part of `region` inside the box with `corners`. A box with no area, or
/// one that is not finite, leaves nothing.
fn intersect(region: &Region, corners: &[(f64, f64); 4]) -> Region {
    let finite = corners.iter().all(|&(x, y)| x.is_finite() && y.is_finite());
    let area = cross(corners[0], corners[1], corners[2]);
    if !finite || area == 0.0 {
        return Some(Vec::new());
    }
    // The sign makes the distance to an edge positive inside the box,
    // whichever way its corners turn.
    let sign = area.signum();
    let edges = || {
        corners
            .iter()
            .copied()
            .zip(corners.iter().copied().cycle().skip(1))
    };
    let distance = |(p, q): ((f64, f64), (f64, f64)), r: (f64, f64)| {
        sign * cross(p, q, r) / (q.0 - p.0).hypot(q.1 - p.1)
    };
    let Some(region) = region else {
        return Some(corners.to_vec());
    };
    let contains = |r: (f64, f64)| edges().all(|e| distance(e, r) >= -INSIDE_TOLERANCE);
    if region.iter().all(|&r| contains(r)) {
        return Some(region.clone());
    }
    // Sutherland-Hodgman. The box is convex, so each of its edges cuts away
    // what lies outside it.
    let mut out = region.clone();
    for edge in edges() {
        let points = std::mem::take(&mut out);
        let sides = points
            .iter()
            .copied()
            .zip(points.iter().copied().cycle().skip(1));
        for (p, q) in sides {
            let (dp, dq) = (distance(edge, p), distance(edge, q));
            if dp >= 0.0 {
                out.push(p);
            }
            if (dp >= 0.0) != (dq >= 0.0) {
                let t = dp / (dp - dq);
                out.push((p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1)));
            }
        }
        // A corner on the edge comes back as itself and as the point where
        // the edge cuts the side.
        let same = |p: (f64, f64), q: (f64, f64)| (p.0 - q.0).hypot(p.1 - q.1) < INSIDE_TOLERANCE;
        out.dedup_by(|p, q| same(*p, *q));
        while let [first, .., last] = out[..]
            && same(first, last)
        {
            out.pop();
        }
    }
    if out.len() < 3 {
        out.clear();
    }
    Some(out)
}

/// Twice the signed area of the triangle `p`, `q`, `r`.
fn cross(p: (f64, f64), q: (f64, f64), r: (f64, f64)) -> f64 {
    (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
}

/// A point of a path with `style`, moved by `at`, for the scene. A thin
/// outline on a half pixel covers one row of pixels, and on a whole pixel it
/// blurs over two, so an aligned style puts the point on the center of its
/// pixel. The residue takes 9.999999999999998 as 10.
fn pixel_point(style: &Resolved, at: Point) -> impl Fn(Point) -> (f32, f32) {
    let aligned = style.aligned();
    let v = move |v: f64| {
        let v = without_residue(v);
        (if aligned {
            (v + RESIDUE).floor() + 0.5
        } else {
            v
        }) as f32
    };
    move |p: Point| (v(p.x + at.x), v(p.y + at.y))
}

/// The text of `text` in `font` with `style`, in the box of `transform`, or
/// `None` when it draws nothing.
fn draw_text(style: &Style, transform: [f32; 6], text: &str, font: &Font) -> Option<Text> {
    let style = Resolved::read(style).path_style(false)?;
    let mut text = font.spec(text).fit_transform(transform, style.into())?;
    text.underline = font.underline;
    Some(text)
}

/// The first value of each attribute of a [`Style`].
#[derive(Default)]
struct Resolved {
    fill: Option<Color>,
    fill_opacity: Option<f32>,
    fill_rule: Option<FillRule>,
    stroke: Option<Color>,
    stroke_width: Option<f32>,
    stroke_opacity: Option<f32>,
    dash: Option<Vec<f32>>,
    line_cap: Option<LineCap>,
    line_join: Option<LineJoin>,
}

impl Resolved {
    fn read(style: &Style) -> Self {
        let mut s = Self::default();
        for attribute in style.attributes() {
            match attribute {
                Attribute::Fill(c) => _ = s.fill.get_or_insert(*c),
                Attribute::FillOpacity(o) => _ = s.fill_opacity.get_or_insert(o.get() as f32),
                Attribute::FillRule(r) => _ = s.fill_rule.get_or_insert(*r),
                Attribute::Stroke(c) => _ = s.stroke.get_or_insert(*c),
                Attribute::StrokeWidth(w) => _ = s.stroke_width.get_or_insert(w.get() as f32),
                Attribute::StrokeOpacity(o) => _ = s.stroke_opacity.get_or_insert(o.get() as f32),
                Attribute::StrokeDashArray(values) => {
                    _ = s
                        .dash
                        .get_or_insert_with(|| values.iter().map(|v| v.get() as f32).collect());
                }
                Attribute::StrokeLineCap(c) => _ = s.line_cap.get_or_insert(*c),
                Attribute::StrokeLineJoin(j) => _ = s.line_join.get_or_insert(*j),
            }
        }
        s
    }

    /// Returns `true` if the path draws a thin outline on half pixels,
    /// `false` otherwise. A width of 1 is the width an outline has by
    /// default, so it draws the same.
    fn aligned(&self) -> bool {
        self.stroke.is_some() && self.fill.is_none() && self.stroke_width.is_none_or(|w| w == 1.0)
    }

    /// The style of a path, with a closed stroke if `closed`, or `None` if
    /// the path marks nothing.
    fn path_style(&self, closed: bool) -> Option<PathStyle> {
        let outline = self.stroke.is_some();
        // A shape with no fill is black, as in svg, and an outline is not
        // filled.
        let fill = match self.fill {
            Some(c) => c,
            None if outline => Color::NONE,
            None => Color::rgb(0, 0, 0),
        };
        let stroke = self.stroke.and_then(|c| {
            Some(PathStroke {
                paint: paint(c, self.stroke_opacity),
                width: StrokeWidth::new(self.stroke_width.unwrap_or(1.0))?,
                cap: self.line_cap.unwrap_or(LineCap::Round),
                // An outline with no join of its own gets a limit of 10.
                join: self
                    .line_join
                    .unwrap_or(LineJoin::Miter(OUTLINE_MITER_LIMIT)),
                dash: self.dash.clone().and_then(|a| Dash::new(a, 0.0)),
                closed,
            })
        });
        let fill = PathFill {
            paint: paint(fill, self.fill_opacity),
            rule: self.fill_rule.unwrap_or(FillRule::NonZero),
        };
        PathStyle::new(fill.paint.is_visible().then_some(fill), stroke)
    }
}

/// The miter limit of an outline with no join of its own.
const OUTLINE_MITER_LIMIT: MiterLimit = MiterLimit::new(10.0).unwrap();

/// `c` with its alpha times `opacity`, rounded once to the byte of the
/// scene.
fn paint(c: Color, opacity: Option<f32>) -> Paint {
    let a = (c.a.get() as f32 * opacity.unwrap_or(1.0)).clamp(0.0, 1.0) * 255.0;
    Paint::rgba(c.r, c.g, c.b, a.round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Finite, Opacity, PathCmd, YPlace};
    use crate::scene::{Element, TextStyle};

    fn length(v: f64) -> Length {
        Length::new(v).unwrap()
    }

    fn finite(v: f64) -> Finite {
        Finite::new(v).unwrap()
    }

    fn red() -> Color {
        Color::rgb(255, 0, 0)
    }

    fn solid(w: f64, h: f64) -> Image {
        Image::rectangle(length(w), length(h), Style::new([Attribute::Fill(red())]))
    }

    fn outline(w: f64, h: f64, width: Option<f64>) -> Image {
        let mut attributes = vec![Attribute::Stroke(red())];
        attributes.extend(width.map(|w| Attribute::StrokeWidth(length(w))));
        Image::rectangle(length(w), length(h), Style::new(attributes))
    }

    fn svg(image: &Image) -> String {
        image.to_svg(0).unwrap()
    }

    fn scene(image: &Image) -> Scene {
        let mut scene = Scene::new(100.0, 100.0);
        image.draw(&mut scene);
        scene
    }

    #[test]
    fn the_canvas_rounds_up_to_whole_pixels() {
        let size = |w: f64| solid(w, 1.0).canvas_size().unwrap().0;
        assert_eq!(size(10.4), 11.0);
        assert_eq!(size(10.0), 10.0);
        assert_eq!(size(10.0000004), 10.0);
        assert_eq!(size(10.0000006), 11.0);
        assert_eq!(size(9.9999999), 10.0);
        assert_eq!(size(0.0), 0.0);
        assert_eq!(whole_pixels(f64::MAX), f64::MAX);
        // The f64 of 10 + 5e-7 is above it, and the f64 of 5e-7 below it.
        assert_eq!(whole_pixels(10.0 + 5e-7), 11.0);
        assert_eq!(whole_pixels((10.0 + 5e-7_f64).next_down()), 10.0);
        assert_eq!(whole_pixels(5e-7), 0.0);
        assert_eq!(whole_pixels(5e-7_f64.next_up()), 1.0);
        let circle = Image::circle(length(5.2), Style::default()).unwrap();
        assert_eq!(circle.canvas_size(), Ok((11.0, 11.0)));
    }

    #[test]
    fn a_canvas_above_the_largest_f32_is_too_large() {
        assert_eq!(solid(3e38, 1.0).canvas_size(), Ok((3e38, 1.0)));
        assert_eq!(solid(1.0, 1e39).canvas_size(), Err(TooLarge));
    }

    #[test]
    fn a_thin_outline_sits_on_half_pixels() {
        let thin = svg(&outline(10.0, 5.0, None));
        assert!(
            thin.contains(r#"d="M0.5 0.5 L10.5 0.5 L10.5 5.5 L0.5 5.5 Z""#),
            "{thin}"
        );
        assert_eq!(thin, svg(&outline(10.0, 5.0, Some(1.0))));
        let wide = svg(&outline(10.0, 5.0, Some(2.0)));
        assert!(wide.contains(r#"d="M0 0 L10 0 L10 5 L0 5 Z""#), "{wide}");
        let line = Image::line(
            finite(10.0),
            finite(0.0),
            Style::new([Attribute::Stroke(red())]),
        );
        assert!(svg(&line).contains(r#"d="M0.5 0.5 L10.5 0.5""#));
    }

    #[test]
    fn the_parts_draw_in_order_at_their_places() {
        let pair = Image::beside(YPlace::Top, [solid(5.0, 5.0), solid(10.0, 2.0)]).unwrap();
        let pair = svg(&pair);
        let first = pair.find(r#"d="M0 0 L5 0 L5 5 L0 5""#).unwrap();
        let second = pair.find(r#"d="M5 0 L15 0 L15 2 L5 2""#).unwrap();
        assert!(first < second, "{pair}");
    }

    #[test]
    fn a_crop_clips_what_it_holds() {
        let cropped = solid(10.0, 5.0).crop(finite(2.0), finite(1.0), length(4.0), length(3.0));
        let cropped = svg(&cropped);
        assert!(cropped.contains(r#"<path d="M0 0 L4 0 L4 3 L0 3"/></clipPath>"#));
        assert!(
            cropped.contains(r#"d="M-2 -1 L8 -1 L8 4 L-2 4""#),
            "{cropped}"
        );
    }

    #[test]
    fn a_crop_inside_a_crop_is_one_clip() {
        let inner = solid(10.0, 10.0).crop(finite(0.0), finite(0.0), length(6.0), length(6.0));
        let outer = inner.crop(finite(3.0), finite(2.0), length(5.0), length(5.0));
        let outer = svg(&outer);
        assert_eq!(outer.matches("<clipPath").count(), 1, "{outer}");
        assert!(
            outer.contains(r#"<path d="M0 0 L3 0 L3 4 L0 4"/></clipPath>"#),
            "{outer}"
        );
    }

    #[test]
    fn a_crop_with_no_area_draws_nothing() {
        let cropped = solid(10.0, 10.0).crop(finite(0.0), finite(0.0), length(0.0), length(5.0));
        assert!(scene(&cropped).elements().is_empty());
    }

    #[test]
    fn a_deep_chain_of_crops_draws() {
        let chain = (0..200_000).fold(solid(10.0, 10.0), |img, _| {
            img.crop(finite(0.0), finite(0.0), length(10.0), length(10.0))
        });
        assert_eq!(scene(&chain).elements().len(), 1);
    }

    #[test]
    fn a_path_that_draws_nothing_adds_nothing() {
        let no_move = Image::path([PathCmd::line_to(Point::ORIGIN)], false, Style::default());
        assert!(scene(&no_move.unwrap()).elements().is_empty());
    }

    #[test]
    fn a_residue_of_a_rotation_draws_as_zero() {
        let turned = outline(10.0, 5.0, Some(2.0)).rotate(finite(90.0)).unwrap();
        let turned = svg(&turned);
        assert!(
            turned.contains(r#"d="M0 10 L0 0 L5 0 L5 10 Z""#),
            "{turned}"
        );
    }

    #[test]
    fn the_first_attribute_of_a_kind_wins() {
        let style = Style::new([
            Attribute::Fill(red()),
            Attribute::FillOpacity(Opacity::new(0.5).unwrap()),
            Attribute::Fill(Color::rgb(0, 0, 255)),
            Attribute::FillOpacity(Opacity::OPAQUE),
        ]);
        let path_style = Resolved::read(&style).path_style(false).unwrap();
        assert_eq!(
            path_style.fill().unwrap().paint,
            Paint::rgba(255, 0, 0, 128)
        );
        assert_eq!(path_style.stroke(), None);
    }

    #[test]
    fn an_opacity_multiplies_the_alpha_once() {
        let half = Opacity::new(0.5).unwrap();
        let style = Style::new([
            Attribute::Stroke(Color::rgba(255, 0, 0, half)),
            Attribute::StrokeOpacity(half),
        ]);
        let path_style = Resolved::read(&style).path_style(false).unwrap();
        let stroke = path_style.stroke().unwrap();
        assert_eq!(stroke.paint, Paint::rgba(255, 0, 0, 64));
        assert_eq!(path_style.fill(), None);
        assert_eq!(stroke.cap, LineCap::Round);
        assert_eq!(stroke.join, LineJoin::Miter(OUTLINE_MITER_LIMIT));
    }

    #[test]
    fn a_text_takes_the_paint_and_the_underline() {
        let font = Font {
            underline: true,
            ..Font::default()
        };
        let text = Image::text("hi", font, Style::new([Attribute::Fill(red())])).unwrap();
        let scene = scene(&text);
        let [Element::Text(text)] = scene.elements() else {
            panic!("{:?}", scene.elements());
        };
        assert!(text.underline);
        assert_eq!(text.style, TextStyle::Fill(Paint::rgba(255, 0, 0, 255)));
        assert_eq!(text.spec.text, "hi");
    }

    #[test]
    fn a_bitmap_fills_its_box() {
        let png = crate::asset::encode_data_uri("image/png", &crate::asset::png_head(3, 2));
        let bitmap = Image::bitmap_data_uri(&png).unwrap();
        let scene = scene(&bitmap);
        let [Element::Bitmap(bitmap)] = scene.elements() else {
            panic!("{:?}", scene.elements());
        };
        assert_eq!(bitmap.transform, [3.0, 0.0, 0.0, 2.0, 1.5, 1.0]);
    }

    #[test]
    fn each_id_starts_with_the_number() {
        let cropped = solid(10.0, 5.0).crop(finite(2.0), finite(1.0), length(4.0), length(3.0));
        let cropped = cropped.to_svg(7).unwrap();
        assert!(cropped.contains(r#"<clipPath id="s7-c0">"#), "{cropped}");
        assert!(cropped.contains("url(#s7-c0)"), "{cropped}");
    }

    #[test]
    fn a_repl_shows_one_more_column_and_row() {
        let repl = solid(10.5, 4.0).to_repl_svg(0).unwrap();
        assert!(repl.contains(r#"width="12" height="5""#), "{repl}");
        let (w, h) = (Length::new(1e300).unwrap(), length(1.0));
        let huge = Image::rectangle(w, h, Style::default());
        assert_eq!(huge.to_svg(0), Err(TooLarge));
        assert_eq!(huge.to_repl_svg(0), Err(TooLarge));
    }
}
