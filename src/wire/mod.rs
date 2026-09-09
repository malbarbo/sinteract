//! The wire format, in three layers.
//!
//! [`scene`] and [`event`] convert the values of [`crate::scene`] and
//! [`crate::event`] to and from the Cap'n Proto structs. Neither knows that
//! a session exists. [`protocol`] wraps a payload in the `Message` union
//! and unwraps it again, and owns everything about the session. A private
//! third module decodes a frame straight onto a renderer, for
//! [`Renderer::render_stream`](crate::renderer::Renderer::render_stream).
//!
//! The generated bindings stay private, and a caller goes through
//! [`encode_frame`], [`encode_event`], [`encode_asset`], [`encode_close`]
//! and [`decode`]. The bytes are the standard `serialize::write_message`
//! format, so every Cap'n Proto binding reads them.
//!
//! [`framing`] is below all of them. It wraps an encoded message in the
//! envelope that a byte stream needs to tell one message from the next.
//!
//! The schema files in `schema/` are the source of truth, one per layer,
//! and the header of `scene.capnp` says how to regenerate the bindings.

pub mod event;
pub mod framing;
pub mod protocol;
pub mod scene;
mod stream;

pub use protocol::{Decoded, decode, encode_asset, encode_close, encode_event, encode_frame};
pub use stream::Error as StreamError;
pub(crate) use stream::stream_frame;

/// Serialize a finished builder. `write_message` into a `Vec` cannot fail.
pub(crate) fn finish(builder: capnp::message::Builder<capnp::message::HeapAllocator>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(256);
    capnp::serialize::write_message(&mut bytes, &builder)
        .expect("write_message into Vec is infallible");
    bytes
}

/// A payload is malformed. It says the scene, the event or the message is
/// unusable, and never that the session is. A server that gets one from
/// [`decode`] drops the message and keeps the peer.
#[derive(Debug)]
pub enum Error {
    /// Cap'n Proto rejected the bytes as malformed, truncated, or of the
    /// wrong root.
    Parse(capnp::Error),
    /// A union discriminant matches no known variant.
    UnknownVariant(&'static str, u16),
    /// A required nested struct or list is unset.
    MissingField(&'static str),
    /// The verbs of a `Path` claim a number of floats that its coords do not
    /// hold.
    PathLengthMismatch { verbs: usize, coords: usize },
    /// A `Path` carries a verb byte this crate does not know.
    UnknownVerb(u8),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "parse error: {e}"),
            Error::UnknownVariant(name, tag) => write!(f, "unknown {name} discriminant: {tag}"),
            Error::MissingField(name) => write!(f, "missing required field: {name}"),
            Error::PathLengthMismatch { verbs, coords } => {
                write!(
                    f,
                    "path verbs ({verbs} bytes) and coords ({coords} floats) disagree"
                )
            }
            Error::UnknownVerb(v) => write!(f, "unknown path verb byte: {v}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<capnp::Error> for Error {
    fn from(e: capnp::Error) -> Self {
        Error::Parse(e)
    }
}

impl From<capnp::NotInSchema> for Error {
    fn from(e: capnp::NotInSchema) -> Self {
        Error::UnknownVariant("enum", e.0)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Parse(capnp::Error::failed(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{InputEvent, KeyEvent, KeyKind};
    use crate::protocol_capnp::message;
    use crate::scene::{
        Bitmap, ClipPath, Dash, Element, FillRule, FontStyle, Gradient, LineCap, LineJoin, Paint,
        PathStyle, Rgba, Scene, Segment, SegmentKind, SpreadMode, Stop, TextNode,
    };
    use capnp::message::Builder as MessageBuilder;

    fn sample_scene() -> Scene {
        let mut scene = Scene::new(120.0, 80.0);
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::rgba(10, 20, 30, 0.5),
                stroke: Paint::rgba(200, 0, 0, 1.0),
                stroke_width: 2.5,
                line_cap: LineCap::Round,
                line_join: LineJoin::Bevel,
                fill_rule: FillRule::EvenOdd,
                closed: true,
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(10.0, 0.0);
            p.quad_to(15.0, 5.0, 20.0, 10.0);
            p.cubic_to(25.0, 5.0, 30.0, 15.0, 35.0, 20.0);
        }
        {
            let mut clip = scene.clip_rect(50.0, 50.0, 30.0, 20.0, 15.0, FillRule::EvenOdd);
            clip.text(TextNode {
                fill: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 1.0,
                },
                transform: crate::scene::text_box_affine(
                    "Liberation Sans",
                    700,
                    FontStyle::Italic,
                    12.0,
                    "Olá",
                    60.0,
                    30.0,
                    50.0,
                    14.0,
                    0.0,
                ),
                size: 12.0,
                family: "Liberation Sans".into(),
                weight: 700,
                style: FontStyle::Italic,
                underline: true,
                text: "Olá".into(),
                ..TextNode::default()
            });
            clip.bitmap(Bitmap {
                id: 7,
                // 64×64 asset, mirrored horizontally, rotated 90°, centred at (70, 40).
                transform: crate::scene::bitmap_box_affine(64, 64, 70.0, 40.0, -32.0, 32.0, 90.0),
            });
        }
        scene
    }

    fn assert_scene_eq(a: &Scene, b: &Scene) {
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
        assert_eq!(a.elements.len(), b.elements.len(), "node count");
        for (i, (x, y)) in a.elements.iter().zip(b.elements.iter()).enumerate() {
            assert_eq!(format!("{x:?}"), format!("{y:?}"), "node {i}");
        }
    }

    #[test]
    fn frame_round_trip_preserves_drawlist() {
        let scene = sample_scene();
        let bytes = encode_frame(&scene);
        match decode(&bytes).expect("decode") {
            Decoded::Frame(d) => assert_scene_eq(&scene, &d),
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn a_scene_round_trips_without_the_envelope() {
        let scene = sample_scene();
        let bytes = scene::encode(&scene);
        assert_scene_eq(&scene::decode(&bytes).expect("decode"), &scene);
        assert!(
            bytes.len() < encode_frame(&scene).len(),
            "a bare scene should be smaller than the same scene in a Message"
        );
    }

    #[test]
    fn empty_drawlist_round_trips() {
        let scene = Scene::new(640.0, 480.0);
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                assert_eq!(d.width, 640.0);
                assert_eq!(d.height, 480.0);
                assert!(d.elements.is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn key_event_round_trip() {
        let ev = InputEvent::Key(KeyEvent {
            kind: KeyKind::Down,
            key: "ArrowLeft".into(),
            modifiers: crate::event::modifiers(false, true, true, false, false),
        });
        let bytes = encode_event(&ev);
        match decode(&bytes).unwrap() {
            Decoded::Event(InputEvent::Key(k)) => {
                assert_eq!(k.kind, KeyKind::Down);
                assert_eq!(k.key, "ArrowLeft");
                assert!(k.ctrl());
                assert!(k.shift());
                assert!(!k.alt());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn vsync_and_close_events_round_trip() {
        match decode(&encode_event(&InputEvent::Vsync)).unwrap() {
            Decoded::Event(ev) => assert!(ev.is_vsync()),
            _ => panic!(),
        }
        match decode(&encode_event(&InputEvent::Close)).unwrap() {
            Decoded::Event(ev) => assert!(ev.is_close()),
            _ => panic!(),
        }
    }

    #[test]
    fn asset_round_trip_carries_payload() {
        let blob: Vec<u8> = (0u8..=255).collect();
        let bytes = encode_asset(42, &blob, Some("image/png"));
        match decode(&bytes).unwrap() {
            Decoded::Asset {
                id,
                blob: out,
                mime,
            } => {
                assert_eq!(id, 42);
                assert_eq!(out, blob);
                assert_eq!(mime.as_deref(), Some("image/png"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn close_message_round_trips() {
        let bytes = encode_close();
        assert!(matches!(decode(&bytes).unwrap(), Decoded::Close));
    }

    #[test]
    fn garbage_bytes_fail_to_decode() {
        let r = decode(&[0u8; 4]);
        assert!(r.is_err(), "expected decode error, got {r:?}");
    }

    #[test]
    fn a_path_with_coords_no_verb_claims_is_rejected() {
        // One Move claims 2 floats and 4 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            let _ = p.reborrow().init_style();
            p.set_verbs(&[SegmentKind::Move as u8]);
            let mut coords = p.init_coords(4);
            for i in 0..4 {
                coords.set(i, i as f32);
            }
        }
        let bytes = finish(builder);
        let err = decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::PathLengthMismatch {
                    verbs: 1,
                    coords: 4
                }
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn malformed_path_is_rejected() {
        // One Cubic claims 6 floats and 4 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            let _ = p.reborrow().init_style();
            p.set_verbs(&[SegmentKind::Cubic as u8]);
            let mut coords = p.init_coords(4);
            for i in 0..4 {
                coords.set(i, i as f32);
            }
        }
        let bytes = finish(builder);
        let err = decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::PathLengthMismatch {
                    verbs: 1,
                    coords: 4
                }
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn dash_and_miter_round_trip() {
        let mut scene = Scene::new(100.0, 50.0);
        {
            let mut p = scene.path(PathStyle {
                stroke: Paint::rgba(0, 0, 0, 1.0),
                stroke_width: 2.0,
                miter_limit: 7.5,
                dash: Dash::new(vec![4.0, 2.0, 1.0], 1.5).map(Box::new),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let node = d.elements.first().expect("one node");
                let Element::Path(p) = node else {
                    panic!("expected path");
                };
                assert_eq!(p.style.miter_limit, 7.5);
                let dash = p.style.dash.as_ref().unwrap();
                assert_eq!(dash.array(), [4.0, 2.0, 1.0]);
                assert_eq!(dash.offset(), 1.5);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn linear_gradient_paint_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Gradient::linear(
            0.0,
            0.0,
            50.0,
            0.0,
            vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 0.5,
                    color: Rgba {
                        r: 0,
                        g: 255,
                        b: 0,
                        a: 0.8,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 1.0,
                    },
                },
            ],
        );
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(gradient.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::gradient(gradient));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn radial_gradient_paint_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Gradient::radial(
            25.0,
            25.0,
            20.0,
            vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 255,
                        b: 255,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0.0,
                    },
                },
            ],
        );
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(gradient.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::gradient(gradient));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn gradient_spread_mode_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let linear = Gradient::linear(
            0.0,
            0.0,
            25.0,
            0.0,
            vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 255,
                        g: 0,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba {
                        r: 0,
                        g: 0,
                        b: 255,
                        a: 1.0,
                    },
                },
            ],
        )
        .with_spread(SpreadMode::Reflect);
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(linear.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let radial = Gradient::radial(
            25.0,
            25.0,
            10.0,
            vec![
                Stop {
                    offset: 0.0,
                    color: Rgba {
                        r: 0,
                        g: 255,
                        b: 0,
                        a: 1.0,
                    },
                },
                Stop {
                    offset: 1.0,
                    color: Rgba::default(),
                },
            ],
        )
        .with_spread(SpreadMode::Repeat);
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::gradient(radial.clone()),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p0) = &d.elements[0] else {
                    panic!();
                };
                assert_eq!(p0.style.fill, Paint::gradient(linear));
                let Element::Path(p1) = &d.elements[1] else {
                    panic!();
                };
                assert_eq!(p1.style.fill, Paint::gradient(radial));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn clip_path_round_trips() {
        // Built without clip_rect, so the walk sees a quadratic and the
        // even-odd rule.
        let mut scene = Scene::new(50.0, 50.0);
        {
            let mut clip = scene.clip(
                ClipPath::builder(FillRule::EvenOdd)
                    .move_to(0.0, 0.0)
                    .line_to(30.0, 0.0)
                    .quad_to(40.0, 25.0, 30.0, 40.0)
                    .line_to(0.0, 40.0)
                    .build(),
            );
            let mut p = clip.path(PathStyle::default());
            p.move_to(0.0, 0.0);
            p.line_to(10.0, 10.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Clipped { clip, elements } = &d.elements[0] else {
                    panic!("expected Clipped, got {:?}", d.elements[0]);
                };
                let segs: Vec<_> = clip.segments().collect();
                assert_eq!(
                    segs,
                    [
                        Segment::Move { x: 0.0, y: 0.0 },
                        Segment::Line { x: 30.0, y: 0.0 },
                        Segment::Quad {
                            cx: 40.0,
                            cy: 25.0,
                            x: 30.0,
                            y: 40.0
                        },
                        Segment::Line { x: 0.0, y: 40.0 },
                    ]
                );
                assert_eq!(clip.fill_rule, FillRule::EvenOdd);
                assert_eq!(elements.len(), 1);
                assert!(matches!(&elements[0], Element::Path(_)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn malformed_clip_path_is_rejected() {
        // One Cubic claims 6 floats and 2 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let clipped = node.init_clipped();
            let mut c = clipped.init_clip();
            c.set_verbs(&[SegmentKind::Cubic as u8]);
            let mut coords = c.init_coords(2);
            coords.set(0, 0.0);
            coords.set(1, 0.0);
        }
        let bytes = finish(builder);
        let err = decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                Error::PathLengthMismatch {
                    verbs: 1,
                    coords: 2
                }
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn nested_clips_round_trip() {
        let mut scene = Scene::new(100.0, 100.0);
        {
            let mut outer = scene.clip_rect(50.0, 50.0, 80.0, 80.0, 0.0, FillRule::NonZero);
            let mut p = outer.path(PathStyle::default());
            p.move_to(0.0, 0.0);
            p.line_to(100.0, 100.0);
            drop(p);
            let mut inner = outer.clip_rect(50.0, 50.0, 40.0, 40.0, 0.0, FillRule::EvenOdd);
            let mut p = inner.path(PathStyle::default());
            p.move_to(10.0, 10.0);
            p.line_to(20.0, 20.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                assert_eq!(d.elements.len(), 1);
                let Element::Clipped {
                    clip: _,
                    elements: outer_els,
                } = &d.elements[0]
                else {
                    panic!("expected outer Clipped");
                };
                assert_eq!(outer_els.len(), 2, "outer should hold path + inner clip");
                assert!(matches!(&outer_els[0], Element::Path(_)));
                let Element::Clipped {
                    clip: _,
                    elements: inner_els,
                } = &outer_els[1]
                else {
                    panic!("expected inner Clipped, got {:?}", outer_els[1]);
                };
                assert_eq!(inner_els.len(), 1);
                assert!(matches!(&inner_els[0], Element::Path(_)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn default_style_uses_solid_transparent_paint() {
        let mut scene = Scene::new(10.0, 10.0);
        {
            let mut p = scene.path(PathStyle::default());
            p.move_to(0.0, 0.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements.first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, Paint::Solid(Rgba::default()));
                assert_eq!(p.style.stroke, Paint::Solid(Rgba::default()));
                assert!(p.style.dash.is_none());
                assert_eq!(p.style.miter_limit, crate::scene::DEFAULT_MITER_LIMIT);
            }
            _ => panic!(),
        }
    }
}
