//! The messages from the view to the engine, in the `ViewMessage` union.
//!
//! The view sends its input as events, and ends the session with a close.

use std::io::{self, Read, Write};

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::view_message;

use super::Error;
use super::event::{read_input_event, write_key_event, write_mouse_event, write_resize_event};
use super::framing::{Player, Side, write_framed};
use super::protocol::{ReadError, decode_root, read_next};

/// One message of the view, one variant per arm of `ViewMessage`. The arm
/// `event` is `Input` here, so it does not clash with [`crate::event::Event`].
#[derive(Clone, Debug)]
pub enum Message {
    Input(InputEvent),
    Close,
}

/// Read the next message of the view, with the player it comes from.
/// Returns `None` at the end of the stream. A message or an event of an arm
/// from a newer schema is skipped, and the next one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<(Player, Message)>, ReadError> {
    read_next(r, Side::View, decode)
}

/// Write the input `ev` of `player`.
pub fn write_input(w: &mut impl Write, player: Player, ev: &InputEvent) -> io::Result<()> {
    write_framed(w, Side::View, player, &input_message(ev))
}

/// Write the close of the session of `player`.
pub fn write_close(w: &mut impl Write, player: Player) -> io::Result<()> {
    write_framed(w, Side::View, player, &close_message())
}

/// Decode the payload in `words` in place. `None` for a message or an event
/// of an arm from a newer schema.
pub(super) fn decode(words: &[Word]) -> Result<Option<Message>, Error> {
    decode_root::<view_message::Owned, _>(words, decode_message)
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
    let msg = builder.init_root::<view_message::Builder>();
    match ev {
        InputEvent::Key(k) => write_key_event(msg.init_event().init_key(), k),
        InputEvent::Mouse(m) => write_mouse_event(msg.init_event().init_mouse(), m),
        InputEvent::Resize { width, height } => {
            write_resize_event(msg.init_event().init_resize(), *width, *height)
        }
        InputEvent::Vsync => {
            msg.init_event().init_tick();
        }
    }
    builder
}

fn close_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<view_message::Builder>().init_close();
    builder
}

/// Encode the input `ev`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_input(ev: &InputEvent) -> Vec<u8> {
    super::finish(input_message(ev))
}

/// Encode a close, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    super::finish(close_message())
}
