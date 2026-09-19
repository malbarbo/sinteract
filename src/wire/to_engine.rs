//! The messages from the view to the engine, in the `ViewMessage` union.
//!
//! The view sends its input as events, and ends the session with a close,
//! which [`InputEvent::Close`] stands for on both sides.

use std::io::{self, Read, Write};

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::view_message;

use super::Error;
use super::event::{read_input_event, write_key_event, write_mouse_event, write_resize_event};
use super::framing::{Player, Side, write_framed};
use super::protocol::{ReadError, decode_root, read_next};

/// Read the next message of the view, with the player it comes from.
/// Returns `None` at the end of the stream, and [`InputEvent::Close`] for
/// its close. A message or an event of an arm from a newer schema is
/// skipped, and the next one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<(Player, InputEvent)>, ReadError> {
    read_next(r, Side::View, decode)
}

/// Write `ev` of `player`, where [`InputEvent::Close`] is the close of the
/// session.
pub fn write(w: &mut impl Write, player: Player, ev: &InputEvent) -> io::Result<()> {
    write_framed(w, Side::View, player, &message(ev))
}

/// Decode the payload in `words` in place. `None` for a message or an event
/// of an arm from a newer schema.
pub(super) fn decode(words: &[Word]) -> Result<Option<InputEvent>, Error> {
    decode_root::<view_message::Owned, _>(words, decode_message)
}

fn decode_message(msg: view_message::Reader<'_>) -> Result<Option<InputEvent>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        view_message::Event(e) => read_input_event(e?),
        view_message::Close(_) => Ok(Some(InputEvent::Close)),
    }
}

fn message(ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
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
        InputEvent::Close => {
            msg.init_close();
        }
    }
    builder
}

/// Encode `ev`, with no envelope.
#[cfg(test)]
pub(crate) fn encode(ev: &InputEvent) -> Vec<u8> {
    super::finish(message(ev))
}
