//! [`Link`], the pipe to a server that [`super::Stdio`] shares with the
//! other displays that carry the protocol. A thread reads the messages of
//! the server into the queue, and the display writes its own messages
//! through the link.

use std::io::{self, BufRead, BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::driver::{OpenError, PresentError};
use super::inbox::{Inbox, Queued, Sender};
use crate::event::Interrupt;
use crate::wire::framing::UNROUTED;
use crate::wire::to_engine::{self, Message};
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

const WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// stdin belongs to the process, and a second reader would steal half of
/// the messages. It stays claimed after the display closes, because the
/// reader thread only ends at EOF.
static STDIN_CLAIMED: AtomicBool = AtomicBool::new(false);

impl<E: Queued> Link<E> {
    /// Read `reader` on a thread named `name`, and hand each message of the
    /// server to `route`. `route` returns the event to queue, `None` to skip
    /// the message, or an error that goes into the queue as
    /// [`Interrupt::Read`]. A close of the server never reaches `route`,
    /// since it ends the session. The queue makes a Vsync every
    /// `vsync_period`, or takes it from `route` when it is `None`.
    pub(super) fn new<R, W>(
        reader: R,
        writer: W,
        name: &str,
        vsync_period: Option<Duration>,
        route: impl Fn(Message) -> Result<Option<E>, ReadError> + Send + 'static,
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
        thread::Builder::new()
            .name(name.into())
            .spawn(move || read_loop(reader, tx, flag, route))?;
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
            let _ = to_view::write_close(&mut self.writer, UNROUTED);
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

/// Claim stdin for the reader of a display, or fail with
/// [`OpenError::Busy`] if a reader of another display holds it.
pub(super) fn claim_stdin() -> Result<(), OpenError> {
    if STDIN_CLAIMED.swap(true, Ordering::AcqRel) {
        return Err(OpenError::Busy);
    }
    Ok(())
}

/// Read the messages of the peer into the queue until the stream or the
/// session ends. [`to_engine::read`] skips a message or an event of an arm
/// from a newer schema. A payload that does not decode goes into the queue
/// as [`Interrupt::Read`] and the loop goes on, since the framing already
/// found where the next message starts.
fn read_loop<E>(
    mut reader: impl BufRead,
    tx: Sender<E>,
    peer_closed: Arc<AtomicBool>,
    route: impl Fn(Message) -> Result<Option<E>, ReadError>,
) {
    loop {
        let routed = match to_engine::read(&mut reader) {
            Ok(None | Some(Message::Close)) => break,
            Ok(Some(message)) => route(message),
            Err(e) => Err(e),
        };
        let sent = match routed {
            Ok(Some(ev)) => tx.send_event(ev),
            Ok(None) => continue,
            Err(e @ ReadError::Payload(_)) => tx.send_read_error(e),
            Err(e @ ReadError::Broken(_)) => {
                let _ = tx.send_read_error(e);
                break;
            }
        };
        if sent.is_err() {
            // The display is closed, and nobody reads the queue.
            return;
        }
    }
    peer_closed.store(true, Ordering::Release);
    let _ = tx.send_close();
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
