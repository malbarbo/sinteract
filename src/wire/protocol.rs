//! What the two directions of the session share: the error of a read and
//! the loop that reads the next message.
//!
//! The engine sends an `EngineMessage` and the view sends a `ViewMessage`.
//! [`super::to_view`] and [`super::to_engine`] wrap the payloads of
//! [`super::scene`] and [`super::event`] in them and unwrap them again, and
//! read and write them with the envelope of [`super::framing`].

use std::io::{self, Read};

use capnp::Word;

use super::Error;
use super::framing::read_framed;

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

/// Read messages until `decode` returns one. `decode` returns `None` for a
/// message of an arm from a newer schema, which is skipped. `None` at the
/// end of the stream.
pub(super) fn read_next<T>(
    r: &mut impl Read,
    decode: impl Fn(&[Word]) -> Result<Option<T>, Error>,
) -> Result<Option<T>, ReadError> {
    loop {
        let Some(words) = read_framed(r).map_err(ReadError::Broken)? else {
            return Ok(None);
        };
        if let Some(message) = decode(&words).map_err(ReadError::Payload)? {
            return Ok(Some(message));
        }
    }
}
