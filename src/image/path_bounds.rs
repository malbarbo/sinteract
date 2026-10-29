//! The bounds of a path in f64.

use kurbo::ParamCurveExtrema;

use crate::scene::{ArcSize, Sweep, arc_cubics};

/// The bounds of a path in f64, with more precision than the f32 of a
/// [`Path`](crate::scene::Path). It takes the commands of a
/// [`PathBuilder`](crate::scene::PathBuilder) and covers the segments that
/// the builder draws, with each arc as the same cubics, so a move that no
/// segment leaves adds nothing.
#[derive(Clone, Debug)]
#[must_use = "PathBounds yields the bounds only when get() is called"]
pub(super) struct PathBounds {
    rect: Option<kurbo::Rect>,
    last_point: kurbo::Point,
}

impl PathBounds {
    pub fn new(x: f64, y: f64) -> Self {
        Self {
            rect: None,
            last_point: kurbo::Point::new(x, y),
        }
    }

    pub fn move_to(mut self, x: f64, y: f64) -> Self {
        self.last_point = kurbo::Point::new(x, y);
        self
    }

    pub fn line_to(self, x: f64, y: f64) -> Self {
        let to = kurbo::Point::new(x, y);
        let rect = kurbo::Rect::from_points(self.last_point, to);
        self.add(rect, to)
    }

    pub fn quad_to(self, cx: f64, cy: f64, x: f64, y: f64) -> Self {
        let to = kurbo::Point::new(x, y);
        let points = [self.last_point, kurbo::Point::new(cx, cy), to];
        let bounds = curve_bounds(points, |[p0, p1, p2]| kurbo::QuadBez::new(p0, p1, p2));
        self.add(bounds, to)
    }

    pub fn cubic_to(self, c1x: f64, c1y: f64, c2x: f64, c2y: f64, x: f64, y: f64) -> Self {
        let to = kurbo::Point::new(x, y);
        let points = [
            self.last_point,
            kurbo::Point::new(c1x, c1y),
            kurbo::Point::new(c2x, c2y),
            to,
        ];
        self.add(curve_bounds(points, cubic), to)
    }

    /// Add an SVG endpoint arc as the cubics of
    /// [`PathBuilder::arc_to`](crate::scene::PathBuilder::arc_to). A
    /// degenerate arc is a line.
    #[allow(clippy::too_many_arguments)]
    pub fn arc_to(
        mut self,
        rx: f64,
        ry: f64,
        rotation_deg: f64,
        size: ArcSize,
        sweep: Sweep,
        x: f64,
        y: f64,
    ) -> Self {
        let to = kurbo::Point::new(x, y);
        let radii = kurbo::Vec2::new(rx, ry);
        let Some(cubics) = arc_cubics(self.last_point, radii, rotation_deg, size, sweep, to) else {
            return self.line_to(x, y);
        };
        for [c1, c2, end] in cubics {
            let bounds = curve_bounds([self.last_point, c1, c2, end], cubic);
            self = self.add(bounds, end);
        }
        self.last_point = to;
        self
    }

    /// The bounds of the path, or `None` when it draws no segment.
    pub fn get(&self) -> Option<kurbo::Rect> {
        self.rect
    }

    fn add(mut self, rect: kurbo::Rect, to: kurbo::Point) -> Self {
        self.rect = Some(self.rect.map_or(rect, |r| r.union(rect)));
        self.last_point = to;
        self
    }
}

fn cubic([p0, p1, p2, p3]: [kurbo::Point; 4]) -> kurbo::CubicBez {
    kurbo::CubicBez::new(p0, p1, p2, p3)
}

/// The bounds of the curve that `curve` makes of `points`. kurbo loses the
/// extremes when the differences of the points overflow near f64::MAX or
/// lose their bits among the subnormals. Outside a range far from both, the
/// points go through a power of two that brings the largest one near 1,
/// which keeps every bit of it, and the ends join the bounds as they are.
fn curve_bounds<C: ParamCurveExtrema, const N: usize>(
    points: [kurbo::Point; N],
    curve: impl Fn([kurbo::Point; N]) -> C,
) -> kurbo::Rect {
    const SAFE: std::ops::RangeInclusive<f64> = pow2(-300)..=pow2(300);
    let largest = points
        .iter()
        .fold(0.0_f64, |m, p| m.max(p.x.abs()).max(p.y.abs()));
    if largest == 0.0 || SAFE.contains(&largest) || !largest.is_finite() {
        return curve(points).bounding_box();
    }
    let e = exponent(largest);
    let ends = curve(points);
    let ends = kurbo::Rect::from_points(ends.start(), ends.end());
    let rect = curve(points.map(|p| times_pow2(p, -e))).bounding_box();
    let (p0, p1) = (rect.origin(), kurbo::Point::new(rect.x1, rect.y1));
    kurbo::Rect::from_points(times_pow2(p0, e), times_pow2(p1, e)).union(ends)
}

/// 2 to the power `e`, for `e` from -1022 to 1023.
const fn pow2(e: i32) -> f64 {
    f64::from_bits(((e + 1023) as u64) << 52)
}

/// The exponent of `v` in base 2, for a finite `v` other than 0.
fn exponent(v: f64) -> i32 {
    if v.is_normal() {
        ((v.to_bits() >> 52) & 0x7ff) as i32 - 1023
    } else {
        exponent(v * pow2(600)) - 600
    }
}

/// `p` times 2 to the power `e`, for `e` from -2044 to 2046. Two factors
/// cover the range, and each one is exact while the result stays normal.
fn times_pow2(p: kurbo::Point, e: i32) -> kurbo::Point {
    let (a, b) = (pow2(e / 2), pow2(e - e / 2));
    kurbo::Point::new(p.x * a * b, p.y * a * b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Path, PathStyle, Segment};

    #[test]
    fn an_arc_of_a_radius_far_above_its_chord_keeps_its_shape() {
        for r in [1e15, 1e17, 1e20, 1e100, 1e200, 1e300] {
            let arc = |size| {
                PathBounds::new(0.0, 0.0)
                    .arc_to(r, r, 0.0, size, Sweep::Clockwise, 10.0, 0.0)
                    .get()
                    .map(corners)
                    .unwrap()
            };
            let (x0, y0, x1, y1) = arc(ArcSize::Small);
            let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
            assert!(near(x0, 0.0) && near(y0, 0.0) && near(x1, 10.0) && near(y1, 0.0));
            let (x0, y0, x1, y1) = arc(ArcSize::Large);
            let near = |a: f64, b: f64| (a - b).abs() <= r * 1e-12;
            assert!(near(x0, -r) && near(y0, -2.0 * r) && near(x1, r) && near(y1, 0.0));
        }
    }

    #[test]
    fn an_arc_with_a_subnormal_reach_keeps_its_shape() {
        let arc = |size| {
            PathBounds::new(0.0, 0.0)
                .arc_to(1e300, 1e300, 0.0, size, Sweep::Clockwise, 1e-10, 0.0)
                .get()
                .map(corners)
                .unwrap()
        };
        assert_eq!(arc(ArcSize::Small), (0.0, 0.0, 1e-10, 0.0));
        let (x0, y0, x1, y1) = arc(ArcSize::Large);
        let near = |a: f64, b: f64| (a - b).abs() <= 1e288;
        assert!(near(x0, -1e300) && near(y0, -2e300) && near(x1, 1e300) && near(y1, 0.0));
    }

    #[test]
    fn a_curve_far_from_1_keeps_its_extremes() {
        // Near f64::MAX the differences of the points overflow, and in the
        // subnormals they lose their bits.
        let unit = [(0.0, 0.0), (-1.0, 0.3), (1.7, 0.9), (0.4, -0.2)];
        let [p0, p1, p2, p3] = unit.map(|(x, y)| kurbo::Point::new(x, y));
        let cubic_rect = kurbo::CubicBez::new(p0, p1, p2, p3).bounding_box();
        let quad_rect = kurbo::QuadBez::new(p0, p1, p3).bounding_box();
        for s in [f64::MAX / 2.0, 1e-310] {
            let [p0, p1, p2, p3] = [p0, p1, p2, p3].map(|p| (p.to_vec2() * s).to_point());
            let near = |bounds: Option<(f64, f64, f64, f64)>, r: kurbo::Rect| {
                let (x0, y0, x1, y1) = bounds.unwrap();
                let near = |a: f64, b: f64| (a - b * s).abs() <= s * 1e-9;
                near(x0, r.x0) && near(y0, r.y0) && near(x1, r.x1) && near(y1, r.y1)
            };
            let cubic = PathBounds::new(p0.x, p0.y).cubic_to(p1.x, p1.y, p2.x, p2.y, p3.x, p3.y);
            assert!(near(cubic.get().map(corners), cubic_rect));
            let quad = PathBounds::new(p0.x, p0.y).quad_to(p1.x, p1.y, p3.x, p3.y);
            assert!(near(quad.get().map(corners), quad_rect));
        }
    }

    #[test]
    fn a_curve_near_1_has_the_bounds_of_kurbo() {
        let [p0, p1, p2, p3] = [(3.0, 7.0), (-40.0, 900.0), (650.0, -0.25), (12.5, 30.0)]
            .map(|(x, y)| kurbo::Point::new(x, y));
        let rect = kurbo::CubicBez::new(p0, p1, p2, p3).bounding_box();
        let bounds = PathBounds::new(p0.x, p0.y)
            .cubic_to(p1.x, p1.y, p2.x, p2.y, p3.x, p3.y)
            .get()
            .map(corners);
        assert_eq!(bounds, Some((rect.x0, rect.y0, rect.x1, rect.y1)));
    }

    #[test]
    fn an_arc_too_large_for_kurbo_keeps_its_shape() {
        // The radii squared overflow, and so do the ends added together.
        let half = PathBounds::new(0.0, 0.0)
            .arc_to(1e80, 1e80, 0.0, ArcSize::Small, Sweep::Clockwise, 2e80, 0.0)
            .get()
            .map(corners);
        let x = f64::MAX * 0.6;
        let far = PathBounds::new(x, 0.0)
            .arc_to(10.0, 10.0, 0.0, ArcSize::Small, Sweep::Clockwise, x, 20.0)
            .get()
            .map(corners);
        let (x0, y0, x1, y1) = half.unwrap();
        assert!(x0.abs() < 1e68 && (x1 - 2e80).abs() < 1e68);
        assert!((y0 + 1e80).abs() < 1e68 && y1.abs() < 1e68);
        let (x0, y0, x1, y1) = far.unwrap();
        let near_x = |v: f64| (v - x).abs() <= x * f64::EPSILON;
        assert!(near_x(x0) && near_x(x1) && y0.abs() < 1e-12 && (y1 - 20.0).abs() < 1e-12);
    }

    fn corners(r: kurbo::Rect) -> (f64, f64, f64, f64) {
        (r.x0, r.y0, r.x1, r.y1)
    }

    /// The bounds of the f32 segments of `path`.
    fn segment_bounds(path: &Path) -> Option<(f64, f64, f64, f64)> {
        let f = f64::from;
        let mut segs = path.segments();
        let Some(Segment::Move { x, y }) = segs.next() else {
            return None;
        };
        let mut bounds = PathBounds::new(f(x), f(y));
        for seg in segs {
            bounds = match seg {
                Segment::Move { x, y } => bounds.move_to(f(x), f(y)),
                Segment::Line { x, y } => bounds.line_to(f(x), f(y)),
                Segment::Quad { cx, cy, x, y } => bounds.quad_to(f(cx), f(cy), f(x), f(y)),
                Segment::Cubic {
                    c1x,
                    c1y,
                    c2x,
                    c2y,
                    x,
                    y,
                } => bounds.cubic_to(f(c1x), f(c1y), f(c2x), f(c2y), f(x), f(y)),
            };
        }
        bounds.get().map(corners)
    }

    #[test]
    fn path_bounds_leave_out_a_move_that_no_segment_leaves() {
        let bounds = PathBounds::new(-40.0, 0.0)
            .move_to(0.0, 0.0)
            .line_to(10.0, 5.0)
            .move_to(50.0, 50.0);
        assert_eq!(bounds.get().map(corners), Some((0.0, 0.0, 10.0, 5.0)));
        assert_eq!(PathBounds::new(5.0, 5.0).move_to(9.0, 9.0).get(), None);
    }

    #[test]
    fn path_bounds_take_an_arc_with_a_nan_radius_as_a_line() {
        for size in [ArcSize::Small, ArcSize::Large] {
            let bounds = PathBounds::new(0.0, 0.0)
                .arc_to(f64::NAN, 1.0, 0.0, size, Sweep::Clockwise, 10.0, 0.0)
                .line_to(10.0, 5.0);
            assert_eq!(bounds.get().map(corners), Some((0.0, 0.0, 10.0, 5.0)));
        }
    }

    #[test]
    fn path_bounds_cover_the_extremes_of_a_curve() {
        let quad = PathBounds::new(0.0, 0.0).quad_to(5.0, 10.0, 10.0, 0.0);
        assert_eq!(quad.get().map(corners), Some((0.0, 0.0, 10.0, 5.0)));
        let cubic = PathBounds::new(0.0, 0.0).cubic_to(0.0, -8.0, 10.0, -8.0, 10.0, 0.0);
        assert_eq!(cubic.get().map(corners), Some((0.0, -6.0, 10.0, 0.0)));
    }

    #[test]
    fn path_bounds_of_an_arc_fit_the_cubics_that_the_builder_draws() {
        let arcs = [
            (10.0, 10.0, 0.0, ArcSize::Small, Sweep::Clockwise, 0.0, 10.0),
            (
                30.0,
                10.0,
                30.0,
                ArcSize::Large,
                Sweep::Counterclockwise,
                5.0,
                40.0,
            ),
            (1e9, 1e9, 0.0, ArcSize::Small, Sweep::Clockwise, 3.0, 4.0),
            (0.0, 10.0, 0.0, ArcSize::Small, Sweep::Clockwise, 7.0, 7.0),
        ];
        for (rx, ry, rotation, size, sweep, x, y) in arcs {
            let path = Path::builder(PathStyle::black(), 20.0, 10.0)
                .arc_to(rx, ry, rotation, size, sweep, x, y)
                .build();
            let bounds = PathBounds::new(20.0, 10.0)
                .arc_to(
                    f64::from(rx),
                    f64::from(ry),
                    f64::from(rotation),
                    size,
                    sweep,
                    f64::from(x),
                    f64::from(y),
                )
                .get()
                .map(corners)
                .unwrap();
            let drawn = segment_bounds(&path).unwrap();
            let (b, d) = (
                [bounds.0, bounds.1, bounds.2, bounds.3],
                [drawn.0, drawn.1, drawn.2, drawn.3],
            );
            for (b, d) in b.into_iter().zip(d) {
                assert!(
                    (b - d).abs() <= 1e-3 * d.abs().max(1.0),
                    "{bounds:?} {drawn:?}"
                );
            }
        }
    }
}
