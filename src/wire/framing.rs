//! The envelope that carries one encoded message over a byte stream.
//!
//! Cap'n Proto frames its own payload, and this envelope puts a header of
//! 12 bytes in front of it: a magic of four bytes, the player as a
//! little-endian `u32`, and the length of the payload as a little-endian
//! `u32`. The magic is `SI`, then `E` from the engine or `V` from the view,
//! then the version of the payload, `1`.
//!
//! The magic rejects text from another writer on the same pipe, such as a
//! stray `print` from the program of a student, before the bytes reach the
//! Cap'n Proto reader. It also says who wrote the message, since a Cap'n
//! Proto message does not name its root, and a peer that gets the magic of
//! its own side knows that the two ends have the same role. A reader that
//! does not know the version stops instead of reading what it cannot.
//!
//! The player routes a message between the engine and a server that
//! serves several views, and a view behind the server never sees it. The
//! server reads the header and passes the payload on untouched, so a view
//! cannot claim to be another player. A WebSocket, which frames its own messages, carries the
//! payload alone and the version in its subprotocol, `sinteract.v1`.
//!
//! A message goes from the builder to the writer, and from the reader into
//! the words that the decoder reads in place, with no copy in between.

use std::io::{self, Read, Write};

use capnp::Word;
use capnp::message::{Allocator, Builder};
use capnp::serialize;

/// The side of the session that writes a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Runs the program and sends the frames.
    Engine,
    /// Draws the frames and sends the input.
    View,
}

impl Side {
    /// The magic at the start of every message that this side writes.
    pub const fn magic(self) -> [u8; 4] {
        match self {
            Side::Engine => *b"SIE1",
            Side::View => *b"SIV1",
        }
    }

    fn other(self) -> Side {
        match self {
            Side::Engine => Side::View,
            Side::View => Side::Engine,
        }
    }
}

/// Who a message of the engine goes to, or who a message of the view comes
/// from, when a server routes the messages of several views.
pub type Player = u32;

/// A message that no server routes: from the engine it goes to every view,
/// and from the view it comes from the only one.
pub const UNROUTED: Player = 0;

/// The length of the header in front of each payload.
pub const HEADER_BYTES: usize = 12;

/// Cap on both sides, so a corrupted length cannot make the reader allocate
/// gigabytes. A 1080p RGBA pixmap is about 8 MiB.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Write `message` from `side` to `player`, with its envelope, and flush,
/// so the peer sees it at once. The message goes straight to `w`, which
/// should be buffered, since Cap'n Proto writes the segment table and each
/// segment apart. A message above the cap is not written, and the error is
/// [`io::ErrorKind::InvalidInput`].
pub fn write_framed<A: Allocator>(
    w: &mut impl Write,
    side: Side,
    player: Player,
    message: &Builder<A>,
) -> io::Result<()> {
    let len = serialize::compute_serialized_size_in_words(message) * size_of::<Word>();
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("message of {len} bytes exceeds cap {MAX_FRAME_BYTES}"),
        ));
    }
    let mut header = [0u8; HEADER_BYTES];
    header[..4].copy_from_slice(&side.magic());
    header[4..8].copy_from_slice(&player.to_le_bytes());
    header[8..].copy_from_slice(&(len as u32).to_le_bytes());
    w.write_all(&header)?;
    // The only errors of `write_message` come from `w`.
    serialize::write_message(&mut *w, message).map_err(io::Error::other)?;
    w.flush()
}

/// Read one message that `side` wrote, with its player, into the words that
/// [`read_message_from_flat_slice`](serialize::read_message_from_flat_slice)
/// reads in place. Returns `None` when the stream ends before the envelope.
/// The stream ending anywhere else is [`io::ErrorKind::UnexpectedEof`].
/// A magic that is not the one of `side`, and a length that is not a whole
/// number of words or exceeds the cap, are [`io::ErrorKind::InvalidData`].
pub fn read_framed(r: &mut impl Read, side: Side) -> io::Result<Option<(Player, Vec<Word>)>> {
    let mut header = [0u8; HEADER_BYTES];
    if !read_start(r, &mut header)? {
        return Ok(None);
    }
    let [m0, m1, m2, m3, p0, p1, p2, p3, l0, l1, l2, l3] = header;
    check_magic([m0, m1, m2, m3], side)?;
    let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_FRAME_BYTES || !len.is_multiple_of(size_of::<Word>()) {
        return Err(invalid(format!(
            "frame length {len} is not a whole number of words up to {MAX_FRAME_BYTES}"
        )));
    }
    let mut words = Word::allocate_zeroed_vec(len / size_of::<Word>());
    r.read_exact(Word::words_to_bytes_mut(&mut words))?;
    Ok(Some((u32::from_le_bytes([p0, p1, p2, p3]), words)))
}

/// Fill `buf`, or return `false` if the stream ends before its first byte.
fn read_start(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    loop {
        match r.read(buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                r.read_exact(&mut buf[n..])?;
                return Ok(true);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// Say what is wrong with a magic that is not the one of `side`.
fn check_magic(magic: [u8; 4], side: Side) -> io::Result<()> {
    let expected = side.magic();
    if magic == expected {
        return Ok(());
    }
    let other = side.other();
    Err(invalid(if magic == other.magic() {
        format!("got a message of the {other:?} side, which this end is too")
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

    fn header(magic: [u8; 4], len: u32) -> Vec<u8> {
        let mut out = magic.to_vec();
        out.extend_from_slice(&7u32.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out
    }

    fn read(bytes: &[u8]) -> io::Result<Option<(Player, Vec<Word>)>> {
        read_framed(&mut &bytes[..], Side::View)
    }

    fn read_error(bytes: &[u8]) -> io::Error {
        read(bytes).expect_err("an error")
    }

    #[test]
    fn a_message_round_trips_with_its_player() {
        let mut message = capnp::message::Builder::new_default();
        message.set_root("hi").unwrap();
        let mut bytes = Vec::new();
        write_framed(&mut bytes, Side::View, 7, &message).unwrap();
        assert_eq!(&bytes[..4], b"SIV1");
        let (player, words) = read(&bytes).unwrap().expect("a message");
        assert_eq!(player, 7);
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
        let err = read_error(&Side::View.magic());
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_stream_that_ends_inside_the_payload_is_an_error() {
        let mut bytes = header(Side::View.magic(), 16);
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(read_error(&bytes).kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_message_of_the_same_side_is_an_error() {
        let err = read_framed(&mut &header(Side::View.magic(), 0)[..], Side::Engine)
            .expect_err("an error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("View side"), "{err}");
    }

    #[test]
    fn an_unknown_version_is_an_error() {
        let err = read_error(&header(*b"SIV2", 0));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("version"), "{err}");
    }

    #[test]
    fn a_wrong_magic_is_an_error() {
        let err = read_error(&header(*b"junk", 0));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("not a sinteract stream"), "{err}");
    }

    #[test]
    fn a_length_that_is_not_whole_words_is_an_error() {
        let mut bytes = header(Side::View.magic(), 4);
        bytes.extend_from_slice(&[0; 4]);
        assert_eq!(read_error(&bytes).kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_length_above_the_cap_is_an_error() {
        let len = (MAX_FRAME_BYTES + size_of::<Word>()) as u32;
        let err = read_error(&header(Side::View.magic(), len));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
