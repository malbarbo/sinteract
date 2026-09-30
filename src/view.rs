//! The view side of a session. A view reads the messages that the server
//! sends it with a [`FrameReader`], which gives the scene of each frame,
//! and sends each input of the user with [`encode_input`]. The server and
//! the view pass the messages over a WebSocket, which frames each message
//! itself, so a message of a view has no envelope. The view asks for the
//! subprotocol [`SUBPROTOCOL`].

pub use crate::wire::framing::SUBPROTOCOL;
pub use crate::wire::server_to_view::FrameReader;
pub use crate::wire::view_to_server::encode_input;
