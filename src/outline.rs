//! The sink that paths, clips and glyphs are outlined into, and the adapter
//! that turns quadratics into cubics for a backend that has no quadratic
//! operator.

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

/// Turns every quadratic into a cubic for a [`PathSink`] that has no
/// quadratic operator. It tracks the current point itself, and `close`
/// returns the point to the start of the subpath, so the backend does not
/// reconstruct it.
pub(crate) struct ElevateQuads<'a, B: ?Sized> {
    inner: &'a mut B,
    start: Option<(f32, f32)>,
    last: Option<(f32, f32)>,
}

impl<'a, B: PathSink + ?Sized> ElevateQuads<'a, B> {
    pub(crate) fn new(inner: &'a mut B) -> Self {
        Self {
            inner,
            start: None,
            last: None,
        }
    }
}

impl<B: PathSink + ?Sized> PathSink for ElevateQuads<'_, B> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = Some((x, y));
        self.last = Some((x, y));
        self.inner.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.last = Some((x, y));
        self.inner.line_to(x, y);
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        // A path that opens on a quadratic has no current point, so the
        // quadratic is dropped. After a close, the current point is the start
        // of the closed subpath.
        let Some(p0) = self.last else { return };
        let (c1x, c1y, c2x, c2y) = quad_to_cubic(p0, cx, cy, x, y);
        self.last = Some((x, y));
        self.inner.cubic_to(c1x, c1y, c2x, c2y, x, y);
    }

    fn cubic_to(&mut self, cx1: f32, cy1: f32, cx2: f32, cy2: f32, x: f32, y: f32) {
        self.last = Some((x, y));
        self.inner.cubic_to(cx1, cy1, cx2, cy2, x, y);
    }

    fn close(&mut self) {
        self.last = self.start;
        self.inner.close();
    }
}

/// The two control points of the cubic equal to the quadratic from `p0`
/// through the control `(cx, cy)` to `(x, y)`.
pub(crate) fn quad_to_cubic(
    p0: (f32, f32),
    cx: f32,
    cy: f32,
    x: f32,
    y: f32,
) -> (f32, f32, f32, f32) {
    let (p0x, p0y) = p0;
    (
        p0x + 2.0 / 3.0 * (cx - p0x),
        p0y + 2.0 / 3.0 * (cy - p0y),
        x + 2.0 / 3.0 * (cx - x),
        y + 2.0 / 3.0 * (cy - y),
    )
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

    #[test]
    fn elevate_quads_tracks_the_point_across_close() {
        // After `close` the current point is the start of the subpath, not
        // the end of the last op.
        let mut sink = Recorder::default();
        {
            let mut out = ElevateQuads::new(&mut sink);
            out.move_to(0.0, 0.0);
            out.line_to(6.0, 0.0);
            out.close();
            out.quad_to(3.0, 3.0, 6.0, 0.0);
        }
        assert_eq!(
            sink.ops,
            vec![
                "M 0 0".to_string(),
                "L 6 0".to_string(),
                "Z".to_string(),
                // anchored at (0, 0), the start of the subpath
                "C 2 2 4 2 6 0".to_string(),
            ]
        );
    }

    #[test]
    fn elevate_quads_drops_a_contour_opening_on_a_quad() {
        let mut sink = Recorder::default();
        {
            let mut out = ElevateQuads::new(&mut sink);
            out.quad_to(3.0, 3.0, 6.0, 0.0);
            out.move_to(1.0, 1.0);
        }
        assert_eq!(sink.ops, vec!["M 1 1".to_string()]);
    }
}
