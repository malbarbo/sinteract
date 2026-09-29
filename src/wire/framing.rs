//! The envelope that carries one encoded message over a byte stream.
//!
//! Cap'n Proto frames its own payload, and this envelope puts a header of
//! 8 bytes in front of it. The header holds a magic of four bytes and the
//! length of the payload as a little-endian `u32`. The magic is `SI`, then
//! `E` from the engine or `S` from the server, then the version of the
//! payload, `1`.
//!
//! The magic rejects text from another writer on the same pipe, such as a
//! stray `print` from the program of a student, before the bytes reach the
//! Cap'n Proto reader. It also says who wrote the message, since a Cap'n
//! Proto message does not name its root, and a reader that gets the magic
//! of another side knows that the two ends are not wired as it expects. A
//! reader that does not know the version stops instead of reading what it
//! cannot.
//!
//! A WebSocket, which frames its own messages, carries the payload alone
//! and the version in its subprotocol, `sinteract.v1`. A view only talks
//! over a WebSocket, so it has no magic.
//!
//! A message goes from the builder to the writer, and from the reader into
//! the words that the decoder reads in place, with no copy in between.

use std::io::{self, Write};

use capnp::Word;
use capnp::message::{Allocator, Builder};
use capnp::serialize;

/// The side of the session that writes a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Runs the program and sends the frames.
    Engine,
    /// Owns the session. It passes the input of the views to the engine and
    /// tells the engine who plays.
    Server,
}

impl Side {
    /// The magic at the start of every message that this side writes.
    pub const fn magic(self) -> [u8; 4] {
        match self {
            Side::Engine => *b"SIE1",
            Side::Server => *b"SIS1",
        }
    }

    /// The side whose magic is `magic`, if any.
    fn from_magic(magic: [u8; 4]) -> Option<Side> {
        [Side::Engine, Side::Server]
            .into_iter()
            .find(|side| side.magic() == magic)
    }
}

/// The length of the header in front of each payload.
pub const HEADER_BYTES: usize = 8;

/// Cap on both sides, so a corrupted length cannot make the reader allocate
/// gigabytes. A 1080p RGBA pixmap is about 8 MiB.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Write `message` from `side`, with its envelope, and flush, so the peer
/// sees it at once. The message goes straight to `w`, which should be
/// buffered, since Cap'n Proto writes the segment table and each segment
/// apart. A message above the cap is not written, and the error is
/// [`io::ErrorKind::InvalidInput`].
pub fn write_framed<A: Allocator>(
    w: &mut impl Write,
    side: Side,
    message: &Builder<A>,
) -> io::Result<()> {
    let len = serialize::compute_serialized_size_in_words(message) * size_of::<Word>();
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("message of {len} bytes exceeds cap {MAX_MESSAGE_BYTES}"),
        ));
    }
    w.write_all(&header(side, len as u32))?;
    // The only errors of `write_message` come from `w`.
    serialize::write_message(&mut *w, message).map_err(io::Error::other)?;
    w.flush()
}

/// The length of the payload in `header`. A magic that is not the one of
/// `side`, and a length that is not a whole number of words or exceeds the
/// cap, are [`io::ErrorKind::InvalidData`].
pub(super) fn parse_header(header: [u8; HEADER_BYTES], side: Side) -> io::Result<usize> {
    let [m0, m1, m2, m3, l0, l1, l2, l3] = header;
    check_magic([m0, m1, m2, m3], side)?;
    let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_MESSAGE_BYTES || !len.is_multiple_of(size_of::<Word>()) {
        return Err(invalid(format!(
            "message length {len} is not a whole number of words up to {MAX_MESSAGE_BYTES}"
        )));
    }
    Ok(len)
}

/// Split the first message that `side` wrote off the front of `bytes`, for
/// a reader that keeps the stream in a buffer. Returns the payload and the
/// bytes after it, or `None` while `bytes` holds no whole message. The
/// checks on the header are the ones of [`parse_header`], and a header
/// that fails them is an error before its payload arrives.
pub fn split_message(bytes: &[u8], side: Side) -> io::Result<Option<(&[u8], &[u8])>> {
    let Some((header, rest)) = bytes.split_first_chunk::<HEADER_BYTES>() else {
        return Ok(None);
    };
    let len = parse_header(*header, side)?;
    Ok(rest.split_at_checked(len))
}

/// The header in front of a payload of `len` bytes.
pub(crate) fn header(side: Side, len: u32) -> [u8; HEADER_BYTES] {
    let mut header = [0u8; HEADER_BYTES];
    header[..4].copy_from_slice(&side.magic());
    header[4..].copy_from_slice(&len.to_le_bytes());
    header
}

/// Say what is wrong with a magic that is not the one of `side`.
fn check_magic(magic: [u8; 4], side: Side) -> io::Result<()> {
    let expected = side.magic();
    if magic == expected {
        return Ok(());
    }
    Err(invalid(if let Some(other) = Side::from_magic(magic) {
        format!("got a message of the {other:?} side, not of the {side:?} side")
    } else if magic[..3] == expected[..3] {
        format!(
            "version {:?} of the protocol is not supported",
            magic[3] as char
        )
    } else {
        format!("not a sinteract stream: got {magic:?}")
    }))
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::testing::read_framed;

    fn header_with(magic: [u8; 4], len: u32) -> Vec<u8> {
        let mut out = header(Side::Server, len);
        out[..4].copy_from_slice(&magic);
        out.to_vec()
    }

    fn read(bytes: &[u8]) -> io::Result<Option<Vec<Word>>> {
        read_framed(&mut &bytes[..], Side::Server)
    }

    fn read_error(bytes: &[u8]) -> io::Error {
        read(bytes).expect_err("an error")
    }

    #[test]
    fn a_message_round_trips() {
        let mut message = capnp::message::Builder::new_default();
        message.set_root("hi").unwrap();
        let mut bytes = Vec::new();
        write_framed(&mut bytes, Side::Server, &message).unwrap();
        assert_eq!(&bytes[..4], b"SIS1");
        let words = read(&bytes).unwrap().expect("a message");
        assert_eq!(
            Word::words_to_bytes(&words),
            &serialize::write_message_to_words(&message)[..]
        );
    }

    #[test]
    fn an_empty_stream_is_the_end() {
        assert!(read(&[]).unwrap().is_none());
    }

    #[test]
    fn a_stream_that_ends_inside_the_header_is_an_error() {
        let err = read_error(&Side::Server.magic());
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_stream_that_ends_inside_the_payload_is_an_error() {
        let mut bytes = header_with(Side::Server.magic(), 16);
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(read_error(&bytes).kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_message_of_another_side_is_an_error() {
        let err = read_framed(&mut &header_with(Side::Engine.magic(), 0)[..], Side::Server)
            .expect_err("an error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("Engine side"), "{err}");
    }

    #[test]
    fn each_side_has_its_own_magic() {
        for side in [Side::Engine, Side::Server] {
            assert_eq!(Side::from_magic(side.magic()), Some(side));
        }
    }

    #[test]
    fn an_unknown_version_is_an_error() {
        let err = read_error(&header_with(*b"SIS2", 0));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("version"), "{err}");
    }

    #[test]
    fn a_wrong_magic_is_an_error() {
        let err = read_error(&header_with(*b"junk", 0));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("not a sinteract stream"), "{err}");
    }

    #[test]
    fn a_length_that_is_not_whole_words_is_an_error() {
        let mut bytes = header_with(Side::Server.magic(), 4);
        bytes.extend_from_slice(&[0; 4]);
        assert_eq!(read_error(&bytes).kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_header_parses_to_its_length() {
        let header = header(Side::Engine, 16);
        assert_eq!(parse_header(header, Side::Engine).unwrap(), 16);
        let err = parse_header(header, Side::Server).expect_err("an error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn split_message_waits_for_a_whole_message() {
        let mut bytes = header(Side::Engine, 8).to_vec();
        bytes.extend_from_slice(&[1; 8]);
        bytes.extend_from_slice(&[2; 3]);
        for end in 0..HEADER_BYTES + 8 {
            assert_eq!(split_message(&bytes[..end], Side::Engine).unwrap(), None);
        }
        let (payload, rest) = split_message(&bytes, Side::Engine)
            .unwrap()
            .expect("a message");
        assert_eq!(payload, [1; 8]);
        assert_eq!(rest, [2; 3]);
    }

    #[test]
    fn split_message_rejects_a_header_of_another_side() {
        let err = split_message(&header(Side::Engine, 0), Side::Server).expect_err("an error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_length_above_the_cap_is_an_error() {
        let len = (MAX_MESSAGE_BYTES + size_of::<Word>()) as u32;
        let err = read_error(&header_with(Side::Server.magic(), len));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
