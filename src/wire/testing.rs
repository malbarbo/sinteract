//! What the tests of the wire, the session and the server read and write. A
//! reader of a stream, a decoder of each message of the engine, and the
//! encoders of the messages that a test sends with no envelope or with a
//! value out of range.

use std::io::Read;
use std::num::NonZeroU32;

use capnp::Word;

use crate::event::InputEvent;
use crate::protocol_capnp::engine_message;
use crate::scene::{Image, Scene};

use super::Error;
use super::framing::{Side, read_framed};
use super::protocol::decode_root;
use super::scene::read_scene;
use super::to_engine;
use super::to_view::{self, PlayerRange};

/// One message of the engine, one variant per arm of `EngineMessage`. A
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
    /// The view drops the asset of this id. Only a server sends it.
    Forget(u32),
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
    decode_root::<engine_message::Owned, _>(payload, decode_message)
}

fn decode_message(msg: engine_message::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        engine_message::Asset(a) => {
            let a = a?;
            Ok(Some(Message::Asset {
                id: a.get_id(),
                blob: a.get_blob()?.to_vec(),
            }))
        }
        engine_message::Frame(f) => {
            let f = f?;
            Ok(Some(Message::Frame {
                player: NonZeroU32::new(f.get_player()),
                scene: read_scene(f.get_scene()?, &|id| {
                    Image::new(crate::asset::png_head(id, 1)).ok()
                })?,
            }))
        }
        engine_message::Hello(h) => Ok(Some(Message::Hello(to_view::read_hello(h?)?))),
        engine_message::Forget(id) => Ok(Some(Message::Forget(id))),
        engine_message::TickTaken(()) => Ok(Some(Message::TickTaken)),
    }
}

/// Encode a scene as a frame for every player, with no envelope.
pub(crate) fn encode_frame(scene: &Scene) -> Vec<u8> {
    encode_frame_to(None, scene)
}

/// Encode a scene as a frame for `player`, with no envelope. The id of an
/// image is its width, as [`decode`] reads it.
pub(crate) fn encode_frame_to(player: Option<NonZeroU32>, scene: &Scene) -> Vec<u8> {
    super::finish(to_view::frame_message(player, scene, &Image::width))
}

/// Encode the file of an image as the asset `id`, with no envelope.
pub(crate) fn encode_asset(id: u32, blob: &[u8]) -> Vec<u8> {
    super::finish(to_view::asset_message(id, blob))
}

/// Encode a hello of `min` to `max` players, with no envelope. A test
/// passes a range that [`to_view::write_hello`] cannot.
pub(crate) fn encode_hello(min: u32, max: u32) -> Vec<u8> {
    super::finish(to_view::hello_message(min, max))
}

/// Encode the input `ev` of `player`, with no envelope. A test passes 0,
/// which [`to_engine::write_input`] cannot.
pub(crate) fn encode_input(player: u32, ev: &InputEvent) -> Vec<u8> {
    super::finish(to_engine::input_message(player, ev))
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
