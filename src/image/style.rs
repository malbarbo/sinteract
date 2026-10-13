//! How a shape or a text paints its fill and its stroke.

use super::{Color, Length, Opacity};
use crate::scene::{FillRule, LineCap, LineJoin};

/// A list of attributes. Of two attributes that set the same thing, the
/// first one wins.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style(Box<[Attribute]>);

impl Style {
    pub fn new(attributes: impl Into<Box<[Attribute]>>) -> Self {
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
}
