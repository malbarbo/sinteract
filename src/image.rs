//! The image model of spython and sgleam: shapes, text and bitmaps that
//! compose, transform and draw on a [`Scene`](crate::scene::Scene).

mod color;
mod font;
mod geometry;
mod measure;
mod path;
mod style;
mod tree;

pub use color::{Color, NAMED_COLORS};
pub use font::{Font, FontWeight};
pub use geometry::Point;
pub use measure::{
    AtLeast, Factor, FactorError, Finite, IsoscelesAngle, Length, LengthError, NotFinite, Opacity,
    OutOfRange, PointCount, SideCount, StepCount, TooFew, TriangleAngle, WedgeAngle,
    WedgeAngleError,
};
pub use path::PathCmd;
pub use style::{Attribute, Style};
pub use tree::{Image, Overflow};
