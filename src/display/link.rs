//! [`Link`] is the pipe that [`super::Stdio`] and [`super::Room`] share. It
//! goes to the peer of the engine, a view or a server. A thread reads the
//! messages of the peer into the queue, and the display writes its own
//! messages through the link.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::driver::{OpenError, PresentError};
use super::inbox::{Inbox, Queued, Sender};
use crate::event::Interrupt;
use crate::wire::{ReadError, to_view};

/// The two ends of the session over a pipe, and the queue that the reader
/// thread feeds. The thread blocks on the read and nothing interrupts it,
/// so it ends with the stream, at EOF or at a read error. Drop closes the
/// session.
pub(super) struct Link<E: Queued> {
    /// Cap'n Proto writes a message in pieces, so they gather here and go
    /// out with the flush at the end of each message. The buffer holds a
    /// frame of a few hundred elements, which then goes out in one write.
    writer: Writer,
    inbox: Inbox<E>,
    /// Set when the peer closes the session or stops reading.
    peer_closed: Arc<AtomicBool>,
    /// Set by [`Link::close`].
    closed: bool,
}

pub(super) type Writer = BufWriter<Box<dyn Write + Send>>;

/// What the reader of a display makes of the next message of the peer.
pub(super) enum Step<E> {
    /// The event to queue.
    Event(E),
    /// A message that the display does not queue.
    Skip,
    /// The stream ended or carried the close of the peer.
    End,
}

/// Stdin with the claim of the process on it. stdin belongs to the
/// process, and a second reader would steal half of the messages. The
/// claim ends when the reader drops it, at the end of the stream, at a
/// close of the peer, or when the display fails to open. A display that
/// closes while the stream goes on keeps the claim for the process, since
/// its reader already holds bytes of the next message.
pub(super) struct ClaimedStdin(BufReader<io::Stdin>);

const WRITE_BUFFER_BYTES: usize = 64 * 1024;

static STDIN_CLAIMED: AtomicBool = AtomicBool::new(false);

impl<E: Queued> Link<E> {
    /// Call `read` on `reader` from a thread named `name` until it returns
    /// [`Step::End`] or [`ReadError::Broken`], or the display closes. An
    /// error of `read` goes into the queue as [`Interrupt::Read`]. The queue
    /// makes a Vsync every `vsync_period`, or takes the Vsync from `read`
    /// when `vsync_period` is `None`.
    pub(super) fn new<R, W>(
        reader: R,
        writer: W,
        name: &str,
        vsync_period: Option<Duration>,
        read: impl FnMut(&mut R) -> Result<Step<E>, ReadError> + Send + 'static,
    ) -> io::Result<Self>
    where
        E: Send + 'static,
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let inbox = Inbox::new(vsync_period);
        let peer_closed = Arc::new(AtomicBool::new(false));
        let tx = inbox.sender();
        let flag = Arc::clone(&peer_closed);
        thread::Builder::new().name(name.into()).spawn(move || {
            let mut reader = reader;
            if !read_loop(&mut reader, tx, flag, read) {
                // The display closed while the stream goes on, so the
                // reader keeps the claim on stdin for the process.
                mem::forget(reader);
            }
        })?;
        Ok(Self {
            writer: BufWriter::with_capacity(WRITE_BUFFER_BYTES, Box::new(writer)),
            inbox,
            peer_closed,
            closed: false,
        })
    }

    /// Write one message with `write`, unless the session ended.
    pub(super) fn send(
        &mut self,
        write: impl FnOnce(&mut Writer) -> io::Result<()>,
    ) -> Result<(), PresentError> {
        if self.closed || self.peer_closed.load(Ordering::Acquire) {
            return Err(PresentError::Closed);
        }
        // A peer that stopped reading may keep stdin open, so the reader
        // thread sees no end and only this write fails. The caller decides
        // whether that ends the session.
        write(&mut self.writer).map_err(PresentError::Io)
    }

    /// The events of the peer and of the [`Sender`]s, in the order of
    /// arrival.
    pub(super) fn wait(&mut self, deadline: Option<Instant>) -> Result<E, Interrupt> {
        self.inbox.wait(deadline)
    }

    pub(super) fn sender(&self) -> Sender<E> {
        self.inbox.sender()
    }

    /// Tell the peer that the session ended, unless the peer ended it. A
    /// second call does nothing.
    pub(super) fn close(&mut self) {
        if self.closed {
            return;
        }
        if !self.peer_closed.load(Ordering::Acquire) {
            let _ = to_view::write_close(&mut self.writer);
        }
        self.closed = true;
        self.inbox.close();
    }
}

impl<E: Queued> Drop for Link<E> {
    fn drop(&mut self) {
        self.close();
    }
}

impl ClaimedStdin {
    /// Claim stdin, or fail with [`OpenError::Busy`] if the reader of a
    /// `Stdio` or a `Room` holds it.
    pub(super) fn claim() -> Result<Self, OpenError> {
        if STDIN_CLAIMED.swap(true, Ordering::AcqRel) {
            return Err(OpenError::Busy);
        }
        Ok(Self(BufReader::new(io::stdin())))
    }
}

impl Read for ClaimedStdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl BufRead for ClaimedStdin {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.0.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.0.consume(amount);
    }
}

impl Drop for ClaimedStdin {
    fn drop(&mut self) {
        STDIN_CLAIMED.store(false, Ordering::Release);
    }
}

/// Read the messages of the peer into the queue until the stream or the
/// session ends. Returns `true` if the stream ended, broke or carried the
/// close of the peer, `false` if the display closed first. A payload that
/// does not decode goes into the queue as [`Interrupt::Read`] and the loop
/// goes on, since the framing already found where the next message starts.
fn read_loop<R, E>(
    reader: &mut R,
    tx: Sender<E>,
    peer_closed: Arc<AtomicBool>,
    mut read: impl FnMut(&mut R) -> Result<Step<E>, ReadError>,
) -> bool {
    loop {
        let sent = match read(reader) {
            Ok(Step::Event(ev)) => tx.send_event(ev),
            Ok(Step::Skip) => continue,
            Ok(Step::End) => break,
            Err(e @ ReadError::Payload(_)) => tx.send_read_error(e),
            Err(e @ ReadError::Broken(_)) => {
                let _ = tx.send_read_error(e);
                break;
            }
        };
        if sent.is_err() {
            // The display is closed, and nobody reads the queue.
            return false;
        }
    }
    peer_closed.store(true, Ordering::Release);
    let _ = tx.send_close();
    true
}

/// A writer whose bytes a test reads back.
#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct SharedWriter(Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl SharedWriter {
    pub(super) fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_goes_back_when_its_reader_drops() {
        let first = ClaimedStdin::claim().unwrap();
        assert!(matches!(ClaimedStdin::claim(), Err(OpenError::Busy)));
        drop(first);
        assert!(ClaimedStdin::claim().is_ok());
    }
}
