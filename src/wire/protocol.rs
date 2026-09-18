//! The `Message` envelope and the session it describes.
//!
//! A session carries three kinds of message. Asset uploads a bitmap once,
//! before the frames. Frame is a scene to paint. Event is the input of the
//! client. Either side ends the session with a close.
//!
//! The codecs of the payloads live in [`super::scene`] and [`super::event`].
//! This module only wraps them in the union and unwraps them again, and
//! reads and writes them with the envelope of [`super::framing`].

use std::io::{self, Read, Write};

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator, ReaderOptions};
use capnp::serialize;

use crate::event::InputEvent;
use crate::protocol_capnp::message;
use crate::scene::Scene;

use super::event::{read_input_event, write_input_event};
use super::framing::{read_framed, write_framed};
use super::scene::{read_scene, write_scene};
use super::{Error, finish};

/// One decoded message, one variant per arm of the `Message` union.
#[derive(Clone, Debug)]
pub enum Decoded {
    Asset {
        id: u32,
        blob: Vec<u8>,
        mime: Option<String>,
    },
    Frame(Scene),
    Event(InputEvent),
    Close,
}

/// Reading a message fails in two ways, and only the second leaves the
/// session usable.
#[derive(Debug)]
pub enum ReadError {
    /// The stream failed, ended inside a message, or does not carry the
    /// envelope. The session cannot go on.
    Broken(io::Error),
    /// The message does not decode. The envelope already found where the
    /// next one starts, so the reader can go on.
    Payload(Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Broken(e) => write!(f, "broken stream: {e}"),
            ReadError::Payload(e) => write!(f, "message does not decode: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

// ---------------------------------------------------------------------------
// Write side
// ---------------------------------------------------------------------------

/// Write a scene as `Message::Frame`.
pub fn write_frame(w: &mut impl Write, scene: &Scene) -> io::Result<()> {
    write_framed(w, &frame_message(scene))
}

/// Write an input event as `Message::Event`.
pub fn write_event(w: &mut impl Write, ev: &InputEvent) -> io::Result<()> {
    write_framed(w, &event_message(ev))
}

/// Write a bitmap upload as `Message::Asset`.
pub fn write_asset(w: &mut impl Write, id: u32, blob: &[u8], mime: Option<&str>) -> io::Result<()> {
    write_framed(w, &asset_message(id, blob, mime))
}

/// Write a session close.
pub fn write_close(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, &close_message())
}

/// Encode a scene as `Message::Frame`, with no envelope, for a caller that
/// keeps the payload in memory, such as
/// [`Renderer::render_stream`](crate::renderer::Renderer::render_stream).
pub fn encode_frame(scene: &Scene) -> Vec<u8> {
    finish(frame_message(scene))
}

fn frame_message(scene: &Scene) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    write_scene(builder.init_root::<message::Builder>().init_frame(), scene);
    builder
}

fn event_message(ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    write_input_event(builder.init_root::<message::Builder>().init_event(), ev);
    builder
}

fn asset_message(id: u32, blob: &[u8], mime: Option<&str>) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut asset = builder.init_root::<message::Builder>().init_asset();
    asset.set_id(id);
    asset.set_blob(blob);
    if let Some(m) = mime {
        asset.set_mime(m);
    }
    builder
}

fn close_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<message::Builder>()
        .set_session_close(());
    builder
}

// ---------------------------------------------------------------------------
// Read side
// ---------------------------------------------------------------------------

/// Read the next message. Returns `None` at the end of the stream. A
/// message or an event of an arm from a newer schema is skipped, and the
/// next one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<Decoded>, ReadError> {
    loop {
        let Some(words) = read_framed(r).map_err(ReadError::Broken)? else {
            return Ok(None);
        };
        if let Some(decoded) = decode(&words).map_err(ReadError::Payload)? {
            return Ok(Some(decoded));
        }
    }
}

/// Decode the payload in `words` in place. `None` for a message or an
/// event of an arm from a newer schema.
pub(super) fn decode(words: &[Word]) -> Result<Option<Decoded>, Error> {
    let reader = serialize::read_message_from_flat_slice_no_alloc(
        &mut Word::words_to_bytes(words),
        ReaderOptions::new(),
    )?;
    let msg: message::Reader = reader.get_root()?;
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        message::Asset(a) => {
            let a = a?;
            let blob = a.get_blob()?.to_vec();
            let mime = match a.get_mime() {
                Ok(t) => {
                    let s = t.to_str()?.to_owned();
                    if s.is_empty() { None } else { Some(s) }
                }
                Err(_) => None,
            };
            Ok(Some(Decoded::Asset {
                id: a.get_id(),
                blob,
                mime,
            }))
        }
        message::Frame(f) => Ok(Some(Decoded::Frame(read_scene(f?)?))),
        message::Event(e) => Ok(read_input_event(e?)?.map(Decoded::Event)),
        message::SessionClose(()) => Ok(Some(Decoded::Close)),
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Encode an input event as `Message::Event`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_event(ev: &InputEvent) -> Vec<u8> {
    finish(event_message(ev))
}

/// Encode a bitmap upload as `Message::Asset`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_asset(id: u32, blob: &[u8], mime: Option<&str>) -> Vec<u8> {
    finish(asset_message(id, blob, mime))
}

/// Encode a session close, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    finish(close_message())
}
