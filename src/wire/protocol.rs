//! What the messages of the session share: the opening of a payload.
//!
//! The engine sends an `EngineToServer`, a view sends a `ViewToServer` and
//! the server sends a `ServerToEngine`. [`super::engine_to_server`],
//! [`super::view_to_server`] and [`super::server_to_engine`] wrap the
//! payloads of [`super::scene`] and [`super::event`] in them and unwrap
//! them again. [`super::engine_to_server`] and [`super::server_to_engine`]
//! write them with the envelope of [`super::framing`].

use capnp::Word;
use capnp::message::ReaderOptions;
use capnp::serialize;
use capnp::traits::Owned;

use super::Error;

/// Open `payload` as a message whose root is `T`, and hand the root to
/// `decode`. Cap'n Proto reads a payload in place when it starts on a word,
/// and a payload that does not goes into a copy first. A payload that is
/// not a whole number of words, or that holds bytes after the message, is
/// an error on both paths.
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
    let reader = super::limit_traversal(serialize::read_message_from_flat_slice(
        &mut bytes,
        ReaderOptions::new(),
    )?);
    if !bytes.is_empty() {
        return Err(capnp::Error::failed(format!(
            "{} bytes follow the message in its payload",
            bytes.len()
        ))
        .into());
    }
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
    use super::super::testing::{self, Message, encode_asset};

    #[test]
    fn a_payload_that_does_not_start_on_a_word_decodes() {
        let asset = encode_asset(7, &[1, 2, 3]);
        let (buffer, start) = unaligned(&asset);
        let payload = &buffer[start..start + asset.len()];
        assert!(matches!(
            testing::decode(payload),
            Ok(Some(Message::Asset { id: 7, .. }))
        ));
    }

    #[test]
    fn a_payload_that_is_not_whole_words_is_an_error_on_both_paths() {
        let asset = encode_asset(7, &[1, 2, 3]);
        let cut = &asset[..asset.len() - 3];
        assert!(testing::decode(cut).is_err());
        let (buffer, start) = unaligned(cut);
        assert!(testing::decode(&buffer[start..start + cut.len()]).is_err());
    }

    #[test]
    fn a_payload_with_bytes_after_the_message_is_an_error_on_both_paths() {
        let mut long = encode_asset(7, &[1, 2, 3]);
        long.extend_from_slice(&[0; 8]);
        assert!(testing::decode(&long).is_err());
        let (buffer, start) = unaligned(&long);
        assert!(testing::decode(&buffer[start..start + long.len()]).is_err());
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
