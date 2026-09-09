//! The `Message` envelope and the session it describes.
//!
//! A session carries three kinds of message. Asset uploads a bitmap once,
//! before the frames. Frame is a scene to paint. Event is the input of the
//! client. Either side ends the session with a close.
//!
//! The codecs of the payloads live in [`super::scene`] and [`super::event`].
//! This module only wraps them in the union and unwraps them again.

use std::io::Cursor;

use capnp::message::{Builder as MessageBuilder, ReaderOptions};
use capnp::serialize;

use crate::event::InputEvent;
use crate::protocol_capnp::message;
use crate::scene::Scene;

use super::event::{read_input_event, write_input_event};
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

// ---------------------------------------------------------------------------
// Encode side
// ---------------------------------------------------------------------------

/// Encode a scene as `Message::Frame`.
pub fn encode_frame(scene: &Scene) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let frame = msg.init_frame();
        write_scene(frame, scene);
    }
    finish(builder)
}

/// Encode an input event as `Message::Event`.
pub fn encode_event(ev: &InputEvent) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let ev_b = msg.init_event();
        write_input_event(ev_b, ev);
    }
    finish(builder)
}

/// Encode a bitmap upload as `Message::Asset`.
pub fn encode_asset(id: u32, blob: &[u8], mime: Option<&str>) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let msg = builder.init_root::<message::Builder>();
        let mut asset = msg.init_asset();
        asset.set_id(id);
        asset.set_blob(blob);
        if let Some(m) = mime {
            asset.set_mime(m);
        }
    }
    finish(builder)
}

/// Encode a session close.
pub fn encode_close() -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    {
        let mut msg = builder.init_root::<message::Builder>();
        msg.set_session_close(());
    }
    finish(builder)
}

// ---------------------------------------------------------------------------
// Decode side
// ---------------------------------------------------------------------------

/// Decode a buffer produced by one of the `encode_*` functions.
pub fn decode(bytes: &[u8]) -> Result<Decoded, Error> {
    let reader = serialize::read_message(Cursor::new(bytes), ReaderOptions::new())?;
    let msg: message::Reader = reader.get_root()?;
    match msg.which()? {
        message::Asset(a) => {
            let a = a?;
            let blob = a.get_blob()?.to_vec();
            let mime = a.get_mime();
            let mime = match mime {
                Ok(t) => {
                    let s = t.to_str()?.to_owned();
                    if s.is_empty() { None } else { Some(s) }
                }
                Err(_) => None,
            };
            Ok(Decoded::Asset {
                id: a.get_id(),
                blob,
                mime,
            })
        }
        message::Frame(f) => Ok(Decoded::Frame(read_scene(f?)?)),
        message::Event(e) => Ok(Decoded::Event(read_input_event(e?)?)),
        message::SessionClose(()) => Ok(Decoded::Close),
    }
}
