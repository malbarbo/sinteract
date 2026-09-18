//! The messages from the engine to the view, in the `EngineMessage` union.
//!
//! The engine uploads each bitmap once as an asset, before the frames that
//! draw it, and sends a frame per repaint. Either side ends the session
//! with a close.

use std::io::{self, Read, Write};

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator, ReaderOptions};
use capnp::serialize;

use crate::protocol_capnp::engine_message;
use crate::scene::Scene;

use super::framing::{Player, Side, write_framed};
use super::protocol::{ReadError, read_next};
use super::scene::{read_scene, write_scene};
use super::{Error, finish};

/// One message of the engine, one variant per arm of `EngineMessage`.
#[derive(Clone, Debug)]
pub enum Message {
    Asset {
        id: u32,
        blob: Vec<u8>,
        mime: Option<String>,
    },
    Frame(Scene),
    Close,
}

/// Read the next message of the engine, with the player it goes to.
/// Returns `None` at the end of the stream. A message of an arm from a
/// newer schema is skipped, and the next one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<(Player, Message)>, ReadError> {
    read_next(r, Side::Engine, decode)
}

/// Write a scene as a frame for `player`.
pub fn write_frame(w: &mut impl Write, player: Player, scene: &Scene) -> io::Result<()> {
    write_framed(w, Side::Engine, player, &frame_message(scene))
}

/// Write a bitmap upload as an asset for `player`.
pub fn write_asset(
    w: &mut impl Write,
    player: Player,
    id: u32,
    blob: &[u8],
    mime: Option<&str>,
) -> io::Result<()> {
    write_framed(w, Side::Engine, player, &asset_message(id, blob, mime))
}

/// Write the close of the session of `player`.
pub fn write_close(w: &mut impl Write, player: Player) -> io::Result<()> {
    write_framed(w, Side::Engine, player, &close_message())
}

/// Encode a scene as a frame, with no envelope, for a caller that keeps the
/// payload in memory, such as
/// [`Renderer::render_stream`](crate::renderer::Renderer::render_stream).
pub fn encode_frame(scene: &Scene) -> Vec<u8> {
    finish(frame_message(scene))
}

/// Decode the payload in `words` in place. `None` for a message of an arm
/// from a newer schema.
pub(super) fn decode(words: &[Word]) -> Result<Option<Message>, Error> {
    let reader = serialize::read_message_from_flat_slice_no_alloc(
        &mut Word::words_to_bytes(words),
        ReaderOptions::new(),
    )?;
    let msg: engine_message::Reader = reader.get_root()?;
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
        engine_message::Frame(f) => Ok(Some(Message::Frame(read_scene(f?)?))),
        engine_message::Close(_) => Ok(Some(Message::Close)),
    }
}

fn frame_message(scene: &Scene) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    write_scene(
        builder.init_root::<engine_message::Builder>().init_frame(),
        scene,
    );
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

/// Encode a bitmap upload as an asset, with no envelope.
#[cfg(test)]
pub(crate) fn encode_asset(id: u32, blob: &[u8], mime: Option<&str>) -> Vec<u8> {
    finish(asset_message(id, blob, mime))
}

/// Encode the close of the session, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    finish(close_message())
}
