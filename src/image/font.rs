//! The font of a text.

use super::Length;
use crate::scene::FontStyle;

#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    pub family: Box<str>,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontWeight {
    Light,
    #[default]
    Regular,
    Bold,
}
