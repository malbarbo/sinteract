//! [`StdioFrontend`] — frontend that talks the wire protocol over
//! stdin/stdout. Used when an engine host (`spython --server` /
//! `sgleam --server`) is launched as a subprocess of a game server: the
//! server feeds [`crate::event::InputEvent`]s on stdin and reads
//! [`crate::scene::Scene`] frames from stdout.
//!
//! ## Framing
//!
//! Each direction is a stream of length-prefixed Cap'n Proto `Message`s
//! wrapped in a tiny stdio envelope:
//!
//! ```text
//! +----+----+----+----+----+----+----+----+--------+
//! | S  | I  | M  | G  | u32 LE length     | bytes... |
//! +----+----+----+----+----+----+----+----+--------+
//! ```
//!
//! Cap'n Proto's `serialize::write_message` already emits a self-framed
//! payload (segment count + per-segment word counts), so the outer
//! `[SIMG][len]` is strictly defense in depth: it lets us reject garbage
//! from an accidental non-`simage` peer (a `print(...)` on the same pipe,
//! a shell prompt, …) *before* feeding bytes into the Cap'n Proto reader.
//!
//! ## Status
//!
//! [`StdioFrontend::wait_event`] and [`StdioFrontend::present`] are wired
//! through stdin/stdout and tested against in-memory mocks. The host-side
//! integration (CLI flag, world.run wiring) lives in `spython` / `sgleam`
//! and is intentionally deferred — see `simage/PLAN.md`, fase 5.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Mutex;
use std::time::Instant;

use crate::event::InputEvent;
use crate::scene::Scene;
use crate::wire::{self, Decoded, FILE_IDENTIFIER};

/// Maximum payload size we will accept on the read side. Hard cap so a
/// corrupted length prefix cannot make the frontend allocate gigabytes.
/// 64 MiB is well above any realistic frame (a 1080p RGBA pixmap is ~8 MiB).
const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// Frontend that talks the wire protocol on stdin/stdout. Construct with
/// [`StdioFrontend::new`] (uses real stdin/stdout) or
/// [`StdioFrontend::with_streams`] for tests. The peer (server/client on the
/// other side of the pipe) is responsible for emitting Vsync events — this
/// frontend is purely the protocol carrier.
pub struct StdioFrontend {
    inner: Mutex<Inner>,
}

struct Inner {
    reader: Box<dyn BufRead + Send>,
    writer: Box<dyn Write + Send>,
}

impl StdioFrontend {
    /// Real stdio. The framing is binary, so callers must make sure the
    /// host did not also write text to stdout (e.g. `print(...)` would
    /// corrupt the stream). Hosts typically rebind `stdout` to stderr for
    /// non-protocol output.
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

    /// No-op for stdio — the framing is the protocol; there is no
    /// "screen" to enter. Hosts call this to keep the lifecycle symmetric
    /// with terminal/window frontends.
    pub fn enter(&mut self) {}
    pub fn exit(&mut self) {}

    /// Send a frame to stdout. `flush` is performed so the consuming server
    /// sees the bytes immediately.
    pub fn present(&mut self, scene: &Scene) {
        let bytes = wire::encode_frame(scene);
        self.write_framed(&bytes);
    }

    /// Send a bitmap upload. Callers must do this *before* the first
    /// [`Self::present`] that references `id` — the server / client may
    /// stream-process and need the asset on hand to resolve the id.
    pub fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>) {
        let bytes = wire::encode_asset(id, blob, mime);
        self.write_framed(&bytes);
    }

    /// Send an explicit close. Most hosts do not need this — closing
    /// stdin / stdout (process exit) is enough.
    pub fn close(&mut self) {
        let bytes = wire::encode_close();
        self.write_framed(&bytes);
    }

    /// Block on stdin for the next [`InputEvent`]. Non-event messages
    /// (`Asset`, `Frame`) are protocol errors when received from a server
    /// upstream; we log to stderr and keep reading. `Close` terminates the
    /// session and surfaces as [`InputEvent::Close`].
    ///
    /// The `_deadline` argument is currently ignored — stdio reads are
    /// blocking on most platforms and we do not have a portable
    /// "read with timeout" yet. Tick scheduling is the server's
    /// responsibility (it sends `Tick` events on its own clock).
    pub fn wait_event(&mut self, _deadline: Option<Instant>) -> Option<InputEvent> {
        loop {
            match self.read_framed() {
                Ok(None) => return None, // EOF
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
            // The peer has gone away — there is no recovery from here.
            // Hosts observe the error indirectly: subsequent wait_event
            // will hit EOF and surface InputEvent::Close.
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

    /// `Vec<u8>` writer that can be inspected after the test runs.
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
        // The payload should round-trip through wire::decode as a Frame.
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
        // First message: an Asset (server should not send this on stdin,
        // but it may happen during protocol bring-up). Second: a real
        // KeyEvent. The frontend logs the first and yields the second.
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
        // wait_event surfaces the error as Close, not a panic.
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
        // Two framed messages back-to-back.
        assert!(buf.len() > 16);
        assert_eq!(&buf[0..4], &FILE_IDENTIFIER);
        let len1 = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        let after_first = 8 + len1;
        assert_eq!(&buf[after_first..after_first + 4], &FILE_IDENTIFIER);
    }
}
