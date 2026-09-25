//! The messages from a view to the server, in the `ViewMessage` union.
//!
//! The view sends its input as events, and ends the session with a close.
//! The server passes the input on to the engine with [`super::to_engine`].
//! A view talks to the server over a WebSocket, which frames each message
//! itself, so a message of a view has no envelope.

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::view_message;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::protocol::decode_root;

/// One message of the view, one variant per arm of `ViewMessage`. The arm
/// `event` is `Input` here, so it does not clash with [`crate::event::Event`].
#[derive(Clone, Debug)]
pub enum Message {
    Input(InputEvent),
    Close,
}

/// Encode the input `ev`.
pub fn encode_input(ev: &InputEvent) -> Vec<u8> {
    super::finish(input_message(ev))
}

/// Encode the close of the session.
pub fn encode_close() -> Vec<u8> {
    super::finish(close_message())
}

/// Decode `payload`. `None` for a message or an event of an arm from a
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
