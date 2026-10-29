//! The shapes that a program draws from measures: rectangles, ellipses,
//! triangles, polygons, stars and wedges.

use std::f64::consts::PI;
use std::fmt;
use std::num::NonZeroU32;

use super::geometry::Point;
use super::path::PathCmd;
use super::tree::{Image, Overflow};
use super::{Finite, Length, PointCount, SideCount, Style, TriangleAngle, WedgeAngle};
use crate::scene::{ArcSize, Sweep};

/// `side * sqrt(3) / 2` is the height of an equilateral triangle.
const SQRT3_2: f64 = 0.8660254037844386;

/// The error of a triangle from its three sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriangleSidesError {
    /// A side is longer than the sum of the other two.
    TooLong,
    Overflow,
}

impl fmt::Display for TriangleSidesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TriangleSidesError::TooLong => {
                f.write_str("a side is longer than the sum of the other two")
            }
            TriangleSidesError::Overflow => Overflow.fmt(f),
        }
    }
}

impl std::error::Error for TriangleSidesError {}

impl From<Overflow> for TriangleSidesError {
    fn from(_: Overflow) -> Self {
        TriangleSidesError::Overflow
    }
}

/// The error of a triangle from two of its angles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AngleSumError {
    /// The two angles add up to 180 or more, which leaves no third angle.
    AtLeast180,
    Overflow,
}

impl fmt::Display for AngleSumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AngleSumError::AtLeast180 => f.write_str("the two angles add up to 180 or more"),
            AngleSumError::Overflow => Overflow.fmt(f),
        }
    }
}

impl std::error::Error for AngleSumError {}

impl From<Overflow> for AngleSumError {
    fn from(_: Overflow) -> Self {
        AngleSumError::Overflow
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StarPolygonError {
    /// The side count and the step count have a common divisor, so the
    /// star visits only some of the vertices.
    CommonDivisor,
    Overflow,
}

impl fmt::Display for StarPolygonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StarPolygonError::CommonDivisor => {
                f.write_str("the side count and the step count have a common divisor")
            }
            StarPolygonError::Overflow => Overflow.fmt(f),
        }
    }
}

impl std::error::Error for StarPolygonError {}

impl From<Overflow> for StarPolygonError {
    fn from(_: Overflow) -> Self {
        StarPolygonError::Overflow
    }
}

impl Image {
    pub fn rectangle(w: Length, h: Length, style: Style) -> Self {
        let (w, h) = (w.get(), h.get());
        let corners = [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)];
        polygon(corners.map(|(x, y)| Point { x, y }), style)
            .expect("the corners of a rectangle are finite")
    }

    pub fn square(side: Length, style: Style) -> Self {
        Image::rectangle(side, side, style)
    }

    pub fn ellipse(w: Length, h: Length, style: Style) -> Result<Self, Overflow> {
        let (w, h) = (w.get(), h.get());
        let (rx, ry) = (w / 2.0, h / 2.0);
        let arc = |x, y| {
            PathCmd::arc(
                rx,
                ry,
                0.0,
                ArcSize::Small,
                Sweep::Clockwise,
                Point { x, y },
            )
        };
        // A quarter arc ends away from where it starts even when the width
        // or the height is 0, so the bounds keep the other side.
        let commands = [arc(rx, h), arc(0.0, ry), arc(rx, 0.0), arc(w, ry)];
        Image::closed_path(Point { x: w, y: ry }, commands, style)
    }

    pub fn circle(radius: Length, style: Style) -> Result<Self, Overflow> {
        let d = 2.0 * radius.get();
        let d = Length::new(d).map_err(|_| Overflow)?;
        Image::ellipse(d, d, style)
    }

    /// The equilateral triangle with a horizontal base.
    pub fn triangle(side: Length, style: Style) -> Self {
        let side = side.get();
        let h = side * SQRT3_2;
        let corners = [(side / 2.0, 0.0), (side, h), (0.0, h)];
        polygon(corners.map(|(x, y)| Point { x, y }), style)
            .expect("the corners of a triangle with a finite side are finite")
    }

    /// The triangle with legs of `side1`, horizontal, and `side2`,
    /// vertical, with the right angle at the bottom left.
    pub fn right_triangle(side1: Length, side2: Length, style: Style) -> Self {
        let (s1, s2) = (side1.get(), side2.get());
        let corners = [(0.0, 0.0), (0.0, s2), (s1, s2)];
        polygon(corners.map(|(x, y)| Point { x, y }), style)
            .expect("the corners of a right triangle are finite")
    }

    /// The triangle with two sides of `side` and the angle `angle` between
    /// them. The angle goes modulo 360, as in 2htdp/image, and an angle
    /// above 180 puts the base above the vertex. An angle of 0 gives a line.
    pub fn isosceles_triangle(side: Length, angle: Finite, style: Style) -> Result<Self, Overflow> {
        let side = side.get();
        // `rem_euclid` is exact.
        let half = angle.get().rem_euclid(360.0) / 2.0;
        let (x, y) = (side * sin_deg(half), side * cos_deg(half));
        polygon([Point { x, y }, Point::ORIGIN, Point { x: -x, y }], style)
    }

    /// The triangle with sides `a`, `b` and `c`. Sides where one is the sum
    /// of the other two make a flat triangle.
    pub fn triangle_sss(
        a: Length,
        b: Length,
        c: Length,
        style: Style,
    ) -> Result<Self, TriangleSidesError> {
        let (a, b, c) = (a.get(), b.get(), c.get());
        if a > b + c || b > a + c || c > a + b {
            return Err(TriangleSidesError::TooLong);
        }
        Ok(two_sides_and_angle(b, c, solve_angle(a, b, c), style)?)
    }

    /// The triangle with sides `a` and `c` and the angle `angle_b` between
    /// them.
    pub fn triangle_sas(
        a: Length,
        angle_b: TriangleAngle,
        c: Length,
        style: Style,
    ) -> Result<Self, Overflow> {
        let (a, c) = (a.get(), c.get());
        let b = solve_side(a, c, angle_b.get());
        two_sides_and_angle(b, c, solve_angle(a, b, c), style)
    }

    /// The triangle with sides `a` and `b` and the angle `angle_c` between
    /// them.
    pub fn triangle_ssa(
        a: Length,
        b: Length,
        angle_c: TriangleAngle,
        style: Style,
    ) -> Result<Self, Overflow> {
        let (a, b) = (a.get(), b.get());
        let c = solve_side(a, b, angle_c.get());
        two_sides_and_angle(b, c, solve_angle(a, b, c), style)
    }

    /// The triangle with the angles `angle_a` and `angle_b` and the side `c`
    /// opposite the third angle.
    pub fn triangle_aas(
        angle_a: TriangleAngle,
        angle_b: TriangleAngle,
        c: Length,
        style: Style,
    ) -> Result<Self, AngleSumError> {
        let (angle_a, angle_b) = (angle_a.get(), angle_b.get());
        let angle_c = third_angle(angle_a, angle_b)?;
        let b = law_of_sines(c.get(), angle_c, angle_b);
        Ok(two_sides_and_angle(b, c.get(), angle_a, style)?)
    }

    /// The triangle with sides `b` and `c` and the angle `angle_a` between
    /// them.
    pub fn triangle_ass(
        angle_a: TriangleAngle,
        b: Length,
        c: Length,
        style: Style,
    ) -> Result<Self, Overflow> {
        two_sides_and_angle(b.get(), c.get(), angle_a.get(), style)
    }

    /// The triangle with the side `b` and the angles `angle_a` and `angle_c`
    /// at its ends.
    pub fn triangle_asa(
        angle_a: TriangleAngle,
        b: Length,
        angle_c: TriangleAngle,
        style: Style,
    ) -> Result<Self, AngleSumError> {
        let (angle_a, angle_c) = (angle_a.get(), angle_c.get());
        let angle_b = third_angle(angle_a, angle_c)?;
        let c = law_of_sines(b.get(), angle_b, angle_c);
        Ok(two_sides_and_angle(b.get(), c, angle_a, style)?)
    }

    /// The triangle with the side `a` and the angles `angle_b` and `angle_c`
    /// at its ends.
    pub fn triangle_saa(
        a: Length,
        angle_b: TriangleAngle,
        angle_c: TriangleAngle,
        style: Style,
    ) -> Result<Self, AngleSumError> {
        let (angle_b, angle_c) = (angle_b.get(), angle_c.get());
        let angle_a = third_angle(angle_b, angle_c)?;
        let b = law_of_sines(a.get(), angle_a, angle_b);
        let c = law_of_sines(a.get(), angle_a, angle_c);
        Ok(two_sides_and_angle(b, c, angle_a, style)?)
    }

    /// The rhombus with sides of `side` and the angle `angle` at the left
    /// and at the right. The angle goes modulo 360, and an angle `a` above
    /// 180 gives the rhombus of `360 - a`, as in 2htdp/image. An angle of 0
    /// or 180 gives a line.
    pub fn rhombus(side: Length, angle: Finite, style: Style) -> Result<Self, Overflow> {
        // `rem_euclid` is exact, and so is `360 - a` for `a` in (180, 360].
        let angle = angle.get().rem_euclid(360.0);
        let angle = if angle > 180.0 { 360.0 - angle } else { angle };
        let side = side.get();
        let h = 2.0 * side * cos_deg(angle / 2.0);
        let w = 2.0 * side * sin_deg(angle / 2.0);
        let corners = [(0.0, h / 2.0), (w / 2.0, 0.0), (w, h / 2.0), (w / 2.0, h)];
        polygon(corners.map(|(x, y)| Point { x, y }), style)
    }

    pub fn regular_polygon(
        side: Length,
        side_count: SideCount,
        style: Style,
    ) -> Result<Self, Overflow> {
        star_polygon(side, u64::from(side_count.get()), 1, style)
    }

    /// The polygon through `points`. Fewer than three points make an open
    /// path.
    pub fn polygon(points: &[Point], style: Style) -> Result<Self, Overflow> {
        polygon(points.iter().copied(), style)
    }

    /// The polygon that joins every `step_count`-th of `side_count`
    /// vertices on a circle, with sides of `side` when `step_count` is 1.
    pub fn star_polygon(
        side: Length,
        side_count: SideCount,
        step_count: NonZeroU32,
        style: Style,
    ) -> Result<Self, StarPolygonError> {
        let (n, step) = (u64::from(side_count.get()), u64::from(step_count.get()));
        if gcd(n, step) != 1 {
            return Err(StarPolygonError::CommonDivisor);
        }
        Ok(star_polygon(side, n, step, style)?)
    }

    /// The star of five points with sides of `side`.
    pub fn star(side: Length, style: Style) -> Result<Self, Overflow> {
        star_polygon(side, 5, 2, style)
    }

    /// The star with `point_count` points on a circle of `outer_radius`
    /// and the vertices between them on a circle of `inner_radius`.
    pub fn radial_star(
        point_count: PointCount,
        inner_radius: Length,
        outer_radius: Length,
        style: Style,
    ) -> Result<Self, Overflow> {
        let n = u64::from(point_count.get());
        let (inner, outer) = (inner_radius.get(), outer_radius.get());
        let alpha = first_angle(n);
        let points = (0..n).flat_map(|i| {
            let theta1 = alpha + 360.0 * (i * 2) as f64 / (2 * n) as f64;
            let theta2 = alpha + 360.0 * (i * 2 + 1) as f64 / (2 * n) as f64;
            [
                Point {
                    x: outer * cos_deg(theta1),
                    y: outer * sin_deg(theta1),
                },
                Point {
                    x: inner * cos_deg(theta2),
                    y: inner * sin_deg(theta2),
                },
            ]
        });
        polygon(points, style)
    }

    /// The regular polygon whose sides bend into curves. Each curve leaves a
    /// vertex at `angle` degrees from the side, and `pull` sets how far the
    /// curve goes before it turns, as a fraction of the side.
    pub fn pulled_regular_polygon(
        side: Length,
        side_count: SideCount,
        pull: Finite,
        angle: Finite,
        style: Style,
    ) -> Result<Self, Overflow> {
        let n = u64::from(side_count.get());
        let radius = circumradius(side, n);
        let alpha = first_angle(n);
        let vertex = |i: u64| {
            let theta = alpha + 360.0 * i as f64 / n as f64;
            Point {
                x: radius * cos_deg(theta),
                y: radius * sin_deg(theta),
            }
        };
        // A huge angle overflows once it becomes radians. `%` is exact.
        let (pull, angle) = (pull.get(), angle.get() % 360.0);
        let first = vertex(0);
        let edges = (0..n).map(|i| {
            let to = if i + 1 < n { vertex(i + 1) } else { first };
            pulled_edge(vertex(i), to, pull, angle)
        });
        let commands: Box<[PathCmd]> = edges.collect();
        Image::closed_path(first, commands, style)
    }

    /// The slice of a circle of `radius` from the horizontal radius on the
    /// right to the radius at `angle` degrees. A positive angle turns
    /// counterclockwise and a negative one clockwise.
    pub fn wedge(radius: Length, angle: WedgeAngle, style: Style) -> Result<Self, Overflow> {
        Image::closed_path(Point::ORIGIN, wedge(radius, angle), style)
    }
}

/// The commands of a wedge that starts at its center, the origin.
pub(super) fn wedge(radius: Length, angle: WedgeAngle) -> Vec<PathCmd> {
    let (r, angle) = (radius.get(), angle.get());
    let sweep = if angle < 0.0 {
        Sweep::Clockwise
    } else {
        Sweep::Counterclockwise
    };
    let mut commands = vec![PathCmd::line_to(Point { x: r, y: 0.0 })];
    if angle.abs() == 360.0 {
        // An arc that ends where it starts draws nothing, so a full
        // turn takes two halves.
        commands.push(PathCmd::arc(
            r,
            r,
            0.0,
            ArcSize::Small,
            sweep,
            Point { x: -r, y: 0.0 },
        ));
        commands.push(PathCmd::arc(
            r,
            r,
            0.0,
            ArcSize::Small,
            sweep,
            Point { x: r, y: 0.0 },
        ));
    } else {
        let end = Point {
            x: r * cos_deg(angle),
            y: -r * sin_deg(angle),
        };
        let size = if angle.abs() > 180.0 {
            ArcSize::Large
        } else {
            ArcSize::Small
        };
        commands.push(PathCmd::arc(r, r, 0.0, size, sweep, end));
    }
    commands
}

/// The path through `points`, closed when there are three or more.
fn polygon(points: impl IntoIterator<Item = Point>, style: Style) -> Result<Image, Overflow> {
    let (start, commands, closed) = polygon_commands(points);
    Ok(Image::path_at(start, commands, closed, style)?.0)
}

/// The start and the commands of the path through `points`, and whether
/// the path is closed. No points make a path with no segment, which draws
/// nothing.
pub(super) fn polygon_commands(
    points: impl IntoIterator<Item = Point>,
) -> (Point, Box<[PathCmd]>, bool) {
    let mut points = points.into_iter();
    let start = points.next().unwrap_or(Point::ORIGIN);
    let commands: Box<[PathCmd]> = points.map(PathCmd::line_to).collect();
    let closed = commands.len() >= 2;
    (start, commands, closed)
}

/// The star polygon of [`Image::star_polygon`], for counts with no common
/// divisor.
fn star_polygon(side: Length, n: u64, step: u64, style: Style) -> Result<Image, Overflow> {
    let radius = circumradius(side, n);
    let alpha = first_angle(n);
    let points = (0..n).map(|i| {
        let theta = alpha + 360.0 * (i * step % n) as f64 / n as f64;
        Point {
            x: radius * cos_deg(theta),
            y: radius * sin_deg(theta),
        }
    });
    polygon(points, style)
}

/// The triangle with the sides `b` and `c` and the angle `angle_a` between
/// them, with `c` horizontal at the top.
fn two_sides_and_angle(b: f64, c: f64, angle_a: f64, style: Style) -> Result<Image, Overflow> {
    let corner = Point {
        x: b * cos_deg(angle_a),
        y: b * sin_deg(angle_a),
    };
    polygon([Point::ORIGIN, Point { x: c, y: 0.0 }, corner], style)
}

/// The angle of a triangle with the angles `a` and `b`.
fn third_angle(a: f64, b: f64) -> Result<f64, AngleSumError> {
    if a + b >= 180.0 {
        Err(AngleSumError::AtLeast180)
    } else {
        Ok(180.0 - a - b)
    }
}

/// The side opposite the angle `to`, in a triangle with `side` opposite the
/// angle `from`.
fn law_of_sines(side: f64, from: f64, to: f64) -> f64 {
    // The ratio of the sines goes first, since the ratio of the side to its
    // sine can overflow when the side that it gives does not.
    side * (sin_deg(to) / sin_deg(from))
}

/// The side opposite the angle `angle_c` between the sides `a` and `b`.
fn solve_side(a: f64, b: f64, angle_c: f64) -> f64 {
    // The sides go through the formula divided by the larger one, so their
    // squares neither overflow nor underflow, and the side scales back.
    let m = a.max(b);
    if m == 0.0 {
        return 0.0;
    }
    let (a, b) = (a / m, b / m);
    let square = a * a + b * b - 2.0 * a * b * cos_deg(angle_c);
    // Rounding can take the square a little below 0 when the angle is near 0.
    if square < 0.0 { 0.0 } else { m * square.sqrt() }
}

/// The angle opposite the side `opposite`, between the sides `adj1` and
/// `adj2`, in degrees.
fn solve_angle(opposite: f64, adj1: f64, adj2: f64) -> f64 {
    // An adjacent side of 0 leaves no angle, and the triangle is flat.
    if adj1 == 0.0 || adj2 == 0.0 {
        return 0.0;
    }
    // The angle does not change with the scale of the sides, and sides of
    // at most 1 have squares that neither overflow nor underflow.
    let m = opposite.max(adj1).max(adj2);
    let (o, a1, a2) = (opposite / m, adj1 / m, adj2 / m);
    let cos = (a1 * a1 + a2 * a2 - o * o) / (2.0 * a1 * a2);
    let cos = cos.clamp(-1.0, 1.0);
    (1.0 - cos * cos).sqrt().atan2(cos) * 180.0 / PI
}

/// The radius of the circle through the vertices of a regular polygon with
/// `n` sides of `side`.
fn circumradius(side: Length, n: u64) -> f64 {
    side.get() / (2.0 * sin_deg(180.0 / n as f64))
}

/// The angle of the first vertex of a polygon with `n` vertices, which puts
/// a side at the bottom.
fn first_angle(n: u64) -> f64 {
    if n.is_multiple_of(2) {
        90.0 + 180.0 / n as f64
    } else {
        -90.0
    }
}

/// The curve of a pulled polygon from the vertex `from` to the vertex `to`.
fn pulled_edge(from: Point, to: Point, pull: f64, angle: f64) -> PathCmd {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let dist = dx.hypot(dy);
    let edge = dy.atan2(dx);
    let angle = angle * PI / 180.0;
    let c1 = Point {
        x: from.x + pull * dist * (edge + angle).cos(),
        y: from.y + pull * dist * (edge + angle).sin(),
    };
    let c2 = Point {
        x: to.x - pull * dist * (edge - angle).cos(),
        y: to.y - pull * dist * (edge - angle).sin(),
    };
    PathCmd::cubic_to(c1, c2, to)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

pub(super) fn cos_deg(angle: f64) -> f64 {
    (angle * PI / 180.0).cos()
}

pub(super) fn sin_deg(angle: f64) -> f64 {
    (angle * PI / 180.0).sin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn length(v: f64) -> Length {
        Length::new(v).unwrap()
    }

    fn angle(v: f64) -> TriangleAngle {
        TriangleAngle::new(v).unwrap()
    }

    fn style() -> Style {
        Style::default()
    }

    #[test]
    fn the_basic_shapes_have_their_measures() {
        let rectangle = Image::rectangle(length(20.0), length(10.0), style());
        assert_eq!(rectangle.dimension(), (length(20.0), length(10.0)));
        let ellipse = Image::ellipse(length(20.0), length(0.0), style()).unwrap();
        assert_eq!(ellipse.dimension(), (length(20.0), length(0.0)));
        let circle = Image::circle(length(5.0), style()).unwrap();
        assert_eq!(circle.dimension(), (length(10.0), length(10.0)));
        let triangle = Image::triangle(length(10.0), style());
        assert_eq!(triangle.dimension(), (length(10.0), length(10.0 * SQRT3_2)));
    }

    #[test]
    fn a_triangle_from_its_sides_keeps_them() {
        let t = Image::triangle_sss(length(3.0), length(4.0), length(5.0), style()).unwrap();
        assert_eq!(t.width(), length(5.0));
        assert!((t.height().get() - 2.4).abs() < 1e-9, "{t:?}");
        let long = Image::triangle_sss(length(1.0), length(1.0), length(3.0), style());
        assert_eq!(long, Err(TriangleSidesError::TooLong));
        let huge = Image::triangle_sss(length(1e200), length(1e200), length(1e200), style());
        assert_eq!(huge.unwrap().width(), length(1e200));
        let tiny = Image::triangle_sss(length(1e-200), length(1e-200), length(1e-200), style());
        assert!(tiny.is_ok(), "{tiny:?}");
        let flat = Image::triangle_sss(length(0.0), length(2.0), length(2.0), style());
        assert_eq!(flat.unwrap().dimension(), (length(2.0), length(0.0)));
    }

    #[test]
    fn an_isosceles_triangle_takes_any_finite_angle() {
        let triangle =
            |v| Image::isosceles_triangle(length(10.0), Finite::new(v).unwrap(), style()).unwrap();
        for (v, same) in [(420.0, 60.0), (-60.0, 300.0), (360.0, 0.0)] {
            assert_eq!(triangle(v), triangle(same), "{v}");
        }
        assert_eq!(triangle(0.0).dimension(), (length(0.0), length(10.0)));
    }

    #[test]
    fn a_rhombus_takes_any_finite_angle() {
        let rhombus = |v| Image::rhombus(length(40.0), Finite::new(v).unwrap(), style()).unwrap();
        for (v, same) in [
            (200.0, 160.0),
            (-160.0, 160.0),
            (380.0, 20.0),
            (340.0, 20.0),
        ] {
            assert_eq!(rhombus(v), rhombus(same), "{v}");
        }
        assert_eq!(rhombus(360.0), rhombus(0.0));
        assert_eq!(rhombus(0.0).dimension(), (length(0.0), length(80.0)));
        assert_eq!(rhombus(180.0).width(), length(80.0));
        assert!(rhombus(180.0).height().get() < 1e-9);
    }

    #[test]
    fn two_angles_leave_a_third() {
        let t = Image::triangle_aas(angle(90.0), angle(45.0), length(10.0), style()).unwrap();
        assert!((t.height().get() - 10.0).abs() < 1e-9, "{t:?}");
        let huge = Image::triangle_saa(length(1e300), angle(1e-10), angle(1e-10), style());
        let width = huge.unwrap().width().get();
        assert!((width / 1e300 - 1.0).abs() < 1e-3, "{width}");
        let none = Image::triangle_asa(angle(90.0), length(10.0), angle(90.0), style());
        assert_eq!(none, Err(AngleSumError::AtLeast180));
    }

    #[test]
    fn a_polygon_with_no_points_draws_nothing() {
        let none = Image::polygon(&[], style()).unwrap();
        assert_eq!(none.dimension(), (length(0.0), length(0.0)));
    }

    #[test]
    fn a_star_polygon_visits_every_vertex() {
        let sides = SideCount::new(6).unwrap();
        let star = Image::star_polygon(length(10.0), sides, NonZeroU32::new(2).unwrap(), style());
        assert_eq!(star, Err(StarPolygonError::CommonDivisor));
        let square = Image::regular_polygon(length(10.0), SideCount::new(4).unwrap(), style());
        let (w, h) = square.unwrap().dimension();
        assert!((w.get() - 10.0).abs() < 1e-9 && (h.get() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_wedge_turns_both_ways() {
        let wedge = |a| Image::wedge(length(10.0), WedgeAngle::new(a).unwrap(), style()).unwrap();
        assert_eq!(wedge(90.0).dimension(), (length(10.0), length(10.0)));
        assert_eq!(wedge(-90.0).dimension(), (length(10.0), length(10.0)));
        assert_ne!(wedge(90.0), wedge(-90.0));
        assert_eq!(wedge(-360.0).dimension(), (length(20.0), length(20.0)));
    }

    #[test]
    fn a_pulled_polygon_takes_its_angle_modulo_360() {
        let sides = SideCount::new(5).unwrap();
        let pulled = |a| {
            Image::pulled_regular_polygon(
                length(10.0),
                sides,
                Finite::new(0.5).unwrap(),
                Finite::new(a).unwrap(),
                style(),
            )
        };
        assert_eq!(pulled(1e308), pulled(1e308 % 360.0));
        assert!(pulled(1e308).is_ok());
    }

    #[test]
    fn a_shape_too_large_is_an_overflow() {
        assert_eq!(Image::circle(length(1e308), style()), Err(Overflow));
        let sides = SideCount::new(4).unwrap();
        assert_eq!(
            Image::regular_polygon(length(f64::MAX), sides, style()),
            Err(Overflow)
        );
    }
}
