//! Rotation, scale and flip.

use super::geometry::{ImageBox, Point};
use super::path::PathCmd;
use super::shape::{cos_deg, sin_deg};
use super::tree::{Image, Overflow, Transform};
use super::{Factor, Finite, Style};

impl Image {
    /// `self` turned `angle` degrees counterclockwise around its center.
    pub fn rotate(&self, angle: Finite) -> Result<Image, Overflow> {
        // y grows downward, so a counterclockwise turn on the screen is a
        // clockwise turn in the plane. The angle goes modulo 360, since a
        // huge angle overflows once it becomes radians. `%` is exact, and
        // keeps an angle under 360 as it is.
        let angle = -(angle.get() % 360.0);
        let rotation = Rotation {
            angle,
            cos: cos_deg(angle),
            sin: sin_deg(angle),
        };
        Ok(self.transform(&rotation, self.center())?.0)
    }

    pub fn scale(&self, factor: Factor) -> Result<Image, Overflow> {
        self.scale_xy(factor, factor)
    }

    /// `self` scaled by `x_factor` along x and `y_factor` along y. A stroke
    /// has one width and one dash, so they take the mean of the two factors,
    /// as the width does in 2htdp.
    pub fn scale_xy(&self, x_factor: Factor, y_factor: Factor) -> Result<Image, Overflow> {
        let scale = Scale {
            x: x_factor.get(),
            y: y_factor.get(),
        };
        Ok(self.transform(&scale, ())?.0)
    }

    /// `self` mirrored left to right.
    pub fn flip_horizontal(&self) -> Result<Image, Overflow> {
        Ok(self.transform(&Flip::Horizontal, ())?.0)
    }

    /// `self` mirrored top to bottom.
    pub fn flip_vertical(&self) -> Result<Image, Overflow> {
        Ok(self.transform(&Flip::Vertical, ())?.0)
    }
}

/// A turn by `angle` degrees, clockwise on the screen, around the point
/// that the parameter gives.
struct Rotation {
    angle: f64,
    cos: f64,
    sin: f64,
}

impl Rotation {
    fn around(&self, p: Point, center: Point) -> Point {
        let dx = p.x - center.x;
        let dy = p.y - center.y;
        Point {
            x: center.x + dx * self.cos - dy * self.sin,
            y: center.y + dx * self.sin + dy * self.cos,
        }
    }
}

impl Transform for Rotation {
    type Param = Point;

    /// A child at `at` sees the center at `center - at`, so the child turns
    /// around that point and keeps `at`.
    fn enter(&self, center: Point, at: Point) -> (Point, Point) {
        (center.minus(at), at)
    }

    fn point(&self, p: Point, center: Point) -> Point {
        self.around(p, center)
    }

    fn command(&self, c: PathCmd, center: Point) -> PathCmd {
        c.map(
            |p| self.around(p, center),
            // Modulo 360, so turns one after the other never overflow.
            |rx, ry, rotation, sweep| (rx, ry, (rotation + self.angle) % 360.0, sweep),
        )
    }

    fn region(&self, r: ImageBox, center: Point) -> ImageBox {
        r.map(
            |p| self.around(p, center),
            |p| self.around(p, Point::ORIGIN),
        )
    }
}

struct Scale {
    x: f64,
    y: f64,
}

impl Scale {
    fn apply(&self, p: Point) -> Point {
        Point {
            x: p.x * self.x,
            y: p.y * self.y,
        }
    }

    /// The radii and the rotation of an arc of an ellipse with radii `rx`
    /// and `ry` turned by `rotation` degrees, once scaled.
    fn arc(&self, rx: f64, ry: f64, rotation: f64) -> (f64, f64, f64) {
        // A rotated ellipse scaled along x and y is an ellipse with other
        // axes. The axes are the eigenvectors of A = M M^T, where M takes the
        // unit circle to the scaled ellipse.
        let ux = rx * cos_deg(rotation) * self.x;
        let uy = rx * sin_deg(rotation) * self.y;
        let vx = -ry * sin_deg(rotation) * self.x;
        let vy = ry * cos_deg(rotation) * self.y;
        // The squares overflow long before the radii do, so the vectors go
        // through a power of two that brings the largest one near 1. A
        // power of two keeps every bit, and the result is the same as
        // without it.
        let largest = ux.abs().max(uy.abs()).max(vx.abs()).max(vy.abs());
        let unit = if largest.is_normal() {
            f64::from_bits(largest.to_bits() & f64::INFINITY.to_bits())
        } else {
            1.0
        };
        let (ux, uy, vx, vy) = (ux / unit, uy / unit, vx / unit, vy / unit);
        let a = ux * ux + vx * vx;
        let b = ux * uy + vx * vy;
        let d = uy * uy + vy * vy;
        let mut new_rotation = (0.5 * (2.0 * b).atan2(a - d)).to_degrees();
        // Of the four axis directions, take the one closest to the old
        // rotation, so an arc that is not rotated stays that way.
        new_rotation += 90.0 * ((rotation - new_rotation) / 90.0).round_ties_even();
        let radius = |angle: f64| {
            let c = cos_deg(angle);
            let s = sin_deg(angle);
            (a * c * c + 2.0 * b * s * c + d * s * s).max(0.0).sqrt() * unit
        };
        (
            radius(new_rotation),
            radius(new_rotation + 90.0),
            new_rotation,
        )
    }
}

impl Transform for Scale {
    type Param = ();

    fn enter(&self, (): (), at: Point) -> ((), Point) {
        ((), self.apply(at))
    }

    fn point(&self, p: Point, (): ()) -> Point {
        self.apply(p)
    }

    fn command(&self, c: PathCmd, (): ()) -> PathCmd {
        c.map(
            |p| self.apply(p),
            |rx, ry, rotation, sweep| {
                let (rx, ry, rotation) = self.arc(rx, ry, rotation);
                (rx, ry, rotation, sweep)
            },
        )
    }

    fn region(&self, r: ImageBox, (): ()) -> ImageBox {
        r.map(|p| self.apply(p), |p| self.apply(p))
    }

    fn style(&self, s: &Style) -> Result<Style, Overflow> {
        // Half of each factor, since the sum of two factors can overflow.
        s.scale(self.x / 2.0 + self.y / 2.0)
    }
}

#[derive(Clone, Copy)]
enum Flip {
    Horizontal,
    Vertical,
}

impl Flip {
    fn apply(self, p: Point) -> Point {
        match self {
            Flip::Horizontal => Point { x: -p.x, y: p.y },
            Flip::Vertical => Point { x: p.x, y: -p.y },
        }
    }
}

impl Transform for Flip {
    type Param = ();

    fn enter(&self, (): (), at: Point) -> ((), Point) {
        ((), self.apply(at))
    }

    fn point(&self, p: Point, (): ()) -> Point {
        self.apply(p)
    }

    fn command(&self, c: PathCmd, (): ()) -> PathCmd {
        c.map(
            |p| self.apply(p),
            |rx, ry, rotation, sweep| (rx, ry, -rotation, sweep.reverse()),
        )
    }

    fn region(&self, r: ImageBox, (): ()) -> ImageBox {
        r.map(|p| self.apply(p), |p| self.apply(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Attribute, Color, Length, WedgeAngle, XPlace};

    fn p(x: f64, y: f64) -> Point {
        Point::new(Finite::new(x).unwrap(), Finite::new(y).unwrap())
    }

    fn finite(v: f64) -> Finite {
        Finite::new(v).unwrap()
    }

    fn length(v: f64) -> Length {
        Length::new(v).unwrap()
    }

    fn factor(v: f64) -> Factor {
        Factor::new(v).unwrap()
    }

    fn stroke(width: f64) -> Style {
        Style::new([
            Attribute::Stroke(Color::rgb(255, 0, 0)),
            Attribute::StrokeWidth(length(width)),
            Attribute::StrokeDashArray([length(width), length(width / 2.0)].into()),
        ])
    }

    fn near(a: Length, b: f64) -> bool {
        (a.get() - b).abs() < 1e-9
    }

    #[test]
    fn a_rotation_turns_counterclockwise_around_the_center() {
        let rect = Image::rectangle(length(20.0), length(10.0), stroke(1.0));
        assert_eq!(rect.rotate(finite(0.0)).unwrap(), rect);
        let (w, h) = rect.rotate(finite(90.0)).unwrap().dimension();
        assert!(near(w, 10.0) && near(h, 20.0), "{w:?} {h:?}");
        // The corner at the origin goes to the bottom, so the first point
        // of the right triangle, which is its top left corner, ends at the
        // bottom left.
        let right = Image::polygon(&[p(0.0, 0.0), p(10.0, 0.0), p(0.0, 20.0)], stroke(1.0));
        let turned = right.unwrap().rotate(finite(90.0)).unwrap();
        let expected = Image::polygon(&[p(0.0, 10.0), p(0.0, 0.0), p(20.0, 10.0)], stroke(1.0));
        assert_eq!(turned.dimension(), expected.unwrap().dimension());
    }

    #[test]
    fn an_arc_keeps_its_rotation_modulo_360() {
        let ellipse = Image::ellipse(length(20.0), length(10.0), stroke(1.0)).unwrap();
        let once = ellipse.rotate(finite(30.0)).unwrap();
        let far = (0..4).try_fold(ellipse.clone(), |img, _| img.rotate(finite(f64::MAX)));
        assert!(far.is_ok());
        let full = ellipse.rotate(finite(390.0)).unwrap();
        assert_eq!(full.dimension(), once.dimension());
    }

    #[test]
    fn a_rotation_turns_every_kind_of_part() {
        let uri = crate::asset::data_uri_of_mime("image/png", &crate::asset::png_head(3, 2));
        let parts = [
            Image::bitmap_data_uri(&uri).unwrap(),
            Image::line(finite(10.0), finite(0.0), stroke(1.0)),
            Image::rectangle(length(8.0), length(4.0), stroke(1.0)).crop(
                Point::new(finite(0.0), finite(0.0)),
                length(6.0),
                length(2.0),
            ),
            Image::wedge(length(5.0), WedgeAngle::new(90.0).unwrap(), stroke(1.0)).unwrap(),
        ];
        for part in parts {
            let (w, h) = part.dimension();
            let (tw, th) = part.rotate(finite(-90.0)).unwrap().dimension();
            assert!(near(tw, h.get()) && near(th, w.get()), "{part:?}");
        }
    }

    #[test]
    fn a_rotation_moves_each_part_with_the_whole() {
        let a = Image::rectangle(length(20.0), length(10.0), stroke(1.0));
        let b = Image::rectangle(length(10.0), length(30.0), stroke(1.0));
        let above = Image::above(XPlace::Left, [a.clone(), b.clone()]).unwrap();
        let turned = above.rotate(finite(180.0)).unwrap();
        let below = Image::above(XPlace::Right, [b, a]).unwrap();
        let (w, h) = turned.dimension();
        assert!(near(w, below.width().get()) && near(h, below.height().get()));
        let inner = |img: &Image| {
            img.crop(
                Point::new(finite(0.0), finite(0.0)),
                length(10.0),
                length(30.0),
            )
        };
        let (w, h) = inner(&turned).rotate(finite(0.0)).unwrap().dimension();
        assert_eq!((w, h), inner(&below).dimension());
    }

    #[test]
    fn a_scale_widens_the_stroke_by_the_mean_factor() {
        let square = Image::square(length(10.0), stroke(2.0));
        assert_eq!(
            square.scale(factor(2.0)).unwrap(),
            Image::square(length(20.0), stroke(4.0))
        );
        let wide = square.scale_xy(factor(3.0), factor(1.0)).unwrap();
        assert_eq!(
            wide,
            Image::rectangle(length(30.0), length(10.0), stroke(4.0))
        );
    }

    #[test]
    fn a_scaled_line_keeps_its_half_pixel() {
        let line = Image::line(finite(10.0), finite(0.0), Style::default());
        assert_eq!(
            line.scale(factor(2.0)).unwrap().dimension(),
            (length(21.0), length(1.0))
        );
    }

    #[test]
    fn a_scaled_circle_is_an_ellipse() {
        let circle = Image::circle(length(10.0), stroke(1.0)).unwrap();
        let ellipse = circle.scale_xy(factor(2.0), factor(1.0)).unwrap();
        assert_eq!(ellipse.dimension(), (length(40.0), length(20.0)));
        let turned = Image::ellipse(length(20.0), length(10.0), stroke(1.0)).unwrap();
        let turned = turned.rotate(finite(30.0)).unwrap();
        let (w, h) = turned.dimension();
        let (sw, sh) = turned
            .scale_xy(factor(2.0), factor(0.5))
            .unwrap()
            .dimension();
        assert!(
            near(sw, w.get() * 2.0) && near(sh, h.get() / 2.0),
            "{sw:?} {sh:?}"
        );
    }

    #[test]
    fn a_scaled_arc_can_be_as_large_as_an_f64() {
        let scale = Scale { x: 2.0, y: 1.0 };
        assert_eq!(scale.arc(1e200, 1e200, 0.0), (2e200, 1e200, 0.0));
        assert_eq!(scale.arc(1e300, 1e300, 90.0), (1e300, 2e300, 90.0));
    }

    #[test]
    fn a_flip_mirrors_the_image() {
        let right = |points: &[(f64, f64)]| {
            let points: Vec<Point> = points.iter().map(|&(x, y)| p(x, y)).collect();
            Image::polygon(&points, stroke(1.0)).unwrap()
        };
        let img = right(&[(0.0, 0.0), (10.0, 0.0), (0.0, 20.0)]);
        assert_eq!(
            img.flip_horizontal().unwrap(),
            right(&[(10.0, 0.0), (0.0, 0.0), (10.0, 20.0)])
        );
        assert_eq!(
            img.flip_vertical().unwrap(),
            right(&[(0.0, 20.0), (10.0, 20.0), (0.0, 0.0)])
        );
        let wedge = Image::wedge(length(5.0), WedgeAngle::new(90.0).unwrap(), stroke(1.0));
        let wedge = wedge.unwrap();
        let back = wedge.flip_vertical().unwrap().flip_vertical().unwrap();
        assert_eq!(back, wedge);
    }

    #[test]
    fn a_transformation_of_a_deep_tree_keeps_the_stack() {
        let zero = finite(0.0);
        let chain = (0..200_000).fold(Image::square(length(10.0), stroke(1.0)), |img, _| {
            img.crop(Point::new(zero, zero), length(10.0), length(10.0))
        });
        let flipped = chain.flip_horizontal().unwrap();
        assert_eq!(flipped.dimension(), chain.dimension());
    }

    #[test]
    fn an_infinite_measure_is_an_overflow() {
        let big = Image::square(length(1.5e308), Style::default());
        assert_eq!(big.rotate(finite(45.0)), Err(Overflow));
        assert_eq!(big.scale(factor(2.0)), Err(Overflow));
        let wide_stroke = Image::square(length(1.0), stroke(1e300));
        assert_eq!(wide_stroke.scale(factor(1e10)), Err(Overflow));
        // A control point can lie far outside the bounds, and the flip puts
        // it on the other side.
        let curve = Image::path(
            p(0.0, 0.0),
            [PathCmd::cubic_to(
                p(-1e308, 0.0),
                p(-1e308, 0.0),
                p(1e308, 0.0),
            )],
            stroke(1.0),
        );
        assert_eq!(curve.unwrap().flip_horizontal(), Err(Overflow));
    }
}
