//! The messages of a view, in the `ViewToServer` struct, which go to the
//! server.
//!
//! The view sends its input as events, and the server passes the input on
//! to the engine with [`super::server_to_engine`]. A view talks to the server
//! over a WebSocket, which frames each message itself, so a message of a
//! view has no envelope. The view ends the session with the close of the
//! WebSocket.

use capnp::message::Builder as MessageBuilder;

use crate::event::InputEvent;
use crate::protocol_capnp::view_to_server;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::protocol::decode_root;

/// Encode the input `ev`.
pub fn encode_input(ev: &InputEvent) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    write_input_event(
        builder.init_root::<view_to_server::Builder>().init_event(),
        ev,
    );
    super::to_bytes(builder)
}

/// Decode the input in `payload`. `None` for a message from a newer schema
/// with no event, and for an event of an arm from a newer schema.
pub fn decode(payload: &[u8]) -> Result<Option<InputEvent>, Error> {
    decode_root::<view_to_server::Owned, _>(payload, |msg| {
        if !msg.has_event() {
            return Ok(None);
        }
        read_input_event(msg.get_event()?)
    })
}
