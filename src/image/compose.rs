//! How images go together: one above, beside or over another, cropped,
//! framed, or placed on a scene.

use super::geometry::{Bounds, ImageBox, Point};
use super::path::PathCmd;
use super::shape::{cos_deg, polygon_commands, sin_deg, wedge};
use super::tree::{Image, Overflow};
use super::{Attribute, Color, Finite, Length, Style, WedgeAngle};

/// How two images line up on the horizontal axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XPlace {
    Left,
    Center,
    Right,
}

/// How two images line up on the vertical axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YPlace {
    Top,
    Middle,
    Bottom,
}

/// One end of a curve, with the angle in degrees that the curve leaves or
/// reaches it, and how far the curve keeps that angle, as a fraction of the
/// distance between the ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CurveEnd {
    pub point: Point,
    pub angle: Finite,
    pub pull: Finite,
}

impl Image {
    /// `images` from the top to the bottom, each lined up with the ones
    /// above it by `x_place`.
    pub fn above(
        x_place: XPlace,
        images: impl IntoIterator<Item = Image>,
    ) -> Result<Image, Overflow> {
        line_up(images, false, |(wa, ha), (wb, _)| {
            let (dxa, dxb) = x_place_dx(x_place, wa, wb);
            (Point { x: dxa, y: 0.0 }, Point { x: dxb, y: ha })
        })
    }

    /// `images` from the left to the right, each lined up with the ones on
    /// its left by `y_place`.
    pub fn beside(
        y_place: YPlace,
        images: impl IntoIterator<Item = Image>,
    ) -> Result<Image, Overflow> {
        line_up(images, false, |(wa, ha), (_, hb)| {
            let (dya, dyb) = y_place_dy(y_place, ha, hb);
            (Point { x: 0.0, y: dya }, Point { x: wa, y: dyb })
        })
    }

    /// `images` from the top to the bottom, each lined up with the ones
    /// over it by `x_place` and `y_place`, and then moved by `offset`.
    pub fn overlay(
        x_place: XPlace,
        y_place: YPlace,
        offset: Point,
        images: impl IntoIterator<Item = Image>,
    ) -> Result<Image, Overflow> {
        line_up(images, true, |top, bottom| {
            let (top_at, bottom_at) = aligned(x_place, y_place, top, bottom);
            (top_at, offset.plus(bottom_at))
        })
    }

    /// `images` from the bottom to the top, each lined up with the ones
    /// under it by `x_place` and `y_place`, and then moved by `offset`.
    pub fn underlay(
        x_place: XPlace,
        y_place: YPlace,
        offset: Point,
        images: impl IntoIterator<Item = Image>,
    ) -> Result<Image, Overflow> {
        line_up(images, false, |bottom, top| {
            let (top_at, bottom_at) = aligned(x_place, y_place, top, bottom);
            (bottom_at, offset.plus(top_at))
        })
    }

    /// `self` over `bottom`, with the least corner of `bottom` at `at`.
    pub fn overlay_xy(&self, at: Point, bottom: &Image) -> Result<Image, Overflow> {
        Image::combine([(bottom.clone(), at), (self.clone(), Point::ORIGIN)])
    }

    /// `top` over `self`, with the least corner of `top` at `at`.
    pub fn underlay_xy(&self, at: Point, top: &Image) -> Result<Image, Overflow> {
        Image::combine([(self.clone(), Point::ORIGIN), (top.clone(), at)])
    }

    /// The part of `self` in a rectangle of `w` by `h` lined up with `self`
    /// by `x_place` and `y_place`. A rectangle larger than `self` leaves
    /// room around it.
    pub fn crop_align(&self, x_place: XPlace, y_place: YPlace, w: Length, h: Length) -> Image {
        let dx = x_share(x_place, self.width().get() - w.get());
        let dy = y_share(y_place, self.height().get() - h.get());
        let at = Point::ORIGIN.minus(Point { x: dx, y: dy });
        self.crop_at(ImageBox::of_size(w, h), at)
            .expect("the difference of two lengths is finite")
    }

    /// `self` with an outline of `color` on the edge of its bounds.
    pub fn frame(&self, color: Color) -> Image {
        let (w, h) = self.dimension();
        let outline = Image::rectangle(w, h, outline(color));
        let framed = Image::combine([(self.clone(), Point::ORIGIN), (outline, Point::ORIGIN)])
            .expect("an image and its outline have the same bounds");
        framed
            .crop_at(ImageBox::of_size(w, h), Point::ORIGIN)
            .expect("the region of a crop of finite measures is finite")
    }

    /// A scene of `w` by `h` with an outline of `color` on its edge.
    pub fn empty_scene(w: Length, h: Length, color: Color) -> Image {
        Image::rectangle(w, h, outline(color))
            .crop_at(ImageBox::of_size(w, h), Point::ORIGIN)
            .expect("the region of a crop of finite measures is finite")
    }

    /// `self` with `img` over it, lined up by `x_place` and `y_place` with
    /// `at`, and cropped to the bounds of `self`.
    pub fn place_image(
        &self,
        at: Point,
        x_place: XPlace,
        y_place: YPlace,
        img: &Image,
    ) -> Result<Image, Overflow> {
        self.place_lined_up(at, x_place, y_place, img)
    }

    /// `self` with each image of `items` placed as [`Image::place_image`]
    /// places it, from the first to the last.
    pub fn place_images(
        &self,
        x_place: XPlace,
        y_place: YPlace,
        items: impl IntoIterator<Item = (Point, Image)>,
    ) -> Result<Image, Overflow> {
        items
            .into_iter()
            .try_fold(self.clone(), |scene, (at, img)| {
                scene.place_lined_up(at, x_place, y_place, &img)
            })
    }

    /// `self` with the center of `img` at `(x, y)`, where `y` grows upward
    /// from the bottom of `self`, cropped to the bounds of `self`.
    pub fn put_image(&self, x: Finite, y: Finite, img: &Image) -> Result<Image, Overflow> {
        let (dx, dy) = place_dx_dy(XPlace::Center, YPlace::Middle, img);
        let at = Point {
            x: x.get() + dx,
            y: self.height().get() - y.get() + dy,
        };
        self.place(img, at)
    }

    /// `self` with the line from `from` to `to`, cropped to the bounds of
    /// `self`.
    pub fn place_line(&self, from: Point, to: Point, style: Style) -> Result<Image, Overflow> {
        let (line, at) = line(from, to, style)?;
        self.place(&line, at)
    }

    /// `self` with the polygon through `points`, cropped to the bounds of
    /// `self`.
    pub fn place_polygon(&self, points: &[Point], style: Style) -> Result<Image, Overflow> {
        let (commands, closed) = polygon_commands(points.iter().copied());
        let (polygon, at) = Image::path_at(commands, closed, style)?;
        self.place(&polygon, at)
    }

    /// `self` with the curve from `from` to `to`, cropped to the bounds of
    /// `self`.
    pub fn place_curve(
        &self,
        from: CurveEnd,
        to: CurveEnd,
        style: Style,
    ) -> Result<Image, Overflow> {
        let (curve, at) = curve(from, to, false, style)?;
        self.place(&curve, at)
    }

    /// `self` with the wedge of [`Image::wedge`] centered at `center`,
    /// cropped to the bounds of `self`.
    pub fn place_wedge(
        &self,
        center: Point,
        radius: Length,
        angle: WedgeAngle,
        style: Style,
    ) -> Result<Image, Overflow> {
        let (wedge, corner) = Image::path_at(wedge(radius, angle), true, style)?;
        self.place(&wedge, center.plus(corner))
    }

    /// `self` with the line from `from` to `to`.
    pub fn add_line(&self, from: Point, to: Point, style: Style) -> Result<Image, Overflow> {
        self.add(line(from, to, style)?)
    }

    /// `self` with the polygon through `points`.
    pub fn add_polygon(&self, points: &[Point], style: Style) -> Result<Image, Overflow> {
        let (commands, closed) = polygon_commands(points.iter().copied());
        self.add(Image::path_at(commands, closed, style)?)
    }

    /// `self` with the curve from `from` to `to`.
    pub fn add_curve(&self, from: CurveEnd, to: CurveEnd, style: Style) -> Result<Image, Overflow> {
        self.add(curve(from, to, false, style)?)
    }

    /// `self` with the curve from `from` to `to`, closed by the segment
    /// from `to` back to `from`.
    pub fn add_solid_curve(
        &self,
        from: CurveEnd,
        to: CurveEnd,
        style: Style,
    ) -> Result<Image, Overflow> {
        self.add(curve(from, to, true, style)?)
    }

    /// `self` with the wedge of [`Image::wedge`] centered at `center`.
    pub fn add_wedge(
        &self,
        center: Point,
        radius: Length,
        angle: WedgeAngle,
        style: Style,
    ) -> Result<Image, Overflow> {
        let (wedge, corner) = Image::path_at(wedge(radius, angle), true, style)?;
        self.add((wedge, center.plus(corner)))
    }

    /// `self` with `img` over it, with the least corner of `img` at `at`,
    /// cropped to the bounds of `self`.
    /// `self` with `img` lined up with `at` by `x_place` and `y_place`, and
    /// cropped to the bounds of `self`.
    fn place_lined_up(
        &self,
        at: Point,
        x_place: XPlace,
        y_place: YPlace,
        img: &Image,
    ) -> Result<Image, Overflow> {
        let (dx, dy) = place_dx_dy(x_place, y_place, img);
        self.place(img, at.plus(Point { x: dx, y: dy }))
    }

    fn place(&self, img: &Image, at: Point) -> Result<Image, Overflow> {
        let (w, h) = self.dimension();
        let parts = [(self.clone(), Point::ORIGIN), (img.clone(), at)];
        let (both, corner) = Image::combination(parts)?;
        both.crop_at(ImageBox::of_size(w, h), corner)
    }

    /// `self` with `shape` over it, where `shape` is an image and the place
    /// of its least corner.
    fn add(&self, (shape, at): (Image, Point)) -> Result<Image, Overflow> {
        Image::combine([(self.clone(), Point::ORIGIN), (shape, at)])
    }
}

/// `images` combined once, each lined up with the ones before it by
/// `place`. `place` takes the size of the ones before and the size of the
/// next one, and gives where the least corner of each goes. With `under`,
/// each image goes under the ones before it. No image is the empty image.
fn line_up(
    images: impl IntoIterator<Item = Image>,
    under: bool,
    place: impl Fn((f64, f64), (f64, f64)) -> (Point, Point),
) -> Result<Image, Overflow> {
    let size = |b: Option<Bounds>| b.map_or((0.0, 0.0), |b| (b.max.x - b.min.x, b.max.y - b.min.y));
    let mut parts = Vec::new();
    // The bounds of the images so far, where the first one sits.
    let mut drawn: Option<Bounds> = None;
    for img in images {
        let next = img.drawn();
        let (before_at, at) = place(size(drawn), size(next));
        let Some(b) = next else {
            parts.push((img, Point::ORIGIN));
            continue;
        };
        let corner = drawn.map_or(Point::ORIGIN, |d| d.min);
        let by = at.minus(before_at).plus(corner).minus(b.min);
        let b = b.translate(by);
        if !b.is_finite() {
            return Err(Overflow);
        }
        drawn = Some(drawn.map_or(b, |d| d.union(b)));
        parts.push((img, by));
    }
    if under {
        parts.reverse();
    }
    match <[_; 1]>::try_from(parts) {
        Ok([(img, _)]) => Ok(img),
        Err(parts) if parts.is_empty() => Ok(Image::empty()),
        Err(parts) => Image::combine(parts),
    }
}

fn outline(color: Color) -> Style {
    let width = Length::new(2.0).expect("2 is a length");
    Style::new([Attribute::Stroke(color), Attribute::StrokeWidth(width)])
}

fn line(from: Point, to: Point, style: Style) -> Result<(Image, Point), Overflow> {
    Image::path_at([PathCmd::move_to(from), PathCmd::line_to(to)], false, style)
}

fn curve(
    from: CurveEnd,
    to: CurveEnd,
    closed: bool,
    style: Style,
) -> Result<(Image, Point), Overflow> {
    let (p1, p2) = (from.point, to.point);
    let dist = (p2.x - p1.x).hypot(p2.y - p1.y);
    let reach = |pull: Finite, c: f64| pull.get() * dist * c;
    // A huge angle overflows once it becomes radians. `%` is exact.
    let (a1, a2) = (from.angle.get() % 360.0, to.angle.get() % 360.0);
    let c1 = Point {
        x: p1.x + reach(from.pull, cos_deg(a1)),
        y: p1.y - reach(from.pull, sin_deg(a1)),
    };
    let c2 = Point {
        x: p2.x - reach(to.pull, cos_deg(a2)),
        y: p2.y + reach(to.pull, sin_deg(a2)),
    };
    let commands = [PathCmd::move_to(p1), PathCmd::cubic_to(c1, c2, p2)];
    Image::path_at(commands, closed, style)
}

/// Where the least corner of a top of size `(wt, ht)` and of a bottom of
/// size `(wb, hb)` go to line them up.
fn aligned(
    x_place: XPlace,
    y_place: YPlace,
    (wt, ht): (f64, f64),
    (wb, hb): (f64, f64),
) -> (Point, Point) {
    let (dxa, dxb) = x_place_dx(x_place, wt, wb);
    let (dya, dyb) = y_place_dy(y_place, ht, hb);
    (Point { x: dxa, y: dya }, Point { x: dxb, y: dyb })
}

fn x_place_dx(x_place: XPlace, wa: f64, wb: f64) -> (f64, f64) {
    let wm = wa.max(wb);
    (x_share(x_place, wm - wa), x_share(x_place, wm - wb))
}

fn y_place_dy(y_place: YPlace, ha: f64, hb: f64) -> (f64, f64) {
    let hm = ha.max(hb);
    (y_share(y_place, hm - ha), y_share(y_place, hm - hb))
}

/// How far the least corner of `img` goes from the point that it lines up
/// with by `x_place` and `y_place`.
fn place_dx_dy(x_place: XPlace, y_place: YPlace, img: &Image) -> (f64, f64) {
    let (w, h) = img.dimension();
    (x_share(x_place, -w.get()), y_share(y_place, -h.get()))
}

/// The part of `room` to the left of an image that `x_place` lines up in
/// it.
fn x_share(x_place: XPlace, room: f64) -> f64 {
    match x_place {
        XPlace::Left => 0.0,
        XPlace::Center => room / 2.0,
        XPlace::Right => room,
    }
}

/// The part of `room` above an image that `y_place` lines up in it.
fn y_share(y_place: YPlace, room: f64) -> f64 {
    match y_place {
        YPlace::Top => 0.0,
        YPlace::Middle => room / 2.0,
        YPlace::Bottom => room,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Point {
        Point::new(Finite::new(x).unwrap(), Finite::new(y).unwrap())
    }

    fn finite(v: f64) -> Finite {
        Finite::new(v).unwrap()
    }

    fn length(v: f64) -> Length {
        Length::new(v).unwrap()
    }

    fn red() -> Style {
        Style::new([Attribute::Fill(Color::rgb(255, 0, 0))])
    }

    fn rectangle(w: f64, h: f64) -> Image {
        Image::rectangle(length(w), length(h), red())
    }

    #[test]
    fn above_and_beside_line_up_the_images() {
        let (a, b) = (rectangle(10.0, 10.0), rectangle(20.0, 5.0));
        let above = Image::above(XPlace::Center, [a.clone(), b.clone()]).unwrap();
        let expected = Image::combine([(a.clone(), p(5.0, 0.0)), (b.clone(), p(0.0, 10.0))]);
        assert_eq!(above, expected.unwrap());
        assert_eq!(above.dimension(), (length(20.0), length(15.0)));
        let beside = Image::beside(YPlace::Bottom, [a.clone(), b.clone()]).unwrap();
        let expected = Image::combine([(a, p(0.0, 0.0)), (b, p(10.0, 5.0))]);
        assert_eq!(beside, expected.unwrap());
    }

    #[test]
    fn many_images_fold_from_the_first() {
        let (a, b, c) = (
            rectangle(10.0, 10.0),
            rectangle(20.0, 5.0),
            rectangle(4.0, 4.0),
        );
        let ab = Image::above(XPlace::Right, [a.clone(), b.clone()]).unwrap();
        let abc = Image::above(XPlace::Right, [a.clone(), b, c.clone()]);
        assert_eq!(abc, Image::above(XPlace::Right, [ab, c]));
        assert_eq!(Image::beside(YPlace::Top, []), Ok(Image::empty()));
        assert_eq!(
            Image::overlay(XPlace::Left, YPlace::Top, p(1.0, 1.0), [a.clone()]),
            Ok(a)
        );
    }

    #[test]
    fn the_offset_moves_each_next_image() {
        let (a, b, c) = (
            rectangle(10.0, 10.0),
            rectangle(20.0, 20.0),
            rectangle(4.0, 4.0),
        );
        let over = Image::overlay(
            XPlace::Left,
            YPlace::Top,
            p(-5.0, 0.0),
            [a.clone(), b.clone(), c.clone()],
        );
        let expected = Image::combine([
            (c.clone(), p(-10.0, 0.0)),
            (b.clone(), p(-5.0, 0.0)),
            (a.clone(), p(0.0, 0.0)),
        ]);
        assert_eq!(over.unwrap(), expected.unwrap());
        let under = Image::underlay(
            XPlace::Center,
            YPlace::Middle,
            p(0.0, 30.0),
            [b.clone(), a.clone()],
        );
        let expected = Image::combine([(b.clone(), p(0.0, 0.0)), (a.clone(), p(5.0, 35.0))]);
        assert_eq!(under.unwrap(), expected.unwrap());
        assert_eq!(
            a.overlay_xy(p(3.0, 4.0), &b).unwrap(),
            b.underlay_xy(p(-3.0, -4.0), &a).unwrap()
        );
    }

    #[test]
    fn a_larger_crop_leaves_room_around_the_image() {
        let a = rectangle(10.0, 10.0);
        let cropped = a.crop_align(XPlace::Right, YPlace::Middle, length(20.0), length(4.0));
        let expected = a.crop(
            Point::new(finite(-10.0), finite(3.0)),
            length(20.0),
            length(4.0),
        );
        assert_eq!(cropped, expected);
    }

    #[test]
    fn a_frame_and_a_scene_keep_their_size() {
        let framed = rectangle(10.0, 7.0).frame(Color::rgb(0, 0, 0));
        assert_eq!(framed.dimension(), (length(10.0), length(7.0)));
        let scene = Image::empty_scene(length(30.0), length(20.0), Color::rgb(0, 0, 0));
        assert_eq!(scene.dimension(), (length(30.0), length(20.0)));
    }

    #[test]
    fn a_placed_image_is_cropped_to_the_scene() {
        let scene = rectangle(30.0, 20.0);
        let a = rectangle(10.0, 10.0);
        let placed = scene.place_image(
            Point::new(finite(30.0), finite(0.0)),
            XPlace::Center,
            YPlace::Middle,
            &a,
        );
        let both = Image::combine([(scene.clone(), p(0.0, 5.0)), (a.clone(), p(25.0, 0.0))]);
        let expected = both.unwrap().crop(
            Point::new(finite(0.0), finite(5.0)),
            length(30.0),
            length(20.0),
        );
        assert_eq!(placed, Ok(expected));
        assert_eq!(scene.put_image(finite(30.0), finite(20.0), &a), placed);
        let (center, middle) = (XPlace::Center, YPlace::Middle);
        let twice =
            placed
                .unwrap()
                .place_image(Point::new(finite(0.0), finite(0.0)), center, middle, &a);
        let items = [(p(30.0, 0.0), a.clone()), (p(0.0, 0.0), a.clone())];
        assert_eq!(scene.place_images(center, middle, items), twice);
    }

    #[test]
    fn a_placed_shape_is_cropped_to_the_scene() {
        let scene = rectangle(30.0, 20.0);
        let line = scene
            .place_line(p(-100.0, 10.0), p(100.0, 10.0), red())
            .unwrap();
        assert_eq!(line.dimension(), scene.dimension());
    }

    #[test]
    fn an_added_shape_grows_the_image() {
        let a = rectangle(10.0, 10.0);
        let line = a.add_line(p(5.0, 5.0), p(25.0, -5.0), red()).unwrap();
        assert_eq!(line.dimension(), (length(25.0), length(15.0)));
        let triangle = a.add_polygon(&[p(0.0, 0.0), p(0.0, 20.0), p(-5.0, 0.0)], red());
        assert_eq!(triangle.unwrap().dimension(), (length(15.0), length(20.0)));
        let wedge = WedgeAngle::new(-90.0).unwrap();
        let wedge = a
            .add_wedge(p(10.0, 10.0), length(5.0), wedge, red())
            .unwrap();
        assert_eq!(wedge.dimension(), (length(15.0), length(15.0)));
    }

    #[test]
    fn a_curve_without_pull_is_a_segment() {
        let end = |x, y| CurveEnd {
            point: p(x, y),
            angle: finite(45.0),
            pull: finite(0.0),
        };
        let a = Image::empty();
        let curve = a.add_curve(end(0.0, 0.0), end(20.0, 10.0), red()).unwrap();
        assert_eq!(curve.dimension(), (length(20.0), length(10.0)));
        let solid = a.add_solid_curve(end(0.0, 0.0), end(20.0, 10.0), red());
        assert_eq!(solid.unwrap().dimension(), (length(20.0), length(10.0)));
    }

    #[test]
    fn a_curve_takes_its_angle_modulo_360() {
        let end = |x, y, a| CurveEnd {
            point: p(x, y),
            angle: finite(a),
            pull: finite(0.5),
        };
        let a = Image::empty();
        let curve = |angle| a.add_curve(end(0.0, 0.0, angle), end(20.0, 10.0, angle), red());
        assert_eq!(curve(1e308), curve(1e308 % 360.0));
        assert!(curve(1e308).is_ok());
    }

    #[test]
    fn a_composition_beyond_the_range_of_f64_is_an_overflow() {
        let long = rectangle(f64::MAX, 1.0);
        let two = || [long.clone(), long.clone()];
        assert_eq!(Image::beside(YPlace::Middle, two()), Err(Overflow));
        assert_eq!(
            Image::above(XPlace::Center, two()).unwrap().width(),
            length(f64::MAX)
        );
        let line = long.add_line(p(0.0, 0.0), p(-f64::MAX, 0.0), red());
        assert_eq!(line, Err(Overflow));
        let wedge = WedgeAngle::new(90.0).unwrap();
        let far = long.place_wedge(p(f64::MAX, 0.0), length(1e300), wedge, red());
        assert_eq!(far, Err(Overflow));
    }
}
