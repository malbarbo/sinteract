//! The envelope that carries one encoded message over a byte stream.
//!
//! Cap'n Proto frames its own payload, and this envelope adds the four
//! bytes `SINT` and a little-endian `u32` length in front of it. The magic
//! rejects text from another writer on the same pipe, such as a stray
//! `print` from the program of a student, before the bytes reach the Cap'n
//! Proto reader.

use std::io::{self, Read, Write};

/// Magic at the start of every framed message.
pub const FILE_IDENTIFIER: [u8; 4] = *b"SINT";

/// Cap on the read side, so a corrupted length prefix cannot make the
/// reader allocate gigabytes. A 1080p RGBA pixmap is about 8 MiB.
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Write `payload` with its envelope and flush, so the peer sees it at once.
pub fn write_framed(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&FILE_IDENTIFIER)?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one framed payload. Returns `None` at a clean end of stream, and an
/// error when the magic does not match or the length exceeds the cap.
pub fn read_framed(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut magic = [0u8; 4];
    match r.read_exact(&mut magic) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    if magic != FILE_IDENTIFIER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("framing magic mismatch: got {magic:?}"),
        ));
    }

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
        ));
    }

    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}
