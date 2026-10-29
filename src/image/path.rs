//! The commands of a path.

use super::Finite;
use super::geometry::{Bounds, Point};
use crate::scene::{Path, PathBounds, PathStyle};

/// A command of a path, as in SVG.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathCmd(Cmd);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmd {
    Move(Point),
    Line(Point),
    Quad {
        control: Point,
        end: Point,
    },
    Cubic {
        c1: Point,
        c2: Point,
        end: Point,
    },
    Arc {
        rx: f64,
        ry: f64,
        rotation: f64,
        large_arc: bool,
        sweep: bool,
        end: Point,
    },
}

impl PathCmd {
    pub fn move_to(p: Point) -> Self {
        Self(Cmd::Move(p))
    }

    pub fn line_to(p: Point) -> Self {
        Self(Cmd::Line(p))
    }

    pub fn quad_to(control: Point, end: Point) -> Self {
        Self(Cmd::Quad { control, end })
    }

    pub fn cubic_to(c1: Point, c2: Point, end: Point) -> Self {
        Self(Cmd::Cubic { c1, c2, end })
    }

    /// An arc of an ellipse with radii `rx` and `ry`, turned by `rotation`
    /// degrees, as in SVG.
    pub fn arc_to(
        rx: Finite,
        ry: Finite,
        rotation: Finite,
        large_arc: bool,
        sweep: bool,
        end: Point,
    ) -> Self {
        Self::arc(rx.get(), ry.get(), rotation.get(), large_arc, sweep, end)
    }

    /// [`PathCmd::arc_to`] for numbers that a shape computes. A number that
    /// is not finite makes the image an `Overflow`.
    pub(super) fn arc(
        rx: f64,
        ry: f64,
        rotation: f64,
        large_arc: bool,
        sweep: bool,
        end: Point,
    ) -> Self {
        Self(Cmd::Arc {
            rx,
            ry,
            rotation,
            large_arc,
            sweep,
            end,
        })
    }

    pub(super) fn translate(self, by: Point) -> Self {
        let t = |p: Point| p.plus(by);
        Self(match self.0 {
            Cmd::Move(p) => Cmd::Move(t(p)),
            Cmd::Line(p) => Cmd::Line(t(p)),
            Cmd::Quad { control, end } => Cmd::Quad {
                control: t(control),
                end: t(end),
            },
            Cmd::Cubic { c1, c2, end } => Cmd::Cubic {
                c1: t(c1),
                c2: t(c2),
                end: t(end),
            },
            Cmd::Arc {
                rx,
                ry,
                rotation,
                large_arc,
                sweep,
                end,
            } => Cmd::Arc {
                rx,
                ry,
                rotation,
                large_arc,
                sweep,
                end: t(end),
            },
        })
    }

    /// `self` with `point` applied to each point and `arc` to the radii, the
    /// rotation and the sweep of an arc.
    pub(super) fn map(
        self,
        point: impl Fn(Point) -> Point,
        arc: impl FnOnce(f64, f64, f64, bool) -> (f64, f64, f64, bool),
    ) -> Self {
        Self(match self.0 {
            Cmd::Move(p) => Cmd::Move(point(p)),
            Cmd::Line(p) => Cmd::Line(point(p)),
            Cmd::Quad { control, end } => Cmd::Quad {
                control: point(control),
                end: point(end),
            },
            Cmd::Cubic { c1, c2, end } => Cmd::Cubic {
                c1: point(c1),
                c2: point(c2),
                end: point(end),
            },
            Cmd::Arc {
                rx,
                ry,
                rotation,
                large_arc,
                sweep,
                end,
            } => {
                let (rx, ry, rotation, sweep) = arc(rx, ry, rotation, sweep);
                Cmd::Arc {
                    rx,
                    ry,
                    rotation,
                    large_arc,
                    sweep,
                    end: point(end),
                }
            }
        })
    }

    pub(super) fn is_finite(self) -> bool {
        match self.0 {
            Cmd::Move(p) | Cmd::Line(p) => p.is_finite(),
            Cmd::Quad { control, end } => control.is_finite() && end.is_finite(),
            Cmd::Cubic { c1, c2, end } => c1.is_finite() && c2.is_finite() && end.is_finite(),
            Cmd::Arc { rx, ry, end, .. } => rx.is_finite() && ry.is_finite() && end.is_finite(),
        }
    }
}

/// The bounds of what the path from `start` through `commands` draws, or
/// `None` when it draws nothing. As in the drawing, a move that no segment
/// leaves adds nothing.
pub(super) fn bounds(start: Point, commands: &[PathCmd]) -> Option<Bounds> {
    let p = |p: Point| (without_residue(p.x), without_residue(p.y));
    let (x, y) = p(start);
    let mut bounds = PathBounds::new(x, y);
    for cmd in commands {
        bounds = match cmd.0 {
            Cmd::Move(end) => {
                let (x, y) = p(end);
                bounds.move_to(x, y)
            }
            Cmd::Line(end) => {
                let (x, y) = p(end);
                bounds.line_to(x, y)
            }
            Cmd::Quad { control, end } => {
                let ((cx, cy), (x, y)) = (p(control), p(end));
                bounds.quad_to(cx, cy, x, y)
            }
            Cmd::Cubic { c1, c2, end } => {
                let ((c1x, c1y), (c2x, c2y), (x, y)) = (p(c1), p(c2), p(end));
                bounds.cubic_to(c1x, c1y, c2x, c2y, x, y)
            }
            Cmd::Arc {
                rx,
                ry,
                rotation,
                large_arc,
                sweep,
                end,
            } => {
                let (x, y) = p(end);
                bounds.arc_to(
                    without_residue(rx),
                    without_residue(ry),
                    without_residue(rotation),
                    large_arc,
                    sweep,
                    x,
                    y,
                )
            }
        };
    }
    let drawn = bounds.get()?;
    Some(Bounds {
        min: Point {
            x: drawn.min_x,
            y: drawn.min_y,
        },
        max: Point {
            x: drawn.max_x,
            y: drawn.max_y,
        },
    })
}

/// The path from `start` through `commands` with `style`, with `point`
/// applied to each point.
pub(super) fn draw(
    start: Point,
    commands: &[PathCmd],
    style: PathStyle,
    point: impl Fn(Point) -> (f32, f32),
) -> Path {
    let (x, y) = point(start);
    let mut builder = Path::builder(style, x, y);
    for cmd in commands {
        builder = match cmd.0 {
            Cmd::Move(end) => {
                let (x, y) = point(end);
                builder.move_to(x, y)
            }
            Cmd::Line(end) => {
                let (x, y) = point(end);
                builder.line_to(x, y)
            }
            Cmd::Quad { control, end } => {
                let ((cx, cy), (x, y)) = (point(control), point(end));
                builder.quad_to(cx, cy, x, y)
            }
            Cmd::Cubic { c1, c2, end } => {
                let ((c1x, c1y), (c2x, c2y), (x, y)) = (point(c1), point(c2), point(end));
                builder.cubic_to(c1x, c1y, c2x, c2y, x, y)
            }
            Cmd::Arc {
                rx,
                ry,
                rotation,
                large_arc,
                sweep,
                end,
            } => {
                let (x, y) = point(end);
                let v = |v: f64| without_residue(v) as f32;
                builder.arc_to(v(rx), v(ry), v(rotation), large_arc, sweep, x, y)
            }
        };
    }
    builder.build()
}

/// How far from 0, or from a whole number, a coordinate may be and still
/// count as one.
pub(super) const RESIDUE: f64 = 1e-10;

/// `v`, or 0 when `v` is within [`RESIDUE`] of 0. A sine or a cosine leaves
/// a residue near 0 that differs from one libm to another, and the drawing
/// takes the residue as 0.
pub(super) fn without_residue(v: f64) -> f64 {
    if v.abs() < RESIDUE { 0.0 } else { v }
}
