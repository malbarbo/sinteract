//! What the messages of the session share: the error of a read and the loop
//! that reads the next message.
//!
//! The engine sends an `EngineMessage`, a view sends a `ViewMessage` and
//! the server sends a `ServerMessage`. [`super::to_view`],
//! [`super::to_server`] and [`super::to_engine`] wrap the payloads of
//! [`super::scene`] and [`super::event`] in them and unwrap them again, and
//! read and write them with the envelope of [`super::framing`].

use std::io::{self, Read};

use capnp::Word;
use capnp::message::ReaderOptions;
use capnp::serialize;
use capnp::traits::Owned;

use super::Error;
use super::framing::{Player, Side, read_framed};

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

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::Broken(e) => Some(e),
            ReadError::Payload(e) => Some(e),
        }
    }
}

/// Read the messages that `side` wrote until `decode` returns one. `decode`
/// gets the player of the header with the payload, and returns `None` for
/// a message of an arm from a newer schema, which `read_next` skips. `None`
/// at the end of the stream.
pub(super) fn read_next<T>(
    r: &mut impl Read,
    side: Side,
    decode: impl Fn(Player, &[Word]) -> Result<Option<T>, Error>,
) -> Result<Option<T>, ReadError> {
    loop {
        let Some((player, words)) = read_framed(r, side).map_err(ReadError::Broken)? else {
            return Ok(None);
        };
        if let Some(message) = decode(player, &words).map_err(ReadError::Payload)? {
            return Ok(Some(message));
        }
    }
}

/// Open the payload in `words` in place as a message whose root is `T`, and
/// hand the root to `decode`.
pub(super) fn decode_root<T: Owned, M>(
    words: &[Word],
    decode: impl FnOnce(T::Reader<'_>) -> Result<Option<M>, Error>,
) -> Result<Option<M>, Error> {
    let reader = serialize::read_message_from_flat_slice_no_alloc(
        &mut Word::words_to_bytes(words),
        ReaderOptions::new(),
    )?;
    decode(reader.get_root()?)
}
