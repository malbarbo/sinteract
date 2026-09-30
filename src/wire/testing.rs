//! What the tests of the wire, the session and the server read and write. A
//! pipe that does not block, a reader of a stream, a decoder of each
//! message of the engine, and the encoders of the messages that a test
//! sends with no envelope or with a value out of range. The helpers at the end change the bytes of a message
//! as a newer peer, or one that writes a float that is not finite, would.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;
use std::rc::Rc;

use capnp::Word;

use crate::event::InputEvent;
use crate::protocol_capnp;
use crate::scene::{Image, Scene};
use crate::scene_capnp::scene;
use crate::session::PlayerRange;

use super::Error;
use super::engine_to_server;
use super::framing::{HEADER_BYTES, Side, parse_header};
use super::protocol::decode_root;
use super::scene::{read_scene, scene_message};
use super::server_to_engine;

/// A pipe that does not block, which a session reads or writes. A clone
/// shares the bytes, so a test keeps one end and the session holds the
/// other. A read of an empty pipe fails with [`io::ErrorKind::WouldBlock`]
/// until the test closes the pipe, and then returns 0.
#[derive(Clone, Debug, Default)]
pub(crate) struct Pipe(Rc<RefCell<PipeState>>);

#[derive(Debug, Default)]
struct PipeState {
    bytes: VecDeque<u8>,
    closed: bool,
    /// Every write fails.
    broken: bool,
}

impl Pipe {
    pub(crate) fn push(&self, bytes: &[u8]) {
        self.0.borrow_mut().bytes.extend(bytes);
    }

    pub(crate) fn close(&self) {
        self.0.borrow_mut().closed = true;
    }

    /// Make every write from now on fail.
    pub(crate) fn break_writes(&self) {
        self.0.borrow_mut().broken = true;
    }

    /// The bytes that wait in the pipe, which leave it.
    pub(crate) fn drain(&self) -> Vec<u8> {
        self.0.borrow_mut().bytes.drain(..).collect()
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.0.borrow_mut();
        if state.bytes.is_empty() && !state.closed {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        state.bytes.read(buf)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.0.borrow_mut();
        if state.broken {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        state.bytes.extend(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One message of the engine, one variant per arm of `EngineToServer`. A
/// bitmap of the id `n` draws [`crate::asset::png_image`] of `n` by 1, as
/// [`encode_frame`] writes it, and one of the id 0 is skipped.
#[derive(Clone, Debug)]
pub(crate) enum Message {
    Asset {
        id: u32,
        blob: Vec<u8>,
    },
    /// A frame for `player`, or for every player when `player` is `None`.
    Frame {
        player: Option<NonZeroU32>,
        scene: Scene,
    },
    Hello(PlayerRange),
    /// The engine took a tick. It goes to the server alone.
    TickTaken,
}

/// Read the next message of the engine, as [`decode`] does. Returns
/// `None` at the end of the stream.
pub(crate) fn read(r: &mut impl Read) -> Result<Option<Message>, Error> {
    read_next(r, Side::Engine, decode)
}

/// Decode `payload`, a message with no envelope. `None` for a message of an
/// arm from a newer schema.
pub(crate) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<protocol_capnp::engine_to_server::Owned, _>(payload, decode_message)
}

/// The ids of the bitmaps of the frame in `payload`, as
/// [`engine_to_server::arm`] reads them.
pub(crate) fn bitmap_ids(payload: &[u8]) -> BTreeSet<u32> {
    match engine_to_server::arm(payload) {
        Ok(Some(engine_to_server::Arm::Frame { ids, .. })) => ids,
        other => panic!("expected a frame, got {other:?}"),
    }
}

fn decode_message(
    msg: protocol_capnp::engine_to_server::Reader<'_>,
) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        protocol_capnp::engine_to_server::Asset(a) => {
            let a = a?;
            Ok(Some(Message::Asset {
                id: a.get_id(),
                blob: a.get_blob()?.to_vec(),
            }))
        }
        protocol_capnp::engine_to_server::Frame(f) => {
            let f = f?;
            Ok(Some(Message::Frame {
                player: NonZeroU32::new(f.get_player()),
                scene: decode_root::<scene::Owned, _>(f.get_scene()?, |s| {
                    read_scene(s, &test_image)
                })?,
            }))
        }
        protocol_capnp::engine_to_server::Hello(h) => {
            Ok(Some(Message::Hello(engine_to_server::read_hello(h?)?)))
        }
        protocol_capnp::engine_to_server::TickTaken(()) => Ok(Some(Message::TickTaken)),
    }
}

/// One message of the server for a view, one variant per arm of
/// `ServerToView`. A bitmap draws as in [`Message`].
#[derive(Clone, Debug)]
pub(crate) enum ViewMessage {
    Asset { id: u32, blob: Vec<u8> },
    Frame(Scene),
    Forget(u32),
}

/// Decode `payload`, a message of the server for a view. `None` for a
/// message of an arm from a newer schema.
pub(crate) fn decode_view(payload: &[u8]) -> Result<Option<ViewMessage>, Error> {
    decode_root::<protocol_capnp::server_to_view::Owned, _>(payload, |msg| {
        let Ok(which) = msg.which() else {
            return Ok(None);
        };
        Ok(Some(match which {
            protocol_capnp::server_to_view::Asset(a) => {
                let a = a?;
                ViewMessage::Asset {
                    id: a.get_id(),
                    blob: a.get_blob()?.to_vec(),
                }
            }
            protocol_capnp::server_to_view::Frame(scene) => {
                ViewMessage::Frame(decode_root::<scene::Owned, _>(scene?, |s| {
                    read_scene(s, &test_image)
                })?)
            }
            protocol_capnp::server_to_view::Forget(id) => ViewMessage::Forget(id),
        }))
    })
}

/// The image that a bitmap of the tests names by the id `id`.
fn test_image(id: u32) -> Option<Image> {
    Image::new(crate::asset::png_head(id, 1)).ok()
}

/// Encode a scene as a frame for every player, with no envelope.
pub(crate) fn encode_frame(scene: &Scene) -> Vec<u8> {
    encode_frame_to(None, scene)
}

/// Encode a scene as a frame for `player`, with no envelope. The id of an
/// image is its width, as [`decode`] reads it.
pub(crate) fn encode_frame_to(player: Option<NonZeroU32>, scene: &Scene) -> Vec<u8> {
    super::to_bytes(engine_to_server::frame_message(
        player,
        scene,
        &Image::width,
    ))
}

/// Encode the blob of an image as the asset `id`, with no envelope.
pub(crate) fn encode_asset(id: u32, blob: &[u8]) -> Vec<u8> {
    super::to_bytes(engine_to_server::asset_message(id, blob))
}

/// Encode a hello of `min` to `max` players, with no envelope. A test
/// passes a range that [`engine_to_server::write_hello`] cannot.
pub(crate) fn encode_hello(min: u32, max: u32) -> Vec<u8> {
    super::to_bytes(engine_to_server::hello_message(min, max))
}

/// Encode the input `ev` of `player`, with no envelope. A test passes 0,
/// which [`server_to_engine::write_input`] cannot.
pub(crate) fn encode_input(player: u32, ev: &InputEvent) -> Vec<u8> {
    super::to_bytes(server_to_engine::input_message(player, ev))
}

/// Read the messages that `side` wrote until `decode` returns one. `decode`
/// returns `None` for a message of an arm from a newer schema, which
/// `read_next` skips. `None` at the end of the stream. A message that does
/// not decode is an error, and the next read goes on after it.
pub(crate) fn read_next<T>(
    r: &mut impl Read,
    side: Side,
    mut decode: impl FnMut(&[u8]) -> Result<Option<T>, Error>,
) -> Result<Option<T>, Error> {
    loop {
        let Some(words) = read_framed(r, side).expect("the stream of a test is whole") else {
            return Ok(None);
        };
        let payload = Word::words_to_bytes(&words);
        if let Some(message) = decode(payload)? {
            return Ok(Some(message));
        }
    }
}

/// Read one message that `side` wrote, into the words that
/// [`read_message_from_flat_slice`](capnp::serialize::read_message_from_flat_slice)
/// reads in place. Returns `None` when the stream ends before the envelope.
/// The stream ending anywhere else is [`io::ErrorKind::UnexpectedEof`].
/// A magic that is not the one of `side`, and a length that is not a whole
/// number of words or exceeds the cap, are [`io::ErrorKind::InvalidData`].
pub(crate) fn read_framed(r: &mut impl Read, side: Side) -> io::Result<Option<Vec<Word>>> {
    let mut header = [0u8; HEADER_BYTES];
    if !read_start(r, &mut header)? {
        return Ok(None);
    }
    let len = parse_header(header, side)?;
    let mut words = Word::allocate_zeroed_vec(len / size_of::<Word>());
    r.read_exact(Word::words_to_bytes_mut(&mut words))?;
    Ok(Some(words))
}

/// Fill `buf`, or return `false` if the stream ends before its first byte.
fn read_start(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    loop {
        match r.read(buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                let rest = buf
                    .get_mut(n..)
                    .ok_or_else(|| io::Error::other("a read returned more than its buffer"))?;
                r.read_exact(rest)?;
                return Ok(true);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Encode a scene as a message whose root is the `Scene` struct of
/// `schema/scene.capnp`, with no session envelope around it. A bitmap goes
/// out with the id that `ids` gives its image.
pub(crate) fn encode_scene(scene: &Scene, ids: &dyn Fn(&Image) -> u32) -> Vec<u8> {
    super::to_bytes(scene_message(scene, ids))
}

/// Decode a message that [`encode_scene`] produced. A bitmap takes the
/// image that `images` gives its id, and a bitmap of an id with no image
/// is skipped.
pub(crate) fn decode_scene(
    bytes: &[u8],
    images: &dyn Fn(u32) -> Option<Image>,
) -> Result<Scene, Error> {
    let reader = super::limit_traversal(capnp::serialize::read_message(
        io::Cursor::new(bytes),
        capnp::message::ReaderOptions::new(),
    )?);
    read_scene(reader.get_root()?, images)
}

/// [`with_unknown_value`] for the bytes of an `EngineToServer`.
pub(crate) fn with_unknown_engine_value(
    bytes: &[u8],
    find: impl FnOnce(protocol_capnp::engine_to_server::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<protocol_capnp::engine_to_server::Owned>(bytes, find)
}

/// [`with_unknown_value`] for the bytes of a `ServerToEngine`.
pub(crate) fn with_unknown_server_value(
    bytes: &[u8],
    find: impl FnOnce(protocol_capnp::server_to_engine::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<protocol_capnp::server_to_engine::Owned>(bytes, find)
}

/// [`with_unknown_value`] for the bytes of a `ViewToServer`.
pub(crate) fn with_unknown_view_value(
    bytes: &[u8],
    find: impl FnOnce(protocol_capnp::view_to_server::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_value::<protocol_capnp::view_to_server::Owned>(bytes, find)
}

/// Overwrite the two bytes that `find` points at with a value this crate
/// does not know, as a peer with a newer schema writes it. `find` points at
/// the tag of a union, at an enum field, or at the first two verbs of a
/// path.
/// `bytes` holds a message whose root is `T`, and the compiler cannot infer
/// `T` from `find`, so a root either has a wrapper or names `T`.
pub(crate) fn with_unknown_value<T: capnp::traits::Owned>(
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
pub(crate) fn tag_of<'a>(r: impl capnp::traits::IntoInternalStructReader<'a>) -> *const u8 {
    capnp::raw::get_struct_data_section(r).as_ptr()
}

/// A frame for every player that holds `scene`, the bytes of a message
/// whose root is a `Scene`, as a test builds or changes it.
pub(crate) fn frame_holding(scene: &[u8]) -> Vec<u8> {
    let mut builder = capnp::message::Builder::new_default();
    builder
        .init_root::<protocol_capnp::engine_to_server::Builder>()
        .init_frame()
        .set_scene(scene);
    super::to_bytes(builder)
}

/// [`with_unknown_value`] for the scene inside the frame in `bytes`. The
/// frame holds the scene as bytes, so `find` points into them.
pub(crate) fn with_unknown_scene_value(
    bytes: &[u8],
    find: impl FnOnce(scene::Reader<'_>) -> *const u8,
) -> Vec<u8> {
    with_unknown_engine_value(bytes, |m| {
        let Ok(protocol_capnp::engine_to_server::Frame(f)) = m.which() else {
            panic!("not a frame");
        };
        let mut bytes = f.expect("frame").get_scene().expect("scene");
        let msg = capnp::serialize::read_message_from_flat_slice_no_alloc(
            &mut bytes,
            capnp::message::ReaderOptions::new(),
        )
        .expect("parse the scene");
        find(msg.get_root().expect("root"))
    })
}

/// Replace every float `from` in `bytes` with `to`, as a peer that writes a
/// float that is not finite does. A `Scene` never holds one, so a test
/// encodes a marker and swaps it. Cap'n Proto aligns a float to 4 bytes.
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
