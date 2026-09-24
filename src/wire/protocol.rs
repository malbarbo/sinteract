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
use super::framing::{Side, read_framed};

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
/// returns `None` for a message of an arm from a newer schema, which
/// `read_next` skips. `None` at the end of the stream.
pub(super) fn read_next<T>(
    r: &mut impl Read,
    side: Side,
    decode: impl Fn(&[u8]) -> Result<Option<T>, Error>,
) -> Result<Option<T>, ReadError> {
    loop {
        let Some(words) = read_framed(r, side).map_err(ReadError::Broken)? else {
            return Ok(None);
        };
        let payload = Word::words_to_bytes(&words);
        if let Some(message) = decode(payload).map_err(ReadError::Payload)? {
            return Ok(Some(message));
        }
    }
}

/// Open `payload` as a message whose root is `T`, and hand the root to
/// `decode`. Cap'n Proto reads a payload in place when it starts on a word,
/// and a payload that does not goes into a copy first. A payload that is
/// not a whole number of words is an error on both paths.
pub(super) fn decode_root<T: Owned, M>(
    payload: &[u8],
    decode: impl FnOnce(T::Reader<'_>) -> Result<Option<M>, Error>,
) -> Result<Option<M>, Error> {
    if !payload.len().is_multiple_of(size_of::<Word>()) {
        return Err(capnp::Error::failed(format!(
            "payload of {} bytes is not a whole number of words",
            payload.len()
        ))
        .into());
    }
    let copy;
    let mut bytes = if payload.as_ptr().addr().is_multiple_of(align_of::<Word>()) {
        payload
    } else {
        copy = aligned_copy(payload);
        Word::words_to_bytes(&copy)
    };
    let reader =
        serialize::read_message_from_flat_slice_no_alloc(&mut bytes, ReaderOptions::new())?;
    decode(reader.get_root()?)
}

/// `payload`, a whole number of words, copied into words.
fn aligned_copy(payload: &[u8]) -> Vec<Word> {
    let mut words = Word::allocate_zeroed_vec(payload.len() / size_of::<Word>());
    Word::words_to_bytes_mut(&mut words).copy_from_slice(payload);
    words
}

#[cfg(test)]
mod tests {
    use super::super::to_view::{self, Message, encode_close};

    #[test]
    fn a_payload_that_does_not_start_on_a_word_decodes() {
        let close = encode_close();
        let (buffer, start) = unaligned(&close);
        let payload = &buffer[start..start + close.len()];
        assert!(matches!(to_view::decode(payload), Ok(Some(Message::Close))));
    }

    #[test]
    fn a_payload_that_is_not_whole_words_is_an_error_on_both_paths() {
        let close = encode_close();
        let cut = &close[..close.len() - 3];
        assert!(to_view::decode(cut).is_err());
        let (buffer, start) = unaligned(cut);
        assert!(to_view::decode(&buffer[start..start + cut.len()]).is_err());
    }

    /// A buffer that holds `bytes` from `start`, one byte past a word.
    fn unaligned(bytes: &[u8]) -> (Vec<u8>, usize) {
        let mut buffer = vec![0u8; bytes.len() + 9];
        let start = (1..=8)
            .find(|&i| buffer[i..].as_ptr().addr() % 8 == 1)
            .unwrap();
        buffer[start..start + bytes.len()].copy_from_slice(bytes);
        (buffer, start)
    }
}
