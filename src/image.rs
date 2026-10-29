//! The image model of spython and sgleam: shapes, text and bitmaps that
//! compose, transform and draw on a [`Scene`](crate::scene::Scene).

mod color;
mod compose;
mod draw;
mod font;
mod geometry;
mod measure;
mod path;
mod same;
mod shape;
mod style;
#[cfg(test)]
mod svg_tests;
mod transform;
mod tree;

pub use color::{Color, NAMED_COLORS};
pub use compose::{CurveEnd, XPlace, YPlace};
pub use draw::CanvasTooLarge;
pub use font::{Font, FontWeight};
pub use geometry::Point;
pub use measure::{
    Count, CountError, Factor, FactorError, Finite, Length, LengthError, MAX_VERTICES, NotFinite,
    Opacity, OutOfRange, PointCount, SideCount, TriangleAngle, WedgeAngle, WedgeAngleError,
};
pub use path::PathCmd;
pub use shape::{AngleSumError, StarPolygonError, TriangleSidesError};
pub use style::{Attribute, Style};
pub use tree::{BitmapError, Image, Overflow};

/// The types of the scene that a [`Style`] and a [`Font`] take.
pub use crate::scene::{FillRule, FontStyle, LineCap, LineJoin, MiterLimit};
