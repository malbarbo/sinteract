//! The sink that paths, clips and glyphs are outlined into.

use crate::scene::{Path, Segment, Segments};

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
            feed(seg, out);
        }
    }
}

impl Path {
    /// Feeds the segments to `out`. A closed path closes every sub-path, so
    /// the stroke joins at the start of each one.
    pub(crate) fn outline(&self, out: &mut impl PathSink) {
        if !self.style.closed {
            return self.segments().outline(out);
        }
        let mut open = false;
        for seg in self.segments() {
            if open && matches!(seg, Segment::Move { .. }) {
                out.close();
            }
            feed(seg, out);
            open = true;
        }
        if open {
            out.close();
        }
    }
}

fn feed(seg: Segment, out: &mut impl PathSink) {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::scene::PathStyle;

    fn two_squares(closed: bool) -> Path {
        let style = PathStyle {
            closed,
            ..PathStyle::default()
        };
        Path::builder(style, 0.0, 0.0)
            .line_to(9.0, 0.0)
            .line_to(9.0, 9.0)
            .move_to(3.0, 3.0)
            .line_to(6.0, 3.0)
            .line_to(6.0, 6.0)
            .build()
    }

    #[test]
    fn a_closed_path_closes_every_sub_path() {
        let mut out = Recorder::default();
        two_squares(true).outline(&mut out);
        assert_eq!(
            out.ops,
            [
                "M 0 0", "L 9 0", "L 9 9", "Z", "M 3 3", "L 6 3", "L 6 6", "Z"
            ]
        );
    }

    #[test]
    fn an_open_path_closes_no_sub_path() {
        let mut out = Recorder::default();
        two_squares(false).outline(&mut out);
        assert_eq!(out.count('Z'), 0);
    }

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
