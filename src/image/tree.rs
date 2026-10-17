//! The tree of an image.
//!
//! Every image that a caller holds has the least corner of its bounds at the
//! origin, and so does every part of a combination and the image of a crop.
//! Each node keeps where its children go, so one picture has one tree
//! however it was built, and moving an image does not copy the tree under
//! it.

use std::fmt;
use std::sync::{Arc, LazyLock};

use super::geometry::{Bounds, ImageBox, Point};
use super::path::{self, PathCmd};
use super::{Finite, Font, Length, Style};
use crate::asset::{ImageError, decode_data_uri};
use crate::scene;

#[derive(Clone)]
pub struct Image(Arc<Node>);

pub(super) enum Node {
    Path {
        drawn: Option<Bounds>,
        style: Style,
        commands: Box<[PathCmd]>,
        closed: bool,
    },
    Line {
        style: Style,
        from: Point,
        to: Point,
    },
    /// The parts from the bottom to the top, each with the place of its
    /// least corner.
    Combination {
        drawn: Option<Bounds>,
        parts: Box<[(Image, Point)]>,
    },
    /// The part of `image` at `at` that falls in `region`.
    Crop {
        region: ImageBox,
        image: Image,
        at: Point,
    },
    Text {
        style: Style,
        region: ImageBox,
        text: Arc<str>,
        font: Font,
    },
    Bitmap {
        region: ImageBox,
        image: scene::Image,
    },
}

/// A measure of the image is too large for an `f64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow;

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the image is too large")
    }
}

impl std::error::Error for Overflow {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BitmapError {
    /// The text is not a `data:` URI in base64.
    NotDataUri,
    Image(ImageError),
}

impl fmt::Display for BitmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BitmapError::NotDataUri => f.write_str("the text is not a data URI in base64"),
            BitmapError::Image(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for BitmapError {}

static EMPTY: LazyLock<Image> = LazyLock::new(|| {
    Image(Arc::new(Node::Path {
        drawn: None,
        style: Style::default(),
        commands: Box::default(),
        closed: true,
    }))
});

impl Image {
    /// The image that draws nothing, with width and height 0.
    pub fn empty() -> Self {
        EMPTY.clone()
    }

    /// The path of `commands`. A path that does not start with a move draws
    /// nothing.
    pub fn path(
        commands: impl Into<Box<[PathCmd]>>,
        closed: bool,
        style: Style,
    ) -> Result<Self, Overflow> {
        Ok(Self::path_at(commands, closed, style)?.0)
    }

    /// The line from the origin to `(x, y)`. As in 2htdp, a line takes one
    /// more column and one more row than the distance between its ends, so a
    /// thin line fits on the canvas.
    pub fn line(x: Finite, y: Finite, style: Style) -> Self {
        let line = Self::new(Node::Line {
            style,
            from: Point::ORIGIN,
            to: Point::new(x, y),
        });
        // A finite number plus one is finite, and so are the bounds.
        line.and_then(|line| line.settle(Point::ORIGIN))
            .expect("a line with finite ends has finite bounds")
            .0
    }

    /// `text` on one line in `font`. A text that draws nothing, such as the
    /// empty text or a text of size 0, is the empty image. Returns
    /// `Overflow` when the measure of the text does not fit in an `f32`.
    pub fn text(text: &str, font: Font, style: Style) -> Result<Self, Overflow> {
        let spec = font.spec(text);
        let measure =
            crate::text::measure(&spec.family, spec.weight, spec.style, spec.size, &spec.text);
        let m = match measure {
            Some(m) if m.width() > 0.0 => m,
            Some(_) => return Ok(Self::empty()),
            // The measure refuses a size that is 0 in an f32.
            None if spec.size == 0.0 => return Ok(Self::empty()),
            None => return Err(Overflow),
        };
        let length = |v: f32| Length::new(f64::from(v)).expect("the measure of a text is finite");
        Ok(Self::new(Node::Text {
            style,
            region: ImageBox::of_size(length(m.width()), length(m.height())),
            text: text.into(),
            font,
        })
        .expect("the measure of a text is finite"))
    }

    /// The image in `uri`, a `data:` URI in base64, at the size of the
    /// original.
    pub fn bitmap_data_uri(uri: &str) -> Result<Self, BitmapError> {
        let blob = decode_data_uri(uri).ok_or(BitmapError::NotDataUri)?;
        let (image, head) = scene::Image::load_with_head(blob).map_err(BitmapError::Image)?;
        let length = |v: u32| Length::new(f64::from(v)).expect("a u32 is a length");
        let bitmap = Self::new(Node::Bitmap {
            region: ImageBox::of_size(length(head.width), length(head.height)),
            image,
        });
        Ok(bitmap.expect("the size of a bitmap is finite"))
    }

    /// `parts` drawn from the first to the last, each with its least corner
    /// at its point.
    pub fn combine(parts: impl IntoIterator<Item = (Image, Point)>) -> Result<Self, Overflow> {
        Ok(Self::combination(parts)?.0)
    }

    /// The part of `self` in the rectangle of `w` by `h` with its least
    /// corner at `(x, y)`.
    pub fn crop(&self, x: Finite, y: Finite, w: Length, h: Length) -> Self {
        let at = Point::ORIGIN.minus(Point::new(x, y));
        self.crop_at(ImageBox::of_size(w, h), at)
            .expect("the region of a crop of finite measures is finite")
    }

    pub fn width(&self) -> Length {
        self.dimension().0
    }

    pub fn height(&self) -> Length {
        self.dimension().1
    }

    pub fn dimension(&self) -> (Length, Length) {
        let b = self.bounds();
        let length = |v| Length::new(v).expect("the bounds of an image are finite");
        (length(b.max.x - b.min.x), length(b.max.y - b.min.y))
    }

    /// The center of the bounds, measured from their least corner.
    pub fn center(&self) -> Point {
        let b = self.bounds();
        Point {
            x: (b.max.x - b.min.x) / 2.0,
            y: (b.max.y - b.min.y) / 2.0,
        }
    }

    /// The path of `commands`, with the place of the least corner of its
    /// bounds.
    pub(super) fn path_at(
        commands: impl Into<Box<[PathCmd]>>,
        closed: bool,
        style: Style,
    ) -> Result<(Self, Point), Overflow> {
        let commands = commands.into();
        let drawn = path::bounds(&commands);
        let path = Self::new(Node::Path {
            drawn,
            style,
            commands,
            closed,
        })?;
        path.settle(Point::ORIGIN)
    }

    /// The part of `self` at `at` that falls in `region`, a region with its
    /// least corner at the origin.
    pub(super) fn crop_at(&self, region: ImageBox, at: Point) -> Result<Self, Overflow> {
        Self::new(Node::Crop {
            region,
            image: self.clone(),
            at,
        })
    }

    /// The image of `node`, or `Overflow` when a number in `node` is not
    /// finite.
    fn new(node: Node) -> Result<Self, Overflow> {
        let finite = match &node {
            Node::Path {
                drawn, commands, ..
            } => drawn.is_none_or(Bounds::is_finite) && commands.iter().all(|c| c.is_finite()),
            Node::Line { from, to, .. } => from.is_finite() && to.is_finite(),
            Node::Combination { drawn, parts } => {
                drawn.is_none_or(Bounds::is_finite) && parts.iter().all(|(_, at)| at.is_finite())
            }
            Node::Crop { region, at, .. } => region.is_finite() && at.is_finite(),
            Node::Text { region, .. } | Node::Bitmap { region, .. } => region.is_finite(),
        };
        if finite {
            Ok(Self(Arc::new(node)))
        } else {
            Err(Overflow)
        }
    }

    pub(super) fn node(&self) -> &Node {
        &self.0
    }

    /// The bounds of what `self` draws, or `None` when it draws nothing.
    pub(super) fn drawn(&self) -> Option<Bounds> {
        match &*self.0 {
            Node::Path { drawn, .. } | Node::Combination { drawn, .. } => *drawn,
            Node::Line { from, to, .. } => Some(Bounds {
                min: from.min(*to).translate(-0.5, -0.5),
                max: from.max(*to).translate(0.5, 0.5),
            }),
            Node::Crop { region, .. } | Node::Text { region, .. } | Node::Bitmap { region, .. } => {
                Some(region.bounds())
            }
        }
    }

    /// The bounds of `self`, which are empty at the origin when `self`
    /// draws nothing.
    fn bounds(&self) -> Bounds {
        self.drawn().unwrap_or(Bounds {
            min: Point::ORIGIN,
            max: Point::ORIGIN,
        })
    }

    /// `self` moved by `by`.
    fn translate(&self, by: Point) -> Result<Self, Overflow> {
        if by == Point::ORIGIN {
            return Ok(self.clone());
        }
        let moved = |drawn: &Option<Bounds>| drawn.map(|b| b.translate(by));
        Self::new(match &*self.0 {
            Node::Path {
                drawn,
                style,
                commands,
                closed,
            } => Node::Path {
                drawn: moved(drawn),
                style: style.clone(),
                commands: commands.iter().map(|c| c.translate(by)).collect(),
                closed: *closed,
            },
            Node::Line { style, from, to } => Node::Line {
                style: style.clone(),
                from: from.plus(by),
                to: to.plus(by),
            },
            Node::Combination { drawn, parts } => Node::Combination {
                drawn: moved(drawn),
                parts: parts
                    .iter()
                    .map(|(part, at)| (part.clone(), at.plus(by)))
                    .collect(),
            },
            Node::Crop { region, image, at } => Node::Crop {
                region: region.translate(by),
                image: image.clone(),
                at: at.plus(by),
            },
            Node::Text {
                style,
                region,
                text,
                font,
            } => Node::Text {
                style: style.clone(),
                region: region.translate(by),
                text: text.clone(),
                font: font.clone(),
            },
            Node::Bitmap { region, image } => Node::Bitmap {
                region: region.translate(by),
                image: image.clone(),
            },
        })
    }

    /// `self` moved so the least corner of its bounds is at the origin,
    /// with the place of that corner when `self` sits at `at`. An image
    /// that draws nothing sits at the origin.
    fn settle(&self, at: Point) -> Result<(Self, Point), Overflow> {
        let Some(Bounds { min, .. }) = self.drawn() else {
            return Ok((self.clone(), Point::ORIGIN));
        };
        let at = at.plus(min);
        if !at.is_finite() {
            return Err(Overflow);
        }
        Ok((self.translate(Point::ORIGIN.minus(min))?, at))
    }

    /// `parts` drawn from the first to the last, each at its point, with
    /// the place of the least corner of the whole. A part that is a
    /// combination stays whole, so an image that grows one part at a time
    /// does not copy the parts before it.
    pub(super) fn combination(
        parts: impl IntoIterator<Item = (Image, Point)>,
    ) -> Result<(Self, Point), Overflow> {
        let mut placed: Vec<(Image, Point)> = Vec::new();
        let mut drawn: Option<Bounds> = None;
        for (part, at) in parts {
            let (part, at) = part.settle(at)?;
            if let Some(b) = part.drawn() {
                let b = b.translate(at);
                drawn = Some(drawn.map_or(b, |d| d.union(b)));
            }
            placed.push((part, at));
        }
        let Some(drawn) = drawn else {
            let parts = placed.into_iter().map(|(p, _)| (p, Point::ORIGIN));
            return Ok((
                Self::new(Node::Combination {
                    drawn: None,
                    parts: parts.collect(),
                })?,
                Point::ORIGIN,
            ));
        };
        let corner = drawn.min;
        let parts = placed.into_iter().map(|(p, at)| {
            let at = if p.drawn().is_some() {
                at.minus(corner)
            } else {
                Point::ORIGIN
            };
            (p, at)
        });
        let combination = Self::new(Node::Combination {
            drawn: Some(Bounds {
                min: Point::ORIGIN,
                max: drawn.max.minus(corner),
            }),
            parts: parts.collect(),
        })?;
        Ok((combination, corner))
    }
}

/// A map of the plane, such as a rotation, that [`Image::transform`]
/// applies to every leaf of an image. Each node has a parameter of its own,
/// such as the center of a rotation in the coordinates of the node.
pub(super) trait Transform {
    type Param: Copy;

    /// The parameter of a child at `at` in a node with `param`, and how far
    /// the mapped child moves from the place that the map gives it.
    fn enter(&self, param: Self::Param, at: Point) -> (Self::Param, Point);

    fn point(&self, p: Point, param: Self::Param) -> Point;

    fn command(&self, c: PathCmd, param: Self::Param) -> PathCmd;

    fn region(&self, r: ImageBox, param: Self::Param) -> ImageBox;

    /// The style of a leaf, which a scale changes and the other maps keep.
    fn style(&self, s: &Style) -> Result<Style, Overflow> {
        Ok(s.clone())
    }
}

/// A step of [`Image::transform`].
enum Step<'a, P> {
    /// Maps `image`, which takes the parameter.
    Visit(&'a Image, P),
    /// Combines the last images mapped, one for each shift, each moved by
    /// its shift.
    Join(Box<[Point]>),
    /// Crops the last image mapped, moved by the shift, to the region.
    Clip(ImageBox, Point),
}

impl Image {
    /// `self` with `t` applied to every leaf and to the region of every
    /// crop, settled, with the place of its least corner. The steps wait on
    /// a list, so a deep tree does not overflow the stack.
    pub(super) fn transform<T: Transform>(
        &self,
        t: &T,
        param: T::Param,
    ) -> Result<(Self, Point), Overflow> {
        let mut pending = vec![Step::Visit(self, param)];
        let mut done: Vec<(Image, Point)> = Vec::new();
        while let Some(step) = pending.pop() {
            match step {
                Step::Visit(image, param) => match &*image.0 {
                    Node::Combination { parts, .. } => {
                        let (params, shifts): (Vec<_>, Vec<_>) =
                            parts.iter().map(|(_, at)| t.enter(param, *at)).unzip();
                        pending.push(Step::Join(shifts.into()));
                        let parts = parts.iter().zip(params).rev();
                        pending.extend(parts.map(|((part, _), param)| Step::Visit(part, param)));
                    }
                    Node::Crop { region, image, at } => {
                        let (image_param, shift) = t.enter(param, *at);
                        pending.push(Step::Clip(t.region(*region, param), shift));
                        pending.push(Step::Visit(image, image_param));
                    }
                    Node::Path {
                        style,
                        commands,
                        closed,
                        ..
                    } => {
                        let commands = commands.iter().map(|c| t.command(*c, param));
                        let commands: Box<[PathCmd]> = commands.collect();
                        done.push(Self::path_at(commands, *closed, t.style(style)?)?);
                    }
                    Node::Line { style, from, to } => {
                        let line = Self::new(Node::Line {
                            style: t.style(style)?,
                            from: t.point(*from, param),
                            to: t.point(*to, param),
                        })?;
                        done.push(line.settle(Point::ORIGIN)?);
                    }
                    Node::Text {
                        style,
                        region,
                        text,
                        font,
                    } => {
                        let text = Self::new(Node::Text {
                            style: t.style(style)?,
                            region: t.region(*region, param),
                            text: text.clone(),
                            font: font.clone(),
                        })?;
                        done.push(text.settle(Point::ORIGIN)?);
                    }
                    Node::Bitmap { region, image } => {
                        let bitmap = Self::new(Node::Bitmap {
                            region: t.region(*region, param),
                            image: image.clone(),
                        })?;
                        done.push(bitmap.settle(Point::ORIGIN)?);
                    }
                },
                Step::Join(shifts) => {
                    let parts = done.split_off(done.len() - shifts.len());
                    let parts = parts.into_iter().zip(shifts.iter());
                    done.push(Self::combination(
                        parts.map(|((part, at), shift)| (part, at.plus(*shift))),
                    )?);
                }
                Step::Clip(region, shift) => {
                    let (image, at) = done.pop().expect("a crop maps its image first");
                    let crop = Self::new(Node::Crop {
                        region,
                        image,
                        at: at.plus(shift),
                    })?;
                    done.push(crop.settle(Point::ORIGIN)?);
                }
            }
        }
        Ok(done.pop().expect("the steps leave one image"))
    }
}

impl Node {
    /// Moves the children of `self` to `stack`, and leaves `self` with none.
    fn move_children(&mut self, stack: &mut Vec<Image>) {
        match self {
            Node::Path { .. } | Node::Line { .. } | Node::Text { .. } | Node::Bitmap { .. } => {}
            Node::Combination { parts, .. } => {
                stack.extend(std::mem::take(parts).into_iter().map(|(p, _)| p));
            }
            Node::Crop { image, .. } => {
                if !Arc::ptr_eq(&image.0, &EMPTY.0) {
                    stack.push(std::mem::replace(image, Image::empty()));
                }
            }
        }
    }
}

impl Drop for Node {
    /// Drops the tree with a stack of its own, since a chain of crops can
    /// be deeper than the stack of the thread.
    fn drop(&mut self) {
        let mut stack = Vec::new();
        self.move_children(&mut stack);
        while let Some(image) = stack.pop() {
            if let Some(mut node) = Arc::into_inner(image.0) {
                node.move_children(&mut stack);
            }
        }
    }
}

impl PartialEq for Image {
    /// Returns `true` if the two trees are the same, `false` otherwise. Two
    /// images that draw the same pixels can have different trees. A
    /// combination inside a combination counts as its parts, so the
    /// grouping of the parts does not matter.
    fn eq(&self, other: &Self) -> bool {
        let mut pending = vec![(self, other)];
        while let Some((a, b)) = pending.pop() {
            if Arc::ptr_eq(&a.0, &b.0) {
                continue;
            }
            match (&*a.0, &*b.0) {
                (
                    Node::Path {
                        drawn: d1,
                        style: s1,
                        commands: c1,
                        closed: k1,
                    },
                    Node::Path {
                        drawn: d2,
                        style: s2,
                        commands: c2,
                        closed: k2,
                    },
                ) => {
                    if d1 != d2 || s1 != s2 || c1 != c2 || k1 != k2 {
                        return false;
                    }
                }
                (
                    Node::Line {
                        style: s1,
                        from: f1,
                        to: t1,
                    },
                    Node::Line {
                        style: s2,
                        from: f2,
                        to: t2,
                    },
                ) => {
                    if s1 != s2 || f1 != f2 || t1 != t2 {
                        return false;
                    }
                }
                (
                    Node::Combination {
                        drawn: d1,
                        parts: p1,
                    },
                    Node::Combination {
                        drawn: d2,
                        parts: p2,
                    },
                ) => {
                    if d1 != d2 {
                        return false;
                    }
                    let (l1, l2) = (leaves(p1), leaves(p2));
                    if l1.len() != l2.len() {
                        return false;
                    }
                    for ((i1, at1), (i2, at2)) in l1.into_iter().zip(l2) {
                        if at1 != at2 {
                            return false;
                        }
                        pending.push((i1, i2));
                    }
                }
                (
                    Node::Crop {
                        region: r1,
                        image: i1,
                        at: at1,
                    },
                    Node::Crop {
                        region: r2,
                        image: i2,
                        at: at2,
                    },
                ) => {
                    if r1 != r2 || at1 != at2 {
                        return false;
                    }
                    pending.push((i1, i2));
                }
                (
                    Node::Text {
                        style: s1,
                        region: r1,
                        text: t1,
                        font: f1,
                    },
                    Node::Text {
                        style: s2,
                        region: r2,
                        text: t2,
                        font: f2,
                    },
                ) => {
                    if s1 != s2 || r1 != r2 || t1 != t2 || f1 != f2 {
                        return false;
                    }
                }
                (
                    Node::Bitmap {
                        region: r1,
                        image: i1,
                    },
                    Node::Bitmap {
                        region: r2,
                        image: i2,
                    },
                ) => {
                    if r1 != r2 || i1 != i2 {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

/// The parts of a combination with each part that is a combination
/// replaced by its own parts, in drawing order. A part that draws nothing
/// sits at the origin, as [`Image::combination`] places it.
fn leaves(parts: &[(Image, Point)]) -> Vec<(&Image, Point)> {
    let mut leaves = Vec::new();
    let mut pending: Vec<_> = parts.iter().rev().map(|(p, at)| (p, *at)).collect();
    while let Some((part, at)) = pending.pop() {
        match &*part.0 {
            Node::Combination { parts, .. } => {
                pending.extend(parts.iter().rev().map(|(p, p_at)| (p, at.plus(*p_at))));
            }
            _ if part.drawn().is_none() => leaves.push((part, Point::ORIGIN)),
            _ => leaves.push((part, at)),
        }
    }
    leaves
}

impl fmt::Debug for Image {
    /// The size alone, since the tree can be deeper than the stack.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (w, h) = self.dimension();
        write!(f, "Image({}×{})", w.get(), h.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Attribute, Color};

    fn p(x: f64, y: f64) -> Point {
        Point::new(Finite::new(x).unwrap(), Finite::new(y).unwrap())
    }

    fn length(v: f64) -> Length {
        Length::new(v).unwrap()
    }

    fn polyline(points: &[(f64, f64)]) -> Result<Image, Overflow> {
        let mut commands = vec![PathCmd::move_to(p(points[0].0, points[0].1))];
        commands.extend(points[1..].iter().map(|&(x, y)| PathCmd::line_to(p(x, y))));
        let red = Style::new([Attribute::Stroke(Color::rgb(255, 0, 0))]);
        Image::path(commands, false, red)
    }

    fn square(side: f64) -> Image {
        polyline(&[(0.0, 0.0), (side, 0.0), (side, side), (0.0, side)]).unwrap()
    }

    #[test]
    fn a_path_sits_at_the_origin() {
        let moved = polyline(&[(10.0, 10.0), (30.0, 10.0), (30.0, 40.0)]).unwrap();
        assert_eq!(moved.dimension(), (length(20.0), length(30.0)));
        assert_eq!(
            moved,
            polyline(&[(0.0, 0.0), (20.0, 0.0), (20.0, 30.0)]).unwrap()
        );
        assert_eq!(moved.center(), p(10.0, 15.0));
        assert_eq!(format!("{moved:?}"), "Image(20×30)");
    }

    #[test]
    fn a_path_measures_what_it_draws() {
        let no_move = Image::path([PathCmd::line_to(p(10.0, 10.0))], false, Style::default());
        assert_eq!(no_move.unwrap().dimension(), (length(0.0), length(0.0)));
        let loose_move = Image::path(
            [
                PathCmd::move_to(p(0.0, 0.0)),
                PathCmd::line_to(p(10.0, 0.0)),
                PathCmd::move_to(p(50.0, 50.0)),
            ],
            false,
            Style::default(),
        );
        assert_eq!(loose_move.unwrap().dimension(), (length(10.0), length(0.0)));
        let residue = polyline(&[(1e-11, 0.0), (10.0, 5.0)]).unwrap();
        assert_eq!(residue.width(), length(10.0));
    }

    #[test]
    fn a_nested_combination_equals_the_flat_one() {
        let (a, b, c) = (square(10.0), square(20.0), square(5.0));
        let ab = Image::combine([(a.clone(), p(0.0, 0.0)), (b.clone(), p(10.0, 0.0))]).unwrap();
        let nested = Image::combine([(ab, p(5.0, 5.0)), (c.clone(), p(0.0, 0.0))]).unwrap();
        let flat = Image::combine([(a, p(5.0, 5.0)), (b, p(15.0, 5.0)), (c, p(0.0, 0.0))]).unwrap();
        assert_eq!(nested, flat);
        assert_eq!(flat.dimension(), (length(35.0), length(25.0)));
    }

    #[test]
    fn a_part_that_draws_nothing_adds_nothing() {
        let both = Image::combine([
            (Image::empty(), p(100.0, 100.0)),
            (square(5.0), p(1.0, 1.0)),
        ]);
        assert_eq!(both.unwrap().dimension(), (length(5.0), length(5.0)));
    }

    #[test]
    fn a_crop_has_the_size_of_its_region() {
        let x = Finite::new(2.0).unwrap();
        let cropped = square(10.0).crop(x, x, length(3.0), length(4.0));
        assert_eq!(cropped.dimension(), (length(3.0), length(4.0)));
    }

    #[test]
    fn an_infinite_measure_is_an_overflow() {
        assert_eq!(polyline(&[(-1e308, 0.0), (1e308, 0.0)]), Err(Overflow));
        let far = Image::combine([(square(1.0), p(-1e308, 0.0)), (square(1.0), p(1e308, 0.0))]);
        assert_eq!(far, Err(Overflow));
    }

    #[test]
    fn a_line_takes_one_more_column_and_row() {
        let x = Finite::new(-10.0).unwrap();
        let line = Image::line(x, Finite::new(0.0).unwrap(), Style::default());
        assert_eq!(line.dimension(), (length(11.0), length(1.0)));
        let max = Finite::new(f64::MAX).unwrap();
        let long = Image::line(max, max, Style::default());
        assert_eq!(long.width(), length(f64::MAX));
    }

    #[test]
    fn a_text_that_draws_nothing_is_empty() {
        assert_eq!(
            Image::text("", Font::default(), Style::default()),
            Ok(Image::empty())
        );
        let zero = Font {
            size: Length::new(0.0).unwrap(),
            ..Font::default()
        };
        assert_eq!(
            Image::text("hi", zero, Style::default()),
            Ok(Image::empty())
        );
        let hi = Image::text("hi", Font::default(), Style::default()).unwrap();
        let (w, h) = hi.dimension();
        assert!(w.get() > 0.0 && h.get() > 0.0, "{hi:?}");
    }

    #[test]
    fn a_text_too_large_for_an_f32_overflows() {
        let font = |size| Font {
            size: Length::new(size).unwrap(),
            ..Font::default()
        };
        let text = |size| Image::text("abcdefghijklmnop", font(size), Style::default());
        assert_eq!(text(1e39), Err(Overflow));
        assert_eq!(text(5e37), Err(Overflow));
    }

    #[test]
    fn a_bitmap_has_the_size_of_its_image() {
        let uri = |w, h| crate::asset::encode_data_uri("image/png", &crate::asset::png_head(w, h));
        let bitmap = Image::bitmap_data_uri(&uri(3, 2)).unwrap();
        assert_eq!(bitmap.dimension(), (length(3.0), length(2.0)));
        assert_eq!(
            Image::bitmap_data_uri(&uri(0, 2)),
            Err(BitmapError::Image(ImageError::Unsupported))
        );
        assert_eq!(
            Image::bitmap_data_uri("image.png"),
            Err(BitmapError::NotDataUri)
        );
        assert_eq!(
            Image::bitmap_data_uri("data:text/plain;base64,aGk="),
            Err(BitmapError::Image(ImageError::Unsupported))
        );
    }

    #[test]
    fn a_deep_tree_compares_and_drops() {
        let chain = || {
            let zero = Finite::new(0.0).unwrap();
            (0..200_000).fold(square(10.0), |img, _| {
                img.crop(zero, zero, length(10.0), length(10.0))
            })
        };
        assert_eq!(chain(), chain());
    }

    #[test]
    fn a_chain_of_combinations_equals_one_combination() {
        let n = 100_000;
        let chain = (1..n).fold(square(1.0), |img, i| {
            Image::combine([(img, p(0.0, 0.0)), (square(1.0), p(i as f64, 0.0))]).unwrap()
        });
        let flat = (0..n).map(|i| (square(1.0), p(i as f64, 0.0)));
        assert_eq!(chain, Image::combine(flat).unwrap());
    }
}
