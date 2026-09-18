//! The envelope that carries one encoded message over a byte stream.
//!
//! Cap'n Proto frames its own payload, and this envelope adds the four
//! bytes `SINT` and a little-endian `u32` length in front of it. The magic
//! rejects text from another writer on the same pipe, such as a stray
//! `print` from the program of a student, before the bytes reach the Cap'n
//! Proto reader.
//!
//! A message goes from the builder to the writer, and from the reader into
//! the words that the decoder reads in place, with no copy in between.

use std::io::{self, Read, Write};

use capnp::Word;
use capnp::message::{Allocator, Builder};
use capnp::serialize;

/// Magic at the start of every framed message.
pub const FILE_IDENTIFIER: [u8; 4] = *b"SINT";

/// Cap on both sides, so a corrupted length prefix cannot make the reader
/// allocate gigabytes. A 1080p RGBA pixmap is about 8 MiB.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Write `message` with its envelope and flush, so the peer sees it at once.
/// The message goes straight to `w`, which should be buffered, since Cap'n
/// Proto writes the segment table and each segment apart. A message above
/// the cap is not written, and the error is [`io::ErrorKind::InvalidInput`].
pub fn write_framed<A: Allocator>(w: &mut impl Write, message: &Builder<A>) -> io::Result<()> {
    let len = serialize::compute_serialized_size_in_words(message) * size_of::<Word>();
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("message of {len} bytes exceeds cap {MAX_FRAME_BYTES}"),
        ));
    }
    w.write_all(&FILE_IDENTIFIER)?;
    w.write_all(&(len as u32).to_le_bytes())?;
    // The only errors of `write_message` come from `w`.
    serialize::write_message(&mut *w, message).map_err(io::Error::other)?;
    w.flush()
}

/// Read one framed message into the words that
/// [`read_message_from_flat_slice`](serialize::read_message_from_flat_slice)
/// reads in place. Returns `None` when the stream ends before the envelope.
/// The stream ending anywhere else is [`io::ErrorKind::UnexpectedEof`], and
/// a wrong magic or a length that is not a whole number of words or exceeds
/// the cap is [`io::ErrorKind::InvalidData`].
pub fn read_framed(r: &mut impl Read) -> io::Result<Option<Vec<Word>>> {
    let mut header = [0u8; 8];
    if !read_start(r, &mut header)? {
        return Ok(None);
    }
    let (magic, len) = header.split_at(4);
    if magic != FILE_IDENTIFIER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("framing magic mismatch: got {magic:?}"),
        ));
    }
    let len = u32::from_le_bytes(len.try_into().expect("4 bytes")) as usize;
    if len > MAX_FRAME_BYTES || !len.is_multiple_of(size_of::<Word>()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} is not a whole number of words up to {MAX_FRAME_BYTES}"),
        ));
    }
    let mut words = Word::allocate_zeroed_vec(len / size_of::<Word>());
    r.read_exact(Word::words_to_bytes_mut(&mut words))?;
    Ok(Some(words))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header(magic: &[u8; 4], len: u32) -> Vec<u8> {
        let mut out = magic.to_vec();
        out.extend_from_slice(&len.to_le_bytes());
        out
    }

    fn read(bytes: &[u8]) -> io::Result<Option<Vec<Word>>> {
        read_framed(&mut &bytes[..])
    }

    #[test]
    fn a_message_round_trips() {
        let mut message = capnp::message::Builder::new_default();
        message.set_root("hi").unwrap();
        let mut bytes = Vec::new();
        write_framed(&mut bytes, &message).unwrap();
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
        let err = read(&FILE_IDENTIFIER).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_stream_that_ends_inside_the_payload_is_an_error() {
        let mut bytes = header(&FILE_IDENTIFIER, 16);
        bytes.extend_from_slice(&[0; 8]);
        assert_eq!(
            read(&bytes).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn a_wrong_magic_is_an_error() {
        let err = read(&header(b"junk", 0)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_length_that_is_not_whole_words_is_an_error() {
        let mut bytes = header(&FILE_IDENTIFIER, 4);
        bytes.extend_from_slice(&[0; 4]);
        assert_eq!(read(&bytes).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_length_above_the_cap_is_an_error() {
        let len = (MAX_FRAME_BYTES + size_of::<Word>()) as u32;
        let err = read(&header(&FILE_IDENTIFIER, len)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
