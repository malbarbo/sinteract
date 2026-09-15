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
/// [`decode`] drops the message and keeps the peer. A value from a newer
/// schema and a float that is not finite are not errors, since the decoders
/// skip what holds them.
#[derive(Debug)]
pub enum Error {
    /// Cap'n Proto rejected the bytes as malformed, truncated, or of the
    /// wrong root.
    Parse(capnp::Error),
    /// The verbs of a `Path` claim a number of floats that its coords do not
    /// hold.
    PathLengthMismatch { verbs: usize, coords: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "parse error: {e}"),
            Error::PathLengthMismatch { verbs, coords } => {
                write!(
                    f,
                    "path verbs ({verbs} bytes) and coords ({coords} floats) disagree"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<capnp::Error> for Error {
    fn from(e: capnp::Error) -> Self {
        Error::Parse(e)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(e: std::str::Utf8Error) -> Self {
        Error::Parse(capnp::Error::failed(e.to_string()))
    }
}

/// How reading the values inside an element or an event fails. Only
/// `skip_unusable` looks inside, so no decoder returns it.
enum ReadError {
    /// The bytes are damaged, and the whole payload is unusable.
    Malformed(Error),
    /// A paint arm, an enum value or a verb byte from a newer schema. The
    /// reader skips the element or the event that holds it.
    Newer,
    /// A float that is not finite, which draws nothing. The reader skips the
    /// element that holds it.
    NotFinite,
}

impl From<Error> for ReadError {
    fn from(e: Error) -> Self {
        ReadError::Malformed(e)
    }
}

impl From<capnp::Error> for ReadError {
    fn from(e: capnp::Error) -> Self {
        ReadError::Malformed(e.into())
    }
}

impl From<capnp::NotInSchema> for ReadError {
    fn from(_: capnp::NotInSchema) -> Self {
        ReadError::Newer
    }
}

impl From<std::str::Utf8Error> for ReadError {
    fn from(e: std::str::Utf8Error) -> Self {
        ReadError::Malformed(e.into())
    }
}

/// `None` when reading an element or an event met a value from a newer
/// schema or a float that is not finite, so the reader skips what holds it.
/// Damage stays an error.
fn skip_unusable<T>(read: Result<T, ReadError>) -> Result<Option<T>, Error> {
    match read {
        Ok(v) => Ok(Some(v)),
        Err(ReadError::Newer | ReadError::NotFinite) => Ok(None),
        Err(ReadError::Malformed(e)) => Err(e),
    }
}

/// Overwrite the two bytes that `find` points at with a value this crate
/// does not know, as a peer with a newer schema writes it. `find` points at
/// the tag of a union, which every union of the schema keeps at offset 0 of
/// its data section, at an enum field, or at the first two verbs of a path.
#[cfg(test)]
pub(crate) fn with_unknown_value(
    bytes: &[u8],
    find: impl FnOnce(crate::protocol_capnp::message::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    let mut words = capnp::Word::allocate_zeroed_vec(bytes.len() / 8);
    capnp::Word::words_to_bytes_mut(&mut words).copy_from_slice(bytes);
    let mut out = capnp::Word::words_to_bytes(&words).to_vec();
    let at = {
        let buf = capnp::Word::words_to_bytes(&words);
        let msg = capnp::serialize::read_message_from_flat_slice_no_alloc(
            &mut &buf[..],
            capnp::message::ReaderOptions::new(),
        )
        .expect("parse");
        find(msg.get_root().expect("root")) as usize - buf.as_ptr() as usize
    };
    out[at..at + 2].copy_from_slice(&0xfff0u16.to_le_bytes());
    out
}

/// Where the data section of a struct starts, which is the tag of a union,
/// for [`with_unknown_value`].
#[cfg(test)]
pub(crate) fn tag_of<'a>(r: impl capnp::traits::IntoInternalStructReader<'a>) -> *const u8 {
    capnp::raw::get_struct_data_section(r).as_ptr()
}

/// The scene of a `Message::Frame`, for [`with_unknown_value`].
#[cfg(test)]
pub(crate) fn frame_of(
    m: crate::protocol_capnp::message::Reader<'_>,
) -> crate::scene_capnp::scene::Reader<'_> {
    match m.which() {
        Ok(crate::protocol_capnp::message::Frame(f)) => f.expect("frame"),
        _ => panic!("not a frame"),
    }
}

/// Replace every float `from` in `bytes` with `to`, as a peer that writes a
/// float that is not finite does. A `Scene` never holds one, so a test
/// encodes a marker and swaps it. Cap'n Proto aligns a float to 4 bytes.
#[cfg(test)]
pub(crate) fn with_float(bytes: &[u8], from: f32, to: f32) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let mut swapped = false;
    for chunk in out.as_chunks_mut::<4>().0 {
        if *chunk == from.to_le_bytes() {
            *chunk = to.to_le_bytes();
            swapped = true;
        }
    }
    assert!(swapped, "no {from} in the bytes");
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{InputEvent, KeyEvent, KeyKind, Modifiers};
    use crate::event_capnp::input_event;
    use crate::protocol_capnp::message;
    use crate::scene::{
        Bitmap, ClipPath, Dash, Element, FillRule, FontStyle, Gradient, LineCap, LineJoin, Paint,
        Path, PathStyle, Rgba, RotatedRect, Scene, Segment, SegmentKind, SpreadMode, Stop, Text,
        TextSpec,
    };
    use crate::scene_capnp::element;
    use capnp::message::Builder as MessageBuilder;

    fn sample_scene() -> Scene {
        let mut scene = Scene::new(120.0, 80.0);
        {
            let mut p = scene.path(
                PathStyle {
                    fill: Paint::rgba(10, 20, 30, 0.5),
                    stroke: Paint::rgba(200, 0, 0, 1.0),
                    stroke_width: 2.5,
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Bevel,
                    fill_rule: FillRule::EvenOdd,
                    closed: true,
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(10.0, 0.0);
            p.quad_to(15.0, 5.0, 20.0, 10.0);
            p.cubic_to(25.0, 5.0, 30.0, 15.0, 35.0, 20.0);
        }
        {
            let mut clip = scene.clip(RotatedRect {
                cx: 50.0,
                cy: 50.0,
                w: 30.0,
                h: 20.0,
                angle_deg: 15.0,
            });
            let text = TextSpec {
                size: 12.0,
                family: "Liberation Sans".into(),
                weight: 700,
                style: FontStyle::Italic,
                text: "Olá".into(),
            }
            .fit(RotatedRect {
                cx: 60.0,
                cy: 30.0,
                w: 50.0,
                h: 14.0,
                angle_deg: 0.0,
            })
            .expect("text fits");
            clip.text(Text {
                fill: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 1.0,
                },
                underline: true,
                ..text
            });
            // 64×64 asset, mirrored horizontally, rotated 90°, centred at (70, 40).
            clip.bitmap(Bitmap::fit(
                7,
                64,
                64,
                RotatedRect {
                    cx: 70.0,
                    cy: 40.0,
                    w: -32.0,
                    h: 32.0,
                    angle_deg: 90.0,
                },
            ));
        }
        scene
    }

    fn assert_scene_eq(a: &Scene, b: &Scene) {
        assert_eq!(a.width(), b.width());
        assert_eq!(a.height(), b.height());
        assert_eq!(a.elements().len(), b.elements().len(), "node count");
        for (i, (x, y)) in a.elements().iter().zip(b.elements().iter()).enumerate() {
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
                assert_eq!(d.width(), 640.0);
                assert_eq!(d.height(), 480.0);
                assert!(d.elements().is_empty());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn key_event_round_trip() {
        let key = KeyEvent {
            kind: KeyKind::Down,
            key: "ArrowLeft".into(),
            modifiers: Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::default()
            },
            repeat: true,
        };
        match decode(&encode_event(&InputEvent::Key(key.clone()))).unwrap() {
            Decoded::Event(InputEvent::Key(k)) => assert_eq!(k, key),
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
    fn a_path_with_no_initial_move_begins_at_the_origin() {
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            let _ = p.reborrow().init_style();
            p.set_verbs(&[SegmentKind::Line as u8, SegmentKind::Line as u8]);
            let mut coords = p.init_coords(4);
            for (i, v) in [5.0, 10.0, 20.0, 0.0].into_iter().enumerate() {
                coords.set(i as u32, v);
            }
        }
        let Decoded::Frame(d) = decode(&finish(builder)).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Path(p)] = d.elements() else {
            panic!("expected one Path, got {:?}", d.elements());
        };
        let expected = Path::builder(PathStyle::default(), 0.0, 0.0)
            .line_to(5.0, 10.0)
            .line_to(20.0, 0.0)
            .build();
        assert!(p.segments().eq(expected.segments()), "{p:?}");
    }

    #[test]
    fn a_move_of_the_wire_that_no_segment_follows_is_dropped() {
        // move move line move, then a path of one move.
        let paths: [(&[SegmentKind], &[f32]); 2] = [
            (
                &[
                    SegmentKind::Move,
                    SegmentKind::Move,
                    SegmentKind::Line,
                    SegmentKind::Move,
                ],
                &[1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0],
            ),
            (&[SegmentKind::Move], &[5.0, 5.0]),
        ];
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(paths.len() as u32);
            for (i, (verbs, xs)) in paths.iter().enumerate() {
                let node = nodes.reborrow().get(i as u32);
                let mut p = node.init_path();
                let _ = p.reborrow().init_style();
                let verbs: Vec<u8> = verbs.iter().map(|&k| k as u8).collect();
                p.set_verbs(&verbs);
                let mut coords = p.init_coords(xs.len() as u32);
                for (j, &v) in xs.iter().enumerate() {
                    coords.set(j as u32, v);
                }
            }
        }
        let Decoded::Frame(d) = decode(&finish(builder)).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Path(p), Element::Path(lone)] = d.elements() else {
            panic!("expected two Paths, got {:?}", d.elements());
        };
        let expected = Path::builder(PathStyle::default(), 2.0, 2.0)
            .line_to(3.0, 3.0)
            .build();
        assert!(p.segments().eq(expected.segments()), "{p:?}");
        assert_eq!(lone.segments().len(), 0, "{lone:?}");
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
    fn an_element_of_an_unknown_arm_is_skipped() {
        // A path, then a clip that holds a path and a bitmap. The first path
        // and the bitmap become arms of a newer schema.
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(Path::builder(PathStyle::default(), 0.0, 0.0).build());
        {
            let mut clip = scene.clip(RotatedRect {
                cx: 5.0,
                cy: 5.0,
                w: 10.0,
                h: 10.0,
                angle_deg: 0.0,
            });
            clip.add_path(Path::builder(PathStyle::default(), 1.0, 1.0).build());
            clip.bitmap(Bitmap::fit(
                7,
                4,
                4,
                RotatedRect {
                    cx: 5.0,
                    cy: 5.0,
                    w: 4.0,
                    h: 4.0,
                    angle_deg: 0.0,
                },
            ));
        }
        let bytes = with_unknown_value(&encode_frame(&scene), |m| {
            tag_of(frame_of(m).get_elements().unwrap().get(0))
        });
        let bytes = with_unknown_value(&bytes, |m| {
            let Ok(element::Which::Clipped(c)) = frame_of(m).get_elements().unwrap().get(1).which()
            else {
                panic!("expected Clipped");
            };
            tag_of(c.unwrap().get_elements().unwrap().get(1))
        });
        let Decoded::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Clipped { elements, .. }] = d.elements() else {
            panic!("expected one Clipped, got {:?}", d.elements());
        };
        assert!(matches!(&elements[..], [Element::Path(_)]), "{elements:?}");
    }

    #[test]
    fn a_message_of_an_unknown_arm_decodes_as_unknown() {
        let bytes = with_unknown_value(&encode_close(), |m| tag_of(m));
        assert!(matches!(decode(&bytes).unwrap(), Decoded::Unknown));
    }

    #[test]
    fn an_event_of_an_unknown_arm_decodes_as_unknown() {
        let bytes = with_unknown_value(&encode_event(&InputEvent::Vsync), |m| match m.which() {
            Ok(message::Event(e)) => tag_of(e.unwrap()),
            _ => panic!("not an event"),
        });
        assert!(matches!(decode(&bytes).unwrap(), Decoded::Unknown));
    }

    fn element_at(m: message::Reader<'_>, i: u32) -> element::WhichReader<'_> {
        let Ok(which) = frame_of(m).get_elements().unwrap().get(i).which() else {
            panic!("element {i} of an unknown arm");
        };
        which
    }

    fn path_at(m: message::Reader<'_>, i: u32) -> crate::scene_capnp::path::Reader<'_> {
        let element::Which::Path(p) = element_at(m, i) else {
            panic!("element {i} is not a Path");
        };
        p.unwrap()
    }

    #[test]
    fn an_element_that_holds_an_unknown_value_is_skipped() {
        // Four paths and a clip. The first three paths get a paint arm, a
        // line cap and verbs of a newer schema, and the clip gets verbs of a
        // newer schema. Only the last path, at (9, 9), is left.
        let mut scene = Scene::new(10.0, 10.0);
        for _ in 0..3 {
            let mut p = scene.path(PathStyle::default(), 0.0, 0.0);
            p.line_to(1.0, 1.0);
        }
        scene
            .clip(RotatedRect {
                cx: 5.0,
                cy: 5.0,
                w: 10.0,
                h: 10.0,
                angle_deg: 0.0,
            })
            .add_path(Path::builder(PathStyle::default(), 1.0, 1.0).build());
        scene.add_path(
            Path::builder(PathStyle::default(), 9.0, 9.0)
                .line_to(10.0, 10.0)
                .build(),
        );

        let bytes = with_unknown_value(&encode_frame(&scene), |m| {
            tag_of(path_at(m, 0).get_style().unwrap().get_fill().unwrap())
        });
        // The line cap is the u16 at byte 4 of the data of a PathStyle.
        let bytes = with_unknown_value(&bytes, |m| {
            tag_of(path_at(m, 1).get_style().unwrap()).wrapping_add(4)
        });
        let bytes = with_unknown_value(&bytes, |m| path_at(m, 2).get_verbs().unwrap().as_ptr());
        let bytes = with_unknown_value(&bytes, |m| {
            let element::Which::Clipped(c) = element_at(m, 3) else {
                panic!("expected Clipped");
            };
            c.unwrap().get_clip().unwrap().get_verbs().unwrap().as_ptr()
        });

        let Decoded::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Path(p)] = d.elements() else {
            panic!("expected one Path, got {:?}", d.elements());
        };
        assert_eq!(
            p.segments().collect::<Vec<_>>(),
            [
                Segment::Move { x: 9.0, y: 9.0 },
                Segment::Line { x: 10.0, y: 10.0 }
            ]
        );
    }

    #[test]
    fn an_event_that_holds_an_unknown_value_decodes_as_unknown() {
        let key = InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: "a".into(),
            modifiers: Modifiers::default(),
            repeat: false,
        });
        // The kind is the u16 at byte 0 of the data of a KeyEvent.
        let bytes = with_unknown_value(&encode_event(&key), |m| {
            let Ok(message::Event(e)) = m.which() else {
                panic!("not an event");
            };
            let Ok(input_event::Which::Key(k)) = e.unwrap().which() else {
                panic!("not a key");
            };
            tag_of(k.unwrap())
        });
        assert!(matches!(decode(&bytes).unwrap(), Decoded::Unknown));
    }

    #[test]
    fn a_paint_of_an_unknown_arm_draws_its_fallback_color() {
        // The writer of a newer schema sets the fallback, which this crate
        // never writes, so the path is built by hand.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<message::Builder>();
            let frame = msg.init_frame();
            let mut nodes = frame.init_elements(1);
            let node = nodes.reborrow().get(0);
            let mut p = node.init_path();
            let mut fill = p.reborrow().init_style().init_fill();
            fill.set_fallback(0x3366_ff80);
            fill.set_has_fallback(true);
            let _ = fill.init_solid();
            p.set_verbs(&[SegmentKind::Move as u8]);
            let mut coords = p.init_coords(2);
            coords.set(0, 1.0);
            coords.set(1, 1.0);
        }
        let bytes = with_unknown_value(&finish(builder), |m| {
            tag_of(path_at(m, 0).get_style().unwrap().get_fill().unwrap())
        });

        let Decoded::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Path(p)] = d.elements() else {
            panic!("expected one Path, got {:?}", d.elements());
        };
        let color = Rgba {
            r: 0x33,
            g: 0x66,
            b: 0xff,
            a: 128.0 / 255.0,
        };
        assert_eq!(p.style.fill, Paint::Solid(color));
    }

    #[test]
    fn dash_and_miter_round_trip() {
        let mut scene = Scene::new(100.0, 50.0);
        {
            let mut p = scene.path(
                PathStyle {
                    stroke: Paint::rgba(0, 0, 0, 1.0),
                    stroke_width: 2.0,
                    miter_limit: 7.5,
                    dash: Dash::new(vec![4.0, 2.0, 1.0], 1.5).map(Box::new),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let node = d.elements().first().expect("one node");
                let Element::Path(p) = node else {
                    panic!("expected path");
                };
                assert_eq!(p.style.miter_limit, 7.5);
                let dash = p.style.dash.as_ref().unwrap();
                assert_eq!(dash.array(), [4.0, 2.0, 1.0, 4.0, 2.0, 1.0]);
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
            let mut p = scene.path(
                PathStyle {
                    fill: Paint::gradient(gradient.clone()),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 0.0);
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements().first().unwrap() else {
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
            let mut p = scene.path(
                PathStyle {
                    fill: Paint::gradient(gradient.clone()),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements().first().unwrap() else {
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
            let mut p = scene.path(
                PathStyle {
                    fill: Paint::gradient(linear.clone()),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
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
            let mut p = scene.path(
                PathStyle {
                    fill: Paint::gradient(radial.clone()),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p0) = &d.elements()[0] else {
                    panic!();
                };
                assert_eq!(p0.style.fill, Paint::gradient(linear));
                let Element::Path(p1) = &d.elements()[1] else {
                    panic!();
                };
                assert_eq!(p1.style.fill, Paint::gradient(radial));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn clip_path_round_trips() {
        // Built from a path, so the walk sees a quadratic and the even-odd
        // rule.
        let mut scene = Scene::new(50.0, 50.0);
        {
            let mut clip = scene.clip(
                ClipPath::builder(FillRule::EvenOdd, 0.0, 0.0)
                    .line_to(30.0, 0.0)
                    .quad_to(40.0, 25.0, 30.0, 40.0)
                    .line_to(0.0, 40.0)
                    .build(),
            );
            let mut p = clip.path(PathStyle::default(), 0.0, 0.0);
            p.line_to(10.0, 10.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Clipped { clip, elements } = &d.elements()[0] else {
                    panic!("expected Clipped, got {:?}", d.elements()[0]);
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
            let mut outer = scene.clip(RotatedRect {
                cx: 50.0,
                cy: 50.0,
                w: 80.0,
                h: 80.0,
                angle_deg: 0.0,
            });
            let mut p = outer.path(PathStyle::default(), 0.0, 0.0);
            p.line_to(100.0, 100.0);
            drop(p);
            let mut inner = outer.clip(RotatedRect {
                cx: 50.0,
                cy: 50.0,
                w: 40.0,
                h: 40.0,
                angle_deg: 0.0,
            });
            let mut p = inner.path(PathStyle::default(), 10.0, 10.0);
            p.line_to(20.0, 20.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                assert_eq!(d.elements().len(), 1);
                let Element::Clipped {
                    clip: _,
                    elements: outer_els,
                } = &d.elements()[0]
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
        scene.add_path(Path::builder(PathStyle::default(), 0.0, 0.0).build());
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Decoded::Frame(d) => {
                let Element::Path(p) = d.elements().first().unwrap() else {
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

    #[test]
    fn an_element_that_holds_a_non_finite_float_is_skipped() {
        // Every element but the last two marks a float that becomes NaN on
        // the wire: a coordinate, a stroke width, a clip, a path inside a
        // clip, a text and a bitmap. The clip around a good path and the path
        // at (9, 9) are left.
        let mark = 777.0;
        let line = |style, x| Path::builder(style, 0.0, 0.0).line_to(x, 1.0).build();
        let rect = RotatedRect {
            cx: 5.0,
            cy: 5.0,
            w: 10.0,
            h: 10.0,
            angle_deg: 0.0,
        };
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(line(PathStyle::default(), mark));
        scene.add_path(line(
            PathStyle {
                stroke_width: mark,
                ..PathStyle::default()
            },
            1.0,
        ));
        scene
            .clip(
                ClipPath::builder(FillRule::NonZero, mark, 0.0)
                    .line_to(1.0, 1.0)
                    .build(),
            )
            .add_path(line(PathStyle::default(), 1.0));
        {
            let mut clip = scene.clip(rect);
            clip.add_path(line(PathStyle::default(), mark));
            clip.add_path(line(PathStyle::default(), 1.0));
        }
        scene.text(Text {
            stroke_width: mark,
            ..Text::default()
        });
        scene.bitmap(Bitmap {
            id: 1,
            transform: [1.0, 0.0, 0.0, 1.0, mark, 0.0],
        });
        scene.add_path(
            Path::builder(PathStyle::default(), 9.0, 9.0)
                .line_to(10.0, 10.0)
                .build(),
        );

        let bytes = with_float(&encode_frame(&scene), mark, f32::NAN);
        let Decoded::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Clipped { elements, .. }, Element::Path(p)] = d.elements() else {
            panic!("expected a Clipped and a Path, got {:?}", d.elements());
        };
        assert_eq!(elements.len(), 1, "{elements:?}");
        assert_eq!(p.segments().next(), Some(Segment::Move { x: 9.0, y: 9.0 }));
    }
}
