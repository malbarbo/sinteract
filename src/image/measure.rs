//! The numbers that an image takes, each checked once, when it is built.
//!
//! A front end builds each argument with `new`, which returns the rule that
//! the value breaks. The front end knows the name of the argument and the
//! value as the program wrote it, so it writes the message, or puts a value
//! that keeps the rule in its place.

use std::fmt;

/// A finite number, such as a coordinate or an offset.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Finite(f64);

impl Finite {
    pub fn new(v: f64) -> Result<Self, NotFinite> {
        if v.is_finite() {
            Ok(Self(v))
        } else {
            Err(NotFinite)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// The value is infinite or NaN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotFinite;

impl fmt::Display for NotFinite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the value is not a finite number")
    }
}

impl std::error::Error for NotFinite {}

/// A finite size that is not negative, such as a width or a radius. `-0.0`
/// is not negative.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Length(f64);

impl Length {
    pub fn new(v: f64) -> Result<Self, LengthError> {
        if !v.is_finite() {
            Err(LengthError::NotFinite)
        } else if v < 0.0 {
            Err(LengthError::Negative)
        } else {
            Ok(Self(v))
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LengthError {
    NotFinite,
    Negative,
}

impl fmt::Display for LengthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LengthError::NotFinite => NotFinite.fmt(f),
            LengthError::Negative => f.write_str("the length is negative"),
        }
    }
}

impl std::error::Error for LengthError {}

/// A finite scale factor greater than 0.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Factor(f64);

impl Factor {
    pub fn new(v: f64) -> Result<Self, FactorError> {
        if !v.is_finite() {
            Err(FactorError::NotFinite)
        } else if v <= 0.0 {
            Err(FactorError::NotPositive)
        } else {
            Ok(Self(v))
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactorError {
    NotFinite,
    NotPositive,
}

impl fmt::Display for FactorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FactorError::NotFinite => NotFinite.fmt(f),
            FactorError::NotPositive => f.write_str("the factor is not positive"),
        }
    }
}

impl std::error::Error for FactorError {}

/// An angle of a triangle, in degrees, greater than 0 and less than 180.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct TriangleAngle(f64);

impl TriangleAngle {
    pub fn new(v: f64) -> Result<Self, OutOfRange> {
        if 0.0 < v && v < 180.0 {
            Ok(Self(v))
        } else {
            Err(OutOfRange)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// An opacity, from 0, transparent, to 1, opaque.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Opacity(f64);

impl Opacity {
    pub const TRANSPARENT: Self = Self(0.0);
    pub const OPAQUE: Self = Self(1.0);

    pub fn new(v: f64) -> Result<Self, OutOfRange> {
        if (0.0..=1.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(OutOfRange)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// The value is out of the range of its type. NaN and the infinities are
/// out of every bounded range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange;

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the value is out of range")
    }
}

impl std::error::Error for OutOfRange {}

/// The angle of a wedge, in degrees. A positive angle turns counterclockwise
/// and a negative one clockwise, and 360 or -360 is the whole circle.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct WedgeAngle(f64);

impl WedgeAngle {
    pub fn new(v: f64) -> Result<Self, WedgeAngleError> {
        if v == 0.0 {
            Err(WedgeAngleError::Zero)
        } else if !(-360.0..=360.0).contains(&v) {
            Err(WedgeAngleError::OutOfRange)
        } else {
            Ok(Self(v))
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WedgeAngleError {
    Zero,
    /// The angle is below -360 or above 360, or is NaN.
    OutOfRange,
}

impl fmt::Display for WedgeAngleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WedgeAngleError::Zero => f.write_str("the angle is 0"),
            WedgeAngleError::OutOfRange => OutOfRange.fmt(f),
        }
    }
}

impl std::error::Error for WedgeAngleError {}

/// A count from `MIN` to `MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Count<const MIN: u32, const MAX: u32>(u32);

impl<const MIN: u32, const MAX: u32> Count<MIN, MAX> {
    pub fn new(n: u32) -> Result<Self, CountError> {
        if n < MIN {
            Err(CountError::TooFew)
        } else if n > MAX {
            Err(CountError::TooMany)
        } else {
            Ok(Self(n))
        }
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// The most vertices a shape takes. A shape allocates all its vertices at
/// once, and 10^6 of them already take seconds to draw.
pub const MAX_VERTICES: u32 = 100_000;

/// The sides of a polygon, from 3 to [`MAX_VERTICES`].
pub type SideCount = Count<3, MAX_VERTICES>;

/// The points of a radial star, from 2 to [`MAX_VERTICES`].
pub type PointCount = Count<2, MAX_VERTICES>;

/// How many vertices a star polygon skips from one point to the next, at
/// least 1.
pub type StepCount = Count<1, { u32::MAX }>;

/// The count is outside the range of its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountError {
    TooFew,
    TooMany,
}

impl fmt::Display for CountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CountError::TooFew => f.write_str("the count is too small"),
            CountError::TooMany => f.write_str("the count is too large"),
        }
    }
}

impl std::error::Error for CountError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_refuses_infinity_and_nan() {
        assert_eq!(Finite::new(-1e308).unwrap().get(), -1e308);
        assert_eq!(Finite::new(f64::INFINITY), Err(NotFinite));
        assert_eq!(Finite::new(f64::NEG_INFINITY), Err(NotFinite));
        assert_eq!(Finite::new(f64::NAN), Err(NotFinite));
    }

    #[test]
    fn length_takes_zero_and_negative_zero() {
        assert_eq!(Length::new(0.0).unwrap().get(), 0.0);
        assert!(Length::new(-0.0).unwrap().get().is_sign_negative());
        assert_eq!(Length::new(-1e-300), Err(LengthError::Negative));
        assert_eq!(Length::new(f64::NAN), Err(LengthError::NotFinite));
        assert_eq!(Length::new(f64::NEG_INFINITY), Err(LengthError::NotFinite));
    }

    #[test]
    fn factor_refuses_zero() {
        assert_eq!(Factor::new(1e-300).unwrap().get(), 1e-300);
        assert_eq!(Factor::new(0.0), Err(FactorError::NotPositive));
        assert_eq!(Factor::new(-0.0), Err(FactorError::NotPositive));
        assert_eq!(Factor::new(-2.0), Err(FactorError::NotPositive));
        assert_eq!(Factor::new(f64::INFINITY), Err(FactorError::NotFinite));
    }

    #[test]
    fn angles_leave_out_both_ends() {
        assert!(TriangleAngle::new(90.0).is_ok());
        for v in [0.0, 180.0, -30.0, f64::NAN, f64::INFINITY] {
            assert_eq!(TriangleAngle::new(v), Err(OutOfRange), "{v}");
        }
    }

    #[test]
    fn opacity_keeps_both_ends() {
        assert_eq!(Opacity::new(0.0).unwrap(), Opacity::TRANSPARENT);
        assert_eq!(Opacity::new(1.0).unwrap(), Opacity::OPAQUE);
        for v in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert_eq!(Opacity::new(v), Err(OutOfRange), "{v}");
        }
    }

    #[test]
    fn wedge_angle_takes_both_turns() {
        assert_eq!(WedgeAngle::new(360.0).unwrap().get(), 360.0);
        assert_eq!(WedgeAngle::new(-360.0).unwrap().get(), -360.0);
        assert_eq!(WedgeAngle::new(-90.0).unwrap().get(), -90.0);
        assert_eq!(WedgeAngle::new(0.0), Err(WedgeAngleError::Zero));
        assert_eq!(WedgeAngle::new(-0.0), Err(WedgeAngleError::Zero));
        assert_eq!(WedgeAngle::new(400.0), Err(WedgeAngleError::OutOfRange));
        assert_eq!(WedgeAngle::new(-360.5), Err(WedgeAngleError::OutOfRange));
        for v in [f64::NAN, f64::INFINITY] {
            assert_eq!(WedgeAngle::new(v), Err(WedgeAngleError::OutOfRange), "{v}");
        }
    }

    #[test]
    fn counts_refuse_less_than_the_minimum_and_more_than_the_maximum() {
        assert_eq!(SideCount::new(3).unwrap().get(), 3);
        assert_eq!(SideCount::new(2), Err(CountError::TooFew));
        assert_eq!(SideCount::new(MAX_VERTICES).unwrap().get(), MAX_VERTICES);
        assert_eq!(SideCount::new(MAX_VERTICES + 1), Err(CountError::TooMany));
        assert_eq!(PointCount::new(2).unwrap().get(), 2);
        assert_eq!(PointCount::new(1), Err(CountError::TooFew));
        assert_eq!(PointCount::new(MAX_VERTICES + 1), Err(CountError::TooMany));
        assert_eq!(StepCount::new(0), Err(CountError::TooFew));
        assert_eq!(StepCount::new(u32::MAX).unwrap().get(), u32::MAX);
    }
}
