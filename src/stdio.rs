//! [`StdioFrontend`] talks the wire protocol over stdin and stdout. A game
//! server runs the engine host (`spython --server`, `sgleam --server`) as a
//! subprocess, writes [`crate::event::InputEvent`]s to its stdin and reads
//! [`crate::scene::Scene`] frames from its stdout.
//!
//! Each message is a Cap'n Proto `Message` inside an envelope of the four
//! bytes `SIMG` and a little-endian `u32` length. Cap'n Proto already frames
//! its own payload. The envelope rejects text from another writer on the
//! same pipe, such as a stray `print`, before the bytes reach the Cap'n Proto
//! reader.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Mutex;
use std::time::Instant;

use crate::event::InputEvent;
use crate::scene::Scene;
use crate::wire::{self, Decoded, FILE_IDENTIFIER};

/// Cap on the read side, so a corrupted length prefix cannot make the
/// frontend allocate gigabytes. A 1080p RGBA pixmap is about 8 MiB.
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Construct with [`StdioFrontend::new`] for the real stdin and stdout, or
/// with [`StdioFrontend::with_streams`] in a test. The peer emits the Vsync
/// events. This frontend only carries the protocol.
pub struct StdioFrontend {
    inner: Mutex<Inner>,
}

struct Inner {
    reader: Box<dyn BufRead + Send>,
    writer: Box<dyn Write + Send>,
}

impl StdioFrontend {
    /// The framing is binary, so the host must not write text to stdout. A
    /// host rebinds stdout to stderr for its other output.
    pub fn new() -> Self {
        Self::with_streams(BufReader::new(io::stdin()), io::stdout())
    }

    pub fn with_streams<R: BufRead + Send + 'static, W: Write + Send + 'static>(
        reader: R,
        writer: W,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                reader: Box::new(reader),
                writer: Box::new(writer),
            }),
        }
    }

    /// The framing needs no setup. `enter` and `exit` exist so the host
    /// drives every frontend the same way.
    pub fn enter(&mut self) {}
    pub fn exit(&mut self) {}

    /// Send a frame and flush, so the server sees it at once.
    pub fn present(&mut self, scene: &Scene) {
        let bytes = wire::encode_frame(scene);
        self.write_framed(&bytes);
    }

    /// Send a bitmap upload. Call it before the first [`Self::present`] that
    /// references `id`, because the peer may process the stream as it
    /// arrives.
    pub fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>) {
        let bytes = wire::encode_asset(id, blob, mime);
        self.write_framed(&bytes);
    }

    /// Send an explicit close. Closing stdout at process exit is enough for
    /// most hosts.
    pub fn close(&mut self) {
        let bytes = wire::encode_close();
        self.write_framed(&bytes);
    }

    /// Block on stdin for the next [`InputEvent`]. An `Asset` or a `Frame`
    /// is a protocol error from the server, logged to stderr and skipped.
    /// `Close` arrives as [`InputEvent::Close`], and so does a read or a
    /// decode error. EOF returns `None`.
    ///
    /// `_deadline` is ignored. A stdio read blocks, and there is no portable
    /// read with a timeout. The server sends the Vsync events on its own
    /// clock.
    pub fn wait_event(&mut self, _deadline: Option<Instant>) -> Option<InputEvent> {
        loop {
            match self.read_framed() {
                Ok(None) => return None,
                Ok(Some(bytes)) => match wire::decode(&bytes) {
                    Ok(Decoded::Event(ev)) => return Some(ev),
                    Ok(Decoded::Close) => return Some(InputEvent::Close),
                    Ok(other) => {
                        eprintln!(
                            "[simage::stdio] ignoring unexpected message from server: {other:?}"
                        );
                    }
                    Err(e) => {
                        eprintln!("[simage::stdio] decode error: {e}");
                        return Some(InputEvent::Close);
                    }
                },
                Err(e) => {
                    eprintln!("[simage::stdio] read error: {e}");
                    return Some(InputEvent::Close);
                }
            }
        }
    }

    fn write_framed(&mut self, payload: &[u8]) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let len = payload.len() as u32;
        if g.writer.write_all(&FILE_IDENTIFIER).is_err()
            || g.writer.write_all(&len.to_le_bytes()).is_err()
            || g.writer.write_all(payload).is_err()
            || g.writer.flush().is_err()
        {
            // The peer is gone. The host learns it when the next wait_event
            // hits EOF.
            eprintln!("[simage::stdio] write to stdout failed; peer may have closed");
        }
    }

    fn read_framed(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };

        let mut magic = [0u8; 4];
        match g.reader.read_exact(&mut magic) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        if magic != FILE_IDENTIFIER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("stdio framing magic mismatch: got {magic:?}"),
            ));
        }

        let mut len_buf = [0u8; 4];
        g.reader.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("stdio frame length {len} exceeds cap {MAX_FRAME_BYTES}"),
            ));
        }

        let mut payload = vec![0u8; len as usize];
        g.reader.read_exact(&mut payload)?;
        Ok(Some(payload))
    }
}

impl Default for StdioFrontend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent as IrKeyEvent, KeyKind};
    use crate::scene::{Paint, PathStyle};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex as StdMutex};

    /// A writer whose bytes the test reads back.
    #[derive(Clone, Default)]
    struct SharedWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 8);
        out.extend_from_slice(&FILE_IDENTIFIER);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn present_writes_framed_frame_message() {
        let written = SharedWriter::default();
        let mut fr = StdioFrontend::with_streams(
            BufReader::new(Cursor::new(Vec::<u8>::new())),
            written.clone(),
        );

        let mut scene = Scene::new(10.0, 10.0);
        {
            let mut p = scene.path(PathStyle {
                fill: Paint::rgba(1, 2, 3, 1.0),
                ..PathStyle::default()
            });
            p.move_to(0.0, 0.0);
            p.line_to(10.0, 10.0);
        }
        fr.present(&scene);

        let buf = written.0.lock().unwrap().clone();
        assert!(buf.len() > 8);
        assert_eq!(&buf[0..4], &FILE_IDENTIFIER);
        let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        assert_eq!(buf.len(), 8 + len, "framing length mismatch");
        match wire::decode(&buf[8..]).expect("decode") {
            Decoded::Frame(d) => {
                assert_eq!(d.width, 10.0);
                assert!(!d.elements.is_empty());
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[test]
    fn wait_event_reads_key_event() {
        let payload = wire::encode_event(&InputEvent::Key(IrKeyEvent {
            kind: KeyKind::Press,
            key: "ArrowDown".into(),
            modifiers: 0,
        }));
        let stream = frame(&payload);
        let mut fr =
            StdioFrontend::with_streams(BufReader::new(Cursor::new(stream)), Vec::<u8>::new());

        match fr.wait_event(None).expect("event") {
            InputEvent::Key(k) => {
                assert_eq!(k.key, "ArrowDown");
                assert_eq!(k.kind, KeyKind::Press);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn wait_event_returns_none_on_eof() {
        let mut fr = StdioFrontend::with_streams(
            BufReader::new(Cursor::new(Vec::<u8>::new())),
            Vec::<u8>::new(),
        );
        assert!(fr.wait_event(None).is_none());
    }

    #[test]
    fn wait_event_skips_unexpected_messages() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(&wire::encode_asset(1, b"png", Some("image/png"))));
        stream.extend_from_slice(&frame(&wire::encode_event(&InputEvent::Vsync)));
        let mut fr =
            StdioFrontend::with_streams(BufReader::new(Cursor::new(stream)), Vec::<u8>::new());
        assert!(fr.wait_event(None).unwrap().is_vsync());
    }

    #[test]
    fn close_message_surfaces_as_close_event() {
        let stream = frame(&wire::encode_close());
        let mut fr =
            StdioFrontend::with_streams(BufReader::new(Cursor::new(stream)), Vec::<u8>::new());
        assert!(fr.wait_event(None).unwrap().is_close());
    }

    #[test]
    fn missing_magic_is_an_error_not_a_panic() {
        let mut bad = Vec::new();
        bad.extend_from_slice(b"junk");
        bad.extend_from_slice(&[0u8; 4]);
        let mut fr =
            StdioFrontend::with_streams(BufReader::new(Cursor::new(bad)), Vec::<u8>::new());
        assert!(fr.wait_event(None).unwrap().is_close());
    }

    #[test]
    fn push_asset_then_present_share_writer() {
        let written = SharedWriter::default();
        let mut fr = StdioFrontend::with_streams(
            BufReader::new(Cursor::new(Vec::<u8>::new())),
            written.clone(),
        );
        fr.push_asset(7, b"\x89PNG\r\n", Some("image/png"));
        fr.present(&Scene::new(8.0, 8.0));
        let buf = written.0.lock().unwrap().clone();
        assert!(buf.len() > 16);
        assert_eq!(&buf[0..4], &FILE_IDENTIFIER);
        let len1 = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let after_first = 8 + len1;
        assert_eq!(&buf[after_first..after_first + 4], &FILE_IDENTIFIER);
    }
}
