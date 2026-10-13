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
use super::{Finite, Length, Style};

#[derive(Clone)]
pub struct Image(Arc<Node>);

enum Node {
    Path {
        drawn: Option<Bounds>,
        style: Style,
        commands: Box<[PathCmd]>,
        closed: bool,
    },
    /// The parts from the bottom to the top, each with the place of its
    /// least corner. No part is a combination.
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
        let commands = commands.into();
        let drawn = path::bounds(&commands);
        let path = Self::new(Node::Path {
            drawn,
            style,
            commands,
            closed,
        })?;
        Ok(path.settle(Point::ORIGIN)?.0)
    }

    /// `parts` drawn from the first to the last, each with its least corner
    /// at its point.
    pub fn combine(parts: impl IntoIterator<Item = (Image, Point)>) -> Result<Self, Overflow> {
        Ok(Self::combination(parts)?.0)
    }

    /// The part of `self` in the rectangle of `w` by `h` with its least
    /// corner at `(x, y)`.
    pub fn crop(&self, x: Finite, y: Finite, w: Length, h: Length) -> Self {
        let at = Point::new(x, y);
        Self(Arc::new(Node::Crop {
            region: ImageBox::of_size(w, h),
            image: self.clone(),
            at: Point::ORIGIN.minus(at),
        }))
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

    /// The image of `node`, or `Overflow` when a number in `node` is not
    /// finite.
    fn new(node: Node) -> Result<Self, Overflow> {
        let finite = match &node {
            Node::Path {
                drawn, commands, ..
            } => drawn.is_none_or(Bounds::is_finite) && commands.iter().all(|c| c.is_finite()),
            Node::Combination { drawn, parts } => {
                drawn.is_none_or(Bounds::is_finite) && parts.iter().all(|(_, at)| at.is_finite())
            }
            Node::Crop { region, at, .. } => region.is_finite() && at.is_finite(),
        };
        if finite {
            Ok(Self(Arc::new(node)))
        } else {
            Err(Overflow)
        }
    }

    /// The bounds of what `self` draws, or `None` when it draws nothing.
    fn drawn(&self) -> Option<Bounds> {
        match &*self.0 {
            Node::Path { drawn, .. } | Node::Combination { drawn, .. } => *drawn,
            Node::Crop { region, .. } => Some(region.bounds()),
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
    /// combination gives its own parts.
    fn combination(
        parts: impl IntoIterator<Item = (Image, Point)>,
    ) -> Result<(Self, Point), Overflow> {
        let mut flat: Vec<(Image, Point)> = Vec::new();
        let mut drawn: Option<Bounds> = None;
        for (part, at) in parts {
            let (part, at) = part.settle(at)?;
            if let Some(b) = part.drawn() {
                let b = b.translate(at);
                drawn = Some(drawn.map_or(b, |d| d.union(b)));
            }
            match &*part.0 {
                Node::Combination { parts, .. } => {
                    flat.extend(parts.iter().map(|(p, p_at)| (p.clone(), at.plus(*p_at))));
                }
                _ => flat.push((part, at)),
            }
        }
        let Some(drawn) = drawn else {
            let parts = flat.into_iter().map(|(p, _)| (p, Point::ORIGIN));
            return Ok((
                Self::new(Node::Combination {
                    drawn: None,
                    parts: parts.collect(),
                })?,
                Point::ORIGIN,
            ));
        };
        let corner = drawn.min;
        let parts = flat.into_iter().map(|(p, at)| {
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

impl Node {
    /// Moves the children of `self` to `stack`, and leaves `self` with none.
    fn move_children(&mut self, stack: &mut Vec<Image>) {
        match self {
            Node::Path { .. } => {}
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
    /// images that draw the same pixels can have different trees.
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
                    Node::Combination {
                        drawn: d1,
                        parts: p1,
                    },
                    Node::Combination {
                        drawn: d2,
                        parts: p2,
                    },
                ) => {
                    if d1 != d2 || p1.len() != p2.len() {
                        return false;
                    }
                    for ((i1, at1), (i2, at2)) in p1.iter().zip(p2.iter()) {
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
                _ => return false,
            }
        }
        true
    }
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
    fn a_combination_gives_the_parts_of_a_part() {
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
    fn a_deep_tree_compares_and_drops() {
        let chain = || {
            let zero = Finite::new(0.0).unwrap();
            (0..200_000).fold(square(10.0), |img, _| {
                img.crop(zero, zero, length(10.0), length(10.0))
            })
        };
        assert_eq!(chain(), chain());
    }
}
