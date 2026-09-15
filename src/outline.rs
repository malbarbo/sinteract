//! The sink that paths, clips and glyphs are outlined into.

use crate::scene::{Segment, Segments};

/// Receives an outline: the segments of a path or of a clip, or the glyphs
/// and the underline of a text. Each backend implements it once.
pub(crate) trait PathSink {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32);
    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32);
    fn close(&mut self);
}

impl Segments<'_> {
    /// Feeds the segments to `out`, so a backend draws paths, clips and glyph
    /// outlines through one sink.
    pub(crate) fn outline(self, out: &mut impl PathSink) {
        for seg in self {
            match seg {
                Segment::Move { x, y } => out.move_to(x, y),
                Segment::Line { x, y } => out.line_to(x, y),
                Segment::Quad { cx, cy, x, y } => out.quad_to(cx, cy, x, y),
                Segment::Cubic {
                    c1x,
                    c1y,
                    c2x,
                    c2y,
                    x,
                    y,
                } => out.cubic_to(c1x, c1y, c2x, c2y, x, y),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Records the ops, so a test asserts them exactly.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub(crate) ops: Vec<String>,
    }

    impl Recorder {
        /// The number of ops of one kind, by its letter.
        pub(crate) fn count(&self, op: char) -> usize {
            self.ops.iter().filter(|o| o.starts_with(op)).count()
        }
    }

    impl PathSink for Recorder {
        fn move_to(&mut self, x: f32, y: f32) {
            self.ops.push(format!("M {x} {y}"));
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.ops.push(format!("L {x} {y}"));
        }
        fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
            self.ops.push(format!("Q {cx} {cy} {x} {y}"));
        }
        fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
            self.ops.push(format!("C {c1x} {c1y} {c2x} {c2y} {x} {y}"));
        }
        fn close(&mut self) {
            self.ops.push("Z".to_string());
        }
    }
}
