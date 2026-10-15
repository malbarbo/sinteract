//! How a shape or a text paints its fill and its stroke.

use std::sync::Arc;

use super::{Color, Length, Opacity, Overflow};
use crate::scene::{FillRule, LineCap, LineJoin};

/// A list of attributes. Of two attributes that set the same thing, the
/// first one wins.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style(Arc<[Attribute]>);

impl Style {
    pub fn new(attributes: impl Into<Arc<[Attribute]>>) -> Self {
        Self(attributes.into())
    }

    /// The attributes of every style in `styles`, in order.
    pub fn join<'a>(styles: impl IntoIterator<Item = &'a Style>) -> Self {
        Self(
            styles
                .into_iter()
                .flat_map(|s| s.0.iter().cloned())
                .collect(),
        )
    }

    pub fn attributes(&self) -> &[Attribute] {
        &self.0
    }

    /// `self` for a shape scaled by `factor`, or `Overflow` when a width
    /// becomes infinite. A stroke with no width of its own stays one pixel
    /// wide, as an outline does in 2htdp.
    pub(super) fn scale(&self, factor: f64) -> Result<Self, Overflow> {
        let scale = |v: Length| Length::new(v.get() * factor).map_err(|_| Overflow);
        let attributes = self.0.iter().map(|a| {
            Ok(match a {
                Attribute::StrokeWidth(v) => Attribute::StrokeWidth(scale(*v)?),
                Attribute::StrokeDashArray(values) => Attribute::StrokeDashArray(
                    values.iter().map(|v| scale(*v)).collect::<Result<_, _>>()?,
                ),
                _ => a.clone(),
            })
        });
        Ok(Self(attributes.collect::<Result<_, _>>()?))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Attribute {
    Fill(Color),
    FillOpacity(Opacity),
    FillRule(FillRule),
    Stroke(Color),
    StrokeWidth(Length),
    StrokeOpacity(Opacity),
    StrokeDashArray(Box<[Length]>),
    StrokeLineCap(LineCap),
    StrokeLineJoin(LineJoin),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_keeps_the_order() {
        let red = Style::new([Attribute::Fill(Color::rgb(255, 0, 0))]);
        let blue = Style::new([
            Attribute::Fill(Color::rgb(0, 0, 255)),
            Attribute::FillRule(FillRule::EvenOdd),
        ]);
        let joined = Style::join([&red, &Style::default(), &blue]);
        assert_eq!(
            joined.attributes(),
            [
                Attribute::Fill(Color::rgb(255, 0, 0)),
                Attribute::Fill(Color::rgb(0, 0, 255)),
                Attribute::FillRule(FillRule::EvenOdd),
            ]
        );
    }

    #[test]
    fn scale_widens_the_stroke_and_the_dash() {
        let length = |v| Length::new(v).unwrap();
        let style = |w: f64| {
            Style::new([
                Attribute::Fill(Color::rgb(255, 0, 0)),
                Attribute::StrokeWidth(length(w)),
                Attribute::StrokeDashArray([length(w), length(1.0)].into()),
            ])
        };
        let scaled = Style::new([
            Attribute::Fill(Color::rgb(255, 0, 0)),
            Attribute::StrokeWidth(length(6.0)),
            Attribute::StrokeDashArray([length(6.0), length(3.0)].into()),
        ]);
        assert_eq!(style(2.0).scale(3.0), Ok(scaled));
        assert_eq!(style(1e300).scale(1e10), Err(Overflow));
    }
}
