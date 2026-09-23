//! The messages from a view to the server, in the `ViewMessage` union.
//!
//! The view sends its input as events, and ends the session with a close.
//! The server passes the input on to the engine with [`super::to_engine`],
//! and a view that talks to the engine with no server between them writes
//! with [`super::to_engine`] too.

use std::io::{self, Read, Write};

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::view_message;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::framing::{Side, UNROUTED, write_framed};
use super::protocol::{ReadError, decode_root, read_next};

/// One message of the view, one variant per arm of `ViewMessage`. The arm
/// `event` is `Input` here, so it does not clash with [`crate::event::Event`].
#[derive(Clone, Debug)]
pub enum Message {
    Input(InputEvent),
    Close,
}

/// Read the next message of a view. Returns `None` at the end of the
/// stream. A message or an event of an arm from a newer schema is skipped,
/// and the next one comes out. The server knows the player of a view from
/// its connection, so the player in the header does not count.
pub fn read(r: &mut impl Read) -> Result<Option<Message>, ReadError> {
    read_next(r, Side::View, |_, payload| decode(payload))
}

/// Write the input `ev`.
pub fn write_input(w: &mut impl Write, ev: &InputEvent) -> io::Result<()> {
    write_framed(w, Side::View, UNROUTED, &input_message(ev))
}

/// Write the close of the session.
pub fn write_close(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::View, UNROUTED, &close_message())
}

/// Decode `payload`, a message with no envelope, such as one that came
/// over a WebSocket. `None` for a message or an event of an arm from a
/// newer schema.
pub fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<view_message::Owned, _>(payload, decode_message)
}

fn decode_message(msg: view_message::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        view_message::Event(e) => Ok(read_input_event(e?)?.map(Message::Input)),
        view_message::Close(_) => Ok(Some(Message::Close)),
    }
}

fn input_message(ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    write_input_event(
        builder.init_root::<view_message::Builder>().init_event(),
        ev,
    );
    builder
}

fn close_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<view_message::Builder>().init_close();
    builder
}

/// Encode a close, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    super::finish(close_message())
}
