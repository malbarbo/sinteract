//! Points and boxes in the plane of an image, where y grows downward.

use super::{Finite, Length};

/// A point with finite coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub(super) x: f64,
    pub(super) y: f64,
}

impl Point {
    pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

    pub fn new(x: Finite, y: Finite) -> Self {
        Self {
            x: x.get(),
            y: y.get(),
        }
    }

    pub fn x(self) -> f64 {
        self.x
    }

    pub fn y(self) -> f64 {
        self.y
    }

    pub(super) fn translate(self, dx: f64, dy: f64) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
        }
    }

    pub(super) fn plus(self, p: Point) -> Self {
        self.translate(p.x, p.y)
    }

    pub(super) fn minus(self, p: Point) -> Self {
        self.translate(-p.x, -p.y)
    }

    pub(super) fn min(self, p: Point) -> Self {
        Self {
            x: self.x.min(p.x),
            y: self.y.min(p.y),
        }
    }

    pub(super) fn max(self, p: Point) -> Self {
        Self {
            x: self.x.max(p.x),
            y: self.y.max(p.y),
        }
    }

    pub(super) fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// The least and the greatest corner of a rectangle with sides on the axes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Bounds {
    pub(super) min: Point,
    pub(super) max: Point,
}

impl Bounds {
    pub(super) fn translate(self, by: Point) -> Self {
        Self {
            min: self.min.plus(by),
            max: self.max.plus(by),
        }
    }

    pub(super) fn union(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    pub(super) fn is_finite(self) -> bool {
        self.min.is_finite() && self.max.is_finite()
    }
}

/// The image of a rectangle under an affine map. `x_axis` goes from the
/// middle of the left side to the middle of the right side, and `y_axis`
/// from the middle of the top side to the middle of the bottom side. A flip
/// makes the two axes a mirror of a rotation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ImageBox {
    pub(super) center: Point,
    pub(super) x_axis: Point,
    pub(super) y_axis: Point,
}

impl ImageBox {
    /// The rectangle of `w` by `h` with its least corner at the origin.
    pub(super) fn of_size(w: Length, h: Length) -> Self {
        let (w, h) = (w.get(), h.get());
        Self {
            center: Point {
                x: w / 2.0,
                y: h / 2.0,
            },
            x_axis: Point { x: w, y: 0.0 },
            y_axis: Point { x: 0.0, y: h },
        }
    }

    pub(super) fn translate(self, by: Point) -> Self {
        Self {
            center: self.center.plus(by),
            ..self
        }
    }

    /// The bounds of the rectangle, which is the rectangle itself unless a
    /// rotation turned it.
    pub(super) fn bounds(self) -> Bounds {
        let dx = (self.x_axis.x.abs() + self.y_axis.x.abs()) / 2.0;
        let dy = (self.x_axis.y.abs() + self.y_axis.y.abs()) / 2.0;
        Bounds {
            min: self.center.translate(-dx, -dy),
            max: self.center.translate(dx, dy),
        }
    }

    pub(super) fn is_finite(self) -> bool {
        self.center.is_finite() && self.x_axis.is_finite() && self.y_axis.is_finite()
    }
}
