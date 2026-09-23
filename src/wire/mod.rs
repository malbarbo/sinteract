//! The wire format, in three layers.
//!
//! [`scene`] and [`event`] convert the values of [`crate::scene`] and
//! [`crate::event`] to and from the Cap'n Proto structs. Neither knows that
//! a session exists. [`to_view`], [`to_server`] and [`to_engine`] wrap the
//! payloads in the message of their direction and unwrap them again, and
//! own everything about the session. A private third module decodes a
//! scene straight onto a renderer, for
//! [`Renderer::render_stream`](crate::renderer::Renderer::render_stream).
//!
//! The engine runs the program and writes with [`to_view`]. A view draws
//! the frames, sends the input and writes with [`to_server`]. The server
//! owns the session, passes the input of the views on and writes with
//! [`to_engine`], and so does a view that talks to the engine alone. Each
//! side reads with the module of the side that writes to it. The generated
//! bindings stay private, and the bytes are the standard
//! `serialize::write_message` format, so every Cap'n Proto binding reads
//! them.
//!
//! [`framing`] is below all of them. It wraps an encoded message in the
//! envelope that a byte stream needs to tell one message from the next.
//!
//! The schema files in `schema/` are the source of truth, one per layer,
//! and the header of `scene.capnp` says how to regenerate the bindings.

pub mod event;
pub mod framing;
mod protocol;
pub mod scene;
mod stream;
pub mod to_engine;
pub mod to_server;
pub mod to_view;

pub use protocol::ReadError;
pub use stream::Error as StreamError;
pub(crate) use stream::stream_frame;

/// Serialize a finished builder. `write_message` into a `Vec` cannot fail.
pub(crate) fn finish(builder: capnp::message::Builder<capnp::message::HeapAllocator>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(256);
    capnp::serialize::write_message(&mut bytes, &builder)
        .expect("write_message into Vec is infallible");
    bytes
}

/// A payload is malformed, or does not agree with its header. It says the scene, the event or the message is
/// unusable, and never that the session is. An engine that gets one from
/// [`to_engine::read`] drops the message and keeps the session. A value from a
/// newer schema and a float that is not finite are not errors, since the
/// decoders skip what holds them.
#[derive(Debug)]
pub enum Error {
    /// Cap'n Proto rejected the bytes as malformed, truncated, or of the
    /// wrong root.
    Parse(capnp::Error),
    /// The verbs of a `Path` claim a number of floats that its coords do not
    /// hold.
    PathLengthMismatch { verbs: usize, coords: usize },
    /// A join, a leave or a member of a roster has player 0, which is not
    /// a player of the session.
    NoPlayer,
    /// A roster has a player twice.
    DuplicatePlayer(to_engine::DuplicatePlayer),
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
            Error::NoPlayer => write!(f, "a join, a leave or a member has player 0"),
            Error::DuplicatePlayer(e) => write!(f, "{e}"),
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
enum ValueError {
    /// The bytes are damaged, and the whole payload is unusable.
    Malformed(Error),
    /// A paint arm, an enum value or a verb byte from a newer schema. The
    /// reader skips the element or the event that holds it.
    Newer,
    /// A float that is not finite. The reader skips the element or the
    /// event that holds it.
    NotFinite,
}

impl From<Error> for ValueError {
    fn from(e: Error) -> Self {
        ValueError::Malformed(e)
    }
}

impl From<capnp::Error> for ValueError {
    fn from(e: capnp::Error) -> Self {
        ValueError::Malformed(e.into())
    }
}

impl From<capnp::NotInSchema> for ValueError {
    fn from(_: capnp::NotInSchema) -> Self {
        ValueError::Newer
    }
}

impl From<std::str::Utf8Error> for ValueError {
    fn from(e: std::str::Utf8Error) -> Self {
        ValueError::Malformed(e.into())
    }
}

/// `None` when reading an element or an event met a value from a newer
/// schema or a float that is not finite, so the reader skips what holds it.
/// Damage stays an error.
fn skip_unusable<T>(read: Result<T, ValueError>) -> Result<Option<T>, Error> {
    match read {
        Ok(v) => Ok(Some(v)),
        Err(ValueError::Newer | ValueError::NotFinite) => Ok(None),
        Err(ValueError::Malformed(e)) => Err(e),
    }
}

/// [`with_unknown_value`] for the bytes of an `EngineMessage`.
#[cfg(test)]
pub(crate) fn with_unknown_engine_value(
    bytes: &[u8],
    find: impl FnOnce(crate::protocol_capnp::engine_message::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<crate::protocol_capnp::engine_message::Owned>(bytes, find)
}

/// [`with_unknown_value`] for the bytes of a `ServerMessage`.
#[cfg(test)]
pub(crate) fn with_unknown_server_value(
    bytes: &[u8],
    find: impl FnOnce(crate::protocol_capnp::server_message::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<crate::protocol_capnp::server_message::Owned>(bytes, find)
}

/// [`with_unknown_value`] for the bytes of a `ViewMessage`.
#[cfg(test)]
pub(crate) fn with_unknown_view_value(
    bytes: &[u8],
    find: impl FnOnce(crate::protocol_capnp::view_message::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<crate::protocol_capnp::view_message::Owned>(bytes, find)
}

/// [`with_unknown_value`] for the bytes of a bare `Scene`.
#[cfg(test)]
pub(crate) fn with_unknown_scene_value(
    bytes: &[u8],
    find: impl FnOnce(crate::scene_capnp::scene::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<crate::scene_capnp::scene::Owned>(bytes, find)
}

/// Overwrite the two bytes that `find` points at with a value this crate
/// does not know, as a peer with a newer schema writes it. `find` points at
/// the tag of a union, at an enum field, or at the first two verbs of a
/// path.
/// `bytes` holds a message whose root is `T`, and the compiler cannot infer
/// `T` from `find`, so each root has a wrapper.
#[cfg(test)]
fn with_unknown_value<T: capnp::traits::Owned>(
    bytes: &[u8],
    find: impl FnOnce(T::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    // The reader needs the bytes aligned to words.
    let mut words = capnp::Word::allocate_zeroed_vec(bytes.len() / 8);
    capnp::Word::words_to_bytes_mut(&mut words).copy_from_slice(bytes);
    let buf = capnp::Word::words_to_bytes(&words);
    let msg = capnp::serialize::read_message_from_flat_slice_no_alloc(
        &mut &buf[..],
        capnp::message::ReaderOptions::new(),
    )
    .expect("parse");
    let at = find(msg.get_root().expect("root")) as usize - buf.as_ptr() as usize;
    let mut out = bytes.to_vec();
    out[at..at + 2].copy_from_slice(&0xfff0u16.to_le_bytes());
    out
}

/// Where the data section of a struct starts, for [`with_unknown_value`].
/// A union with no field before it keeps its tag there.
#[cfg(test)]
pub(crate) fn tag_of<'a>(r: impl capnp::traits::IntoInternalStructReader<'a>) -> *const u8 {
    capnp::raw::get_struct_data_section(r).as_ptr()
}

/// The scene of a frame, for [`with_unknown_engine_value`].
#[cfg(test)]
pub(crate) fn frame_of(
    m: crate::protocol_capnp::engine_message::Reader<'_>,
) -> crate::scene_capnp::scene::Reader<'_> {
    let Ok(crate::protocol_capnp::engine_message::Frame(f)) = m.which() else {
        panic!("not a frame");
    };
    f.expect("frame")
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
    use super::to_engine::encode_input as encode_event;
    use super::to_view::{Message, encode_asset, encode_close, encode_frame};
    use super::*;
    use crate::event::{
        InputEvent, KeyEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons,
        MouseEvent, PadButton, PadEvent,
    };
    use crate::event_capnp::input_event;
    use crate::protocol_capnp::{engine_message, server_message};
    use crate::scene::{
        Bitmap, ClipPath, Dash, Element, FillRule, FontStyle, LineCap, LineJoin, Paint, Path,
        PathStyle, Rgba, RotatedRect, Scene, Segment, SegmentKind, SpreadMode, Stop, Text,
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

    /// Decode a message of the engine of an arm this schema knows, as
    /// [`to_view::read`] does after the envelope.
    fn decode(bytes: &[u8]) -> Result<Message, Error> {
        Ok(to_view::decode(&words(bytes))?.expect("an arm this schema knows"))
    }

    /// Whether [`to_view::read`] skips the payload in `bytes`.
    fn is_skipped(bytes: &[u8]) -> bool {
        matches!(to_view::decode(&words(bytes)), Ok(None))
    }

    /// Decode a message of the server of an arm this schema knows, as
    /// [`to_engine::read`] does after the envelope.
    fn decode_event(bytes: &[u8]) -> Result<InputEvent, Error> {
        match to_engine::decode(framing::UNROUTED, &words(bytes))?
            .expect("an arm this schema knows")
        {
            to_engine::Message::Input { event, .. } => Ok(event),
            other => panic!("got {other:?}"),
        }
    }

    /// Whether [`to_engine::read`] skips the payload in `bytes`.
    fn is_event_skipped(bytes: &[u8]) -> bool {
        matches!(
            to_engine::decode(framing::UNROUTED, &words(bytes)),
            Ok(None)
        )
    }

    fn nonzero(n: u32) -> std::num::NonZeroU32 {
        std::num::NonZeroU32::new(n).unwrap()
    }

    fn member(player: u32, nickname: &str) -> to_engine::Member {
        to_engine::Member {
            player: nonzero(player),
            nickname: nickname.into(),
        }
    }

    /// `bytes` in the words that the decoder reads in place.
    fn words(bytes: &[u8]) -> Vec<capnp::Word> {
        let mut words = capnp::Word::allocate_zeroed_vec(bytes.len().div_ceil(8));
        capnp::Word::words_to_bytes_mut(&mut words)[..bytes.len()].copy_from_slice(bytes);
        words
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
            Message::Frame(d) => assert_scene_eq(&scene, &d),
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
            "a bare scene should be smaller than the same scene in an EngineMessage"
        );
    }

    #[test]
    fn empty_drawlist_round_trips() {
        let scene = Scene::new(640.0, 480.0);
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Message::Frame(d) => {
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
        match decode_event(&encode_event(&InputEvent::Key(key.clone()))).unwrap() {
            InputEvent::Key(k) => assert_eq!(k, key),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn mouse_event_round_trip() {
        let actions = [
            MouseAction::Move,
            MouseAction::Down(MouseButton::Right),
            MouseAction::Up(MouseButton::Forward),
            MouseAction::Wheel { dx: -0.5, dy: 2.0 },
            MouseAction::Leave,
        ];
        for action in actions {
            let mouse = MouseEvent {
                action,
                x: -3.5,
                y: 480.25,
                modifiers: Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
                buttons: MouseButtons::default()
                    .with(MouseButton::Left)
                    .with(MouseButton::Back),
            };
            match decode_event(&encode_event(&InputEvent::Mouse(mouse))).unwrap() {
                InputEvent::Mouse(m) => assert_eq!(m, mouse),
                other => panic!("got {other:?}"),
            }
        }
    }

    #[test]
    fn pad_event_round_trip() {
        let buttons = [
            PadButton::Up,
            PadButton::Down,
            PadButton::Left,
            PadButton::Right,
            PadButton::A,
            PadButton::B,
            PadButton::X,
            PadButton::Y,
            PadButton::LeftShoulder,
            PadButton::RightShoulder,
            PadButton::Select,
            PadButton::Start,
        ];
        let events = buttons
            .into_iter()
            .flat_map(|b| [PadEvent::Down(b), PadEvent::Up(b)])
            .chain([PadEvent::Connected, PadEvent::Disconnected]);
        for pad in events {
            match decode_event(&encode_event(&InputEvent::Pad(pad))).unwrap() {
                InputEvent::Pad(p) => assert_eq!(p, pad),
                other => panic!("got {other:?}"),
            }
        }
    }

    #[test]
    fn resize_event_round_trip() {
        let resize = InputEvent::Resize {
            width: 800.0,
            height: 600.5,
        };
        match decode_event(&encode_event(&resize)).unwrap() {
            InputEvent::Resize { width, height } => assert_eq!((width, height), (800.0, 600.5)),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_vsync_and_a_close_of_the_view_round_trip() {
        assert!(matches!(
            decode_event(&encode_event(&InputEvent::Vsync)),
            Ok(InputEvent::Vsync)
        ));
        assert!(matches!(
            to_engine::decode(framing::UNROUTED, &words(&to_engine::encode_close())),
            Ok(Some(to_engine::Message::Close))
        ));
    }

    #[test]
    fn the_messages_of_a_view_round_trip_with_their_header() {
        let mut stream = Vec::new();
        to_server::write_input(&mut stream, &InputEvent::Vsync).unwrap();
        to_server::write_close(&mut stream).unwrap();
        assert_eq!(&stream[..4], b"SIV1");
        let mut r = &stream[..];
        assert!(matches!(
            to_server::read(&mut r),
            Ok(Some(to_server::Message::Input(InputEvent::Vsync)))
        ));
        assert!(matches!(
            to_server::read(&mut r),
            Ok(Some(to_server::Message::Close))
        ));
        assert!(matches!(to_server::read(&mut r), Ok(None)));
    }

    #[test]
    fn a_message_of_a_view_of_an_unknown_arm_is_skipped() {
        let bytes = with_unknown_view_value(&to_server::encode_close(), |m| tag_of(m));
        assert!(matches!(to_server::decode(&words(&bytes)), Ok(None)));
    }

    #[test]
    fn the_players_of_the_server_round_trip_with_their_header() {
        let roster = to_engine::Roster::new(vec![member(1, "Ana"), member(2, "Beto")]).unwrap();
        let mut stream = Vec::new();
        to_engine::write_start(&mut stream, &roster).unwrap();
        to_engine::write_join(&mut stream, nonzero(3), "Caio").unwrap();
        to_engine::write_leave(&mut stream, nonzero(2)).unwrap();
        let mut r = &stream[..];
        let mut next = || to_engine::read(&mut r).unwrap().expect("a message");
        match next() {
            to_engine::Message::Start(got) => assert_eq!(got, roster),
            other => panic!("got {other:?}"),
        }
        match next() {
            to_engine::Message::Join { player, nickname } => {
                assert_eq!((player, nickname.as_str()), (nonzero(3), "Caio"));
            }
            other => panic!("got {other:?}"),
        }
        match next() {
            to_engine::Message::Leave { player } => assert_eq!(player, nonzero(2)),
            other => panic!("got {other:?}"),
        }
        assert!(to_engine::read(&mut r).unwrap().is_none());
    }

    #[test]
    fn a_roster_does_not_take_a_player_twice() {
        let members = vec![member(1, "Ana"), member(2, "Beto"), member(1, "Caio")];
        assert_eq!(
            to_engine::Roster::new(members),
            Err(to_engine::DuplicatePlayer(nonzero(1)))
        );
    }

    #[test]
    fn a_join_or_a_leave_of_player_0_is_an_error_and_the_session_goes_on() {
        let mut join = Vec::new();
        to_engine::write_join(&mut join, nonzero(3), "Caio").unwrap();
        let mut leave = Vec::new();
        to_engine::write_leave(&mut leave, nonzero(3)).unwrap();
        let mut stream = Vec::new();
        for mut message in [join, leave] {
            message[4..8].fill(0);
            stream.extend_from_slice(&message);
        }
        to_engine::write_close(&mut stream).unwrap();
        let mut r = &stream[..];
        for _ in 0..2 {
            assert!(matches!(
                to_engine::read(&mut r),
                Err(ReadError::Payload(Error::NoPlayer))
            ));
        }
        assert!(matches!(
            to_engine::read(&mut r),
            Ok(Some(to_engine::Message::Close))
        ));
    }

    #[test]
    fn a_start_with_player_0_or_a_player_twice_is_an_error() {
        let start = |players: &[u32]| {
            let mut builder = MessageBuilder::new_default();
            let start = builder.init_root::<server_message::Builder>().init_start();
            let mut list = start.init_members(players.len() as u32);
            for (i, &p) in players.iter().enumerate() {
                list.reborrow().get(i as u32).set_player(p);
            }
            words(&finish(builder))
        };
        assert!(matches!(
            to_engine::decode(framing::UNROUTED, &start(&[1, 0])),
            Err(Error::NoPlayer)
        ));
        assert!(matches!(
            to_engine::decode(framing::UNROUTED, &start(&[2, 1, 2])),
            Err(Error::DuplicatePlayer(to_engine::DuplicatePlayer(p))) if p == nonzero(2)
        ));
    }

    #[test]
    fn asset_round_trip_carries_payload() {
        let blob: Vec<u8> = (0u8..=255).collect();
        let bytes = encode_asset(42, &blob, Some("image/png"));
        match decode(&bytes).unwrap() {
            Message::Asset {
                id,
                blob: out,
                mime,
            } => {
                assert_eq!(id, 42);
                assert_eq!(out, blob);
                assert_eq!(mime.as_deref(), Some("image/png"));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn close_message_round_trips() {
        let bytes = encode_close();
        assert!(matches!(decode(&bytes).unwrap(), Message::Close));
    }

    #[test]
    fn garbage_bytes_fail_to_decode() {
        let r = decode(&[0xff; 8]);
        assert!(r.is_err(), "expected decode error, got {r:?}");
    }

    #[test]
    fn a_path_with_coords_no_verb_claims_is_rejected() {
        // One Move claims 2 floats and 4 are present.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<engine_message::Builder>();
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
            let msg = builder.init_root::<engine_message::Builder>();
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
        let Message::Frame(d) = decode(&finish(builder)).unwrap() else {
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
            let msg = builder.init_root::<engine_message::Builder>();
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
        let Message::Frame(d) = decode(&finish(builder)).unwrap() else {
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
            let msg = builder.init_root::<engine_message::Builder>();
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
        let bytes = with_unknown_engine_value(&encode_frame(&scene), |m| {
            tag_of(frame_of(m).get_elements().unwrap().get(0))
        });
        let bytes = with_unknown_engine_value(&bytes, |m| {
            let Ok(element::Which::Clipped(c)) = frame_of(m).get_elements().unwrap().get(1).which()
            else {
                panic!("expected Clipped");
            };
            tag_of(c.unwrap().get_elements().unwrap().get(1))
        });
        let Message::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Clipped { elements, .. }] = d.elements() else {
            panic!("expected one Clipped, got {:?}", d.elements());
        };
        assert!(matches!(&elements[..], [Element::Path(_)]), "{elements:?}");
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_skipped() {
        let bytes = with_unknown_engine_value(&encode_close(), |m| tag_of(m));
        assert!(is_skipped(&bytes));
    }

    #[test]
    fn an_event_of_an_unknown_arm_is_skipped() {
        let bytes = with_unknown_server_value(&encode_event(&InputEvent::Vsync), |m| {
            let Ok(server_message::Event(e)) = m.which() else {
                panic!("not an event");
            };
            tag_of(e.unwrap())
        });
        assert!(is_event_skipped(&bytes));
    }

    fn element_at(m: engine_message::Reader<'_>, i: u32) -> element::WhichReader<'_> {
        let Ok(which) = frame_of(m).get_elements().unwrap().get(i).which() else {
            panic!("element {i} of an unknown arm");
        };
        which
    }

    fn path_at(m: engine_message::Reader<'_>, i: u32) -> crate::scene_capnp::path::Reader<'_> {
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

        let bytes = with_unknown_engine_value(&encode_frame(&scene), |m| {
            tag_of(path_at(m, 0).get_style().unwrap().get_fill().unwrap())
        });
        // The line cap is the u16 at byte 4 of the data of a PathStyle.
        let bytes = with_unknown_engine_value(&bytes, |m| {
            tag_of(path_at(m, 1).get_style().unwrap()).wrapping_add(4)
        });
        let bytes =
            with_unknown_engine_value(&bytes, |m| path_at(m, 2).get_verbs().unwrap().as_ptr());
        let bytes = with_unknown_engine_value(&bytes, |m| {
            let element::Which::Clipped(c) = element_at(m, 3) else {
                panic!("expected Clipped");
            };
            c.unwrap().get_clip().unwrap().get_verbs().unwrap().as_ptr()
        });

        let Message::Frame(d) = decode(&bytes).unwrap() else {
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
    fn an_event_that_holds_an_unknown_value_is_skipped() {
        let key = InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: "a".into(),
            modifiers: Modifiers::default(),
            repeat: false,
        });
        // The kind is the u16 at byte 0 of the data of a KeyEvent.
        let bytes = with_unknown_server_value(&encode_event(&key), |m| {
            let Ok(server_message::Event(e)) = m.which() else {
                panic!("not an event");
            };
            let Ok(input_event::Which::Key(k)) = e.unwrap().which() else {
                panic!("not a key");
            };
            tag_of(k.unwrap())
        });
        assert!(is_event_skipped(&bytes));
    }

    #[test]
    fn a_mouse_event_of_an_unknown_action_or_button_is_skipped() {
        let down = InputEvent::Mouse(MouseEvent {
            action: MouseAction::Down(MouseButton::Left),
            x: 1.0,
            y: 2.0,
            modifiers: Modifiers::default(),
            buttons: MouseButtons::default().with(MouseButton::Left),
        });
        // The tag of the action is the u16 at byte 10 of the data of a
        // MouseEvent, and the button of a down is the u16 at byte 12.
        for offset in [10, 12] {
            let bytes = with_unknown_server_value(&encode_event(&down), |m| {
                let Ok(server_message::Event(e)) = m.which() else {
                    panic!("not an event");
                };
                let Ok(input_event::Which::Mouse(m)) = e.unwrap().which() else {
                    panic!("not a mouse event");
                };
                tag_of(m.unwrap()).wrapping_add(offset)
            });
            assert!(is_event_skipped(&bytes), "offset {offset}");
        }
    }

    #[test]
    fn a_pad_event_of_an_unknown_action_or_button_is_skipped() {
        let down = InputEvent::Pad(PadEvent::Down(PadButton::A));
        // The button of a down is the u16 at byte 0 of the data of a
        // PadEvent, and the tag of the union is the u16 at byte 2.
        for offset in [0, 2] {
            let bytes = with_unknown_server_value(&encode_event(&down), |m| {
                let Ok(server_message::Event(e)) = m.which() else {
                    panic!("not an event");
                };
                let Ok(input_event::Which::Pad(p)) = e.unwrap().which() else {
                    panic!("not a pad event");
                };
                tag_of(p.unwrap()).wrapping_add(offset)
            });
            assert!(is_event_skipped(&bytes), "offset {offset}");
        }
    }

    #[test]
    fn an_event_that_holds_a_non_finite_float_is_skipped() {
        let mouse = |x, action| {
            InputEvent::Mouse(MouseEvent {
                action,
                x,
                y: 0.0,
                modifiers: Modifiers::default(),
                buttons: MouseButtons::default(),
            })
        };
        let events = [
            mouse(f32::NAN, MouseAction::Move),
            mouse(
                0.0,
                MouseAction::Wheel {
                    dx: 0.0,
                    dy: f32::INFINITY,
                },
            ),
            InputEvent::Resize {
                width: f32::NAN,
                height: 1.0,
            },
        ];
        for ev in events {
            assert!(is_event_skipped(&encode_event(&ev)), "{ev:?}");
        }
    }

    #[test]
    fn a_paint_of_an_unknown_arm_draws_its_fallback_color() {
        // The writer of a newer schema sets the fallback, which this crate
        // never writes, so the path is built by hand.
        let mut builder = MessageBuilder::new_default();
        {
            let msg = builder.init_root::<engine_message::Builder>();
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
        let bytes = with_unknown_engine_value(&finish(builder), |m| {
            tag_of(path_at(m, 0).get_style().unwrap().get_fill().unwrap())
        });

        let Message::Frame(d) = decode(&bytes).unwrap() else {
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
            Message::Frame(d) => {
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
        let gradient = Paint::linear(
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
                    fill: gradient.clone(),
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
            Message::Frame(d) => {
                let Element::Path(p) = d.elements().first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, gradient);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn radial_gradient_paint_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let gradient = Paint::radial(
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
                    fill: gradient.clone(),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Message::Frame(d) => {
                let Element::Path(p) = d.elements().first().unwrap() else {
                    panic!();
                };
                assert_eq!(p.style.fill, gradient);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn gradient_spread_mode_round_trips() {
        let mut scene = Scene::new(50.0, 50.0);
        let linear = Paint::linear(
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
                    fill: linear.clone(),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 50.0);
        }
        let radial = Paint::radial(
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
                    fill: radial.clone(),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(50.0, 50.0);
        }
        let bytes = encode_frame(&scene);
        match decode(&bytes).unwrap() {
            Message::Frame(d) => {
                let Element::Path(p0) = &d.elements()[0] else {
                    panic!();
                };
                assert_eq!(p0.style.fill, linear);
                let Element::Path(p1) = &d.elements()[1] else {
                    panic!();
                };
                assert_eq!(p1.style.fill, radial);
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
            Message::Frame(d) => {
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
            let msg = builder.init_root::<engine_message::Builder>();
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
            Message::Frame(d) => {
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
            Message::Frame(d) => {
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
        let Message::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Clipped { elements, .. }, Element::Path(p)] = d.elements() else {
            panic!("expected a Clipped and a Path, got {:?}", d.elements());
        };
        assert_eq!(elements.len(), 1, "{elements:?}");
        assert_eq!(p.segments().next(), Some(Segment::Move { x: 9.0, y: 9.0 }));
    }

    #[test]
    fn a_decoded_stop_moves_up_to_the_one_before_it() {
        // The second stop becomes 0.125 on the wire, before the first.
        let stops = vec![
            Stop {
                offset: 0.25,
                color: Rgba::default(),
            },
            Stop {
                offset: 0.75,
                color: Rgba::default(),
            },
        ];
        let style = PathStyle {
            fill: Paint::linear(0.0, 0.0, 10.0, 0.0, stops),
            ..PathStyle::default()
        };
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(Path::builder(style, 0.0, 0.0).line_to(10.0, 10.0).build());
        let bytes = with_float(&encode_frame(&scene), 0.75, 0.125);

        let Message::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Path(p)] = d.elements() else {
            panic!("expected one Path, got {:?}", d.elements());
        };
        let Paint::Gradient(g) = &p.style.fill else {
            panic!("expected a gradient, got {:?}", p.style.fill);
        };
        assert_eq!(
            g.stops().iter().map(|s| s.offset).collect::<Vec<_>>(),
            [0.25, 0.25]
        );
    }

    #[test]
    fn a_decoded_miter_limit_below_1_rises_to_1() {
        // The scene raises the limit, so the bytes are patched to hold one
        // below 1, as a writer from elsewhere could send it.
        let style = PathStyle {
            miter_limit: 8.0,
            ..PathStyle::default()
        };
        let mut scene = Scene::new(10.0, 10.0);
        scene.add_path(Path::builder(style, 0.0, 0.0).line_to(10.0, 10.0).build());
        let bytes = with_float(&encode_frame(&scene), 8.0, 0.25);

        let Message::Frame(d) = decode(&bytes).unwrap() else {
            panic!("expected Frame");
        };
        let [Element::Path(p)] = d.elements() else {
            panic!("expected one Path, got {:?}", d.elements());
        };
        assert_eq!(p.style.miter_limit, 1.0);
    }
}
