//! The messages from the engine to the view, in the `EngineMessage` union.
//!
//! The engine uploads each bitmap once as an asset, before the frames that
//! draw it, and sends a frame per repaint, for one player or for all of
//! them. Either side ends the session with a close.

use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::protocol_capnp::engine_message;
use crate::scene::Scene;

use super::Error;
use super::framing::{Side, write_framed};
use super::protocol::{ReadError, decode_root, read_next};
use super::scene::{read_scene, write_scene};

/// One message of the engine, one variant per arm of `EngineMessage`. An
/// asset and a close go to every player.
#[derive(Clone, Debug)]
pub enum Message {
    Asset {
        id: u32,
        blob: Vec<u8>,
        mime: Option<String>,
    },
    /// A frame for `player`, or for every player when `player` is `None`.
    Frame {
        player: Option<NonZeroU32>,
        scene: Scene,
    },
    Close,
}

/// The arm of a message of the engine, with the player of a frame. A
/// server routes a message by its arm and passes the payload on as it
/// came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    Asset,
    /// A frame for `player`, or for every player when `player` is `None`.
    Frame {
        player: Option<NonZeroU32>,
    },
    Close,
}

/// Read the next message of the engine. Returns `None` at the end of the
/// stream. A message of an arm from a newer schema is skipped, and the next
/// one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<Message>, ReadError> {
    read_next(r, Side::Engine, decode)
}

/// Write a scene as a frame for `player`, or for every player when
/// `player` is `None`.
pub fn write_frame(
    w: &mut impl Write,
    player: Option<NonZeroU32>,
    scene: &Scene,
) -> io::Result<()> {
    write_framed(w, Side::Engine, &frame_message(player, scene))
}

/// Write a bitmap upload as an asset.
pub fn write_asset(w: &mut impl Write, id: u32, blob: &[u8], mime: Option<&str>) -> io::Result<()> {
    write_framed(w, Side::Engine, &asset_message(id, blob, mime))
}

/// Write the close of the session.
pub fn write_close(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::Engine, &close_message())
}

/// The arm of `payload`, a message with no envelope, without a decode of
/// the scene of a frame. `None` for an arm from a newer schema.
pub fn arm(payload: &[u8]) -> Result<Option<Arm>, Error> {
    decode_root::<engine_message::Owned, _>(payload, |msg| {
        let Ok(which) = msg.which() else {
            return Ok(None);
        };
        Ok(Some(match which {
            engine_message::Asset(_) => Arm::Asset,
            engine_message::Frame(f) => Arm::Frame {
                player: NonZeroU32::new(f?.get_player()),
            },
            engine_message::Close(_) => Arm::Close,
        }))
    })
}

/// Decode `payload`. `None` for a message of an arm from a newer schema.
pub(super) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<engine_message::Owned, _>(payload, decode_message)
}

fn decode_message(msg: engine_message::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        engine_message::Asset(a) => {
            let a = a?;
            let blob = a.get_blob()?.to_vec();
            let mime = match a.get_mime() {
                Ok(t) => {
                    let s = t.to_str()?.to_owned();
                    if s.is_empty() { None } else { Some(s) }
                }
                Err(_) => None,
            };
            Ok(Some(Message::Asset {
                id: a.get_id(),
                blob,
                mime,
            }))
        }
        engine_message::Frame(f) => {
            let f = f?;
            Ok(Some(Message::Frame {
                player: NonZeroU32::new(f.get_player()),
                scene: read_scene(f.get_scene()?)?,
            }))
        }
        engine_message::Close(_) => Ok(Some(Message::Close)),
    }
}

fn frame_message(player: Option<NonZeroU32>, scene: &Scene) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut frame = builder.init_root::<engine_message::Builder>().init_frame();
    frame.set_player(player.map_or(0, NonZeroU32::get));
    write_scene(frame.init_scene(), scene);
    builder
}

fn asset_message(id: u32, blob: &[u8], mime: Option<&str>) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut asset = builder.init_root::<engine_message::Builder>().init_asset();
    asset.set_id(id);
    asset.set_blob(blob);
    if let Some(m) = mime {
        asset.set_mime(m);
    }
    builder
}

fn close_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<engine_message::Builder>().init_close();
    builder
}

/// Encode a scene as a frame for every player, with no envelope.
#[cfg(test)]
pub(crate) fn encode_frame(scene: &Scene) -> Vec<u8> {
    encode_frame_to(None, scene)
}

/// Encode a scene as a frame for `player`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_frame_to(player: Option<NonZeroU32>, scene: &Scene) -> Vec<u8> {
    super::finish(frame_message(player, scene))
}

/// Encode a bitmap upload as an asset, with no envelope.
#[cfg(test)]
pub(crate) fn encode_asset(id: u32, blob: &[u8], mime: Option<&str>) -> Vec<u8> {
    super::finish(asset_message(id, blob, mime))
}

/// Encode the close of the session, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    super::finish(close_message())
}
