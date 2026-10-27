//! The font of a text.

use std::sync::Arc;

use super::Length;
use crate::scene::{FontStyle, TextSpec};

#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    pub family: Arc<str>,
    pub size: Length,
    pub style: FontStyle,
    pub weight: FontWeight,
    pub underline: bool,
}

impl Default for Font {
    /// A regular sans-serif of size 14.
    fn default() -> Self {
        Self {
            family: "sans-serif".into(),
            size: Length::new(14.0).expect("14 is a length"),
            style: FontStyle::Normal,
            weight: FontWeight::Regular,
            underline: false,
        }
    }
}

impl Font {
    /// The spec of `text` in `self`. The measure and the drawing of a text
    /// both read the font here, so they agree on its box.
    pub(super) fn spec(&self, text: &str) -> TextSpec {
        TextSpec {
            size: self.size.get() as f32,
            family: self.family.as_ref().into(),
            weight: self.weight.into(),
            font_style: self.style,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontWeight {
    Light,
    #[default]
    Regular,
    Bold,
}

impl From<FontWeight> for u16 {
    /// The weight on the CSS scale, from 100 to 900, which a scene takes.
    fn from(weight: FontWeight) -> Self {
        match weight {
            FontWeight::Light => 300,
            FontWeight::Regular => 400,
            FontWeight::Bold => 700,
        }
    }
}
