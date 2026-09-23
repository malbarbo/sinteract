//! [`Stdio`] talks the wire protocol over stdin and stdout. A game server
//! runs the engine (`spython --server`, `sgleam --server`) as a
//! subprocess, writes the input of the view to its stdin and reads
//! [`Scene`] frames from its stdout.
//!
//! The display is the engine side of the session. It writes with
//! [`crate::wire::to_view`] and reads with [`crate::wire::to_engine`], and
//! each message goes inside the envelope of [`crate::wire::framing`]. It
//! serves one view, so its messages go to
//! [`UNROUTED`].

use std::io::{self, BufRead, Write};
use std::time::Instant;

use super::driver::{OpenError, PresentError};
use super::inbox::Sender;
use super::link::{ClaimedStdin, Link};
use crate::event::{Event, Interrupt};
use crate::scene::Scene;
use crate::wire::framing::UNROUTED;
use crate::wire::to_engine::Message;
use crate::wire::{ReadError, to_view};

/// A display that shows nothing. The peer sends the Vsync events, and this
/// display only carries the protocol.
///
/// A thread reads the input and feeds the queue, so a [`Sender`] wakes
/// [`Display::wait_event`](super::Display::wait_event) and the deadline
/// holds.
pub struct Stdio {
    link: Link<Event>,
}

impl Stdio {
    /// Talk over the stdin and the stdout of the process. The framing is
    /// binary, so the engine must not write text to stdout. An engine rebinds
    /// stdout to stderr for its other output.
    ///
    /// Fails with [`OpenError::Busy`] while another display reads stdin, and
    /// with [`OpenError::Io`] if the reader thread does not start.
    pub fn new() -> Result<Self, OpenError> {
        Self::with_streams(ClaimedStdin::claim()?, io::stdout()).map_err(OpenError::Io)
    }

    /// Talk over `reader` and `writer`, as a test does.
    pub fn with_streams<R, W>(reader: R, writer: W) -> io::Result<Self>
    where
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let link = Link::new(reader, writer, "sinteract-stdio", None, route)?;
        Ok(Self { link })
    }
}

impl super::Display for Stdio {
    /// Send a frame and flush, so the peer sees it at once.
    fn present(&mut self, scene: Scene) -> Result<(), PresentError> {
        self.link
            .send(|w| to_view::write_frame(w, UNROUTED, &scene))
    }

    /// The events of the peer and of the [`Sender`]s, in the order of
    /// arrival. A read error or EOF arrives as [`Interrupt::Close`].
    fn wait_event(&mut self, deadline: Option<Instant>) -> Result<Event, Interrupt> {
        self.link.wait(deadline)
    }

    fn sender(&self) -> Sender {
        self.link.sender()
    }

    /// Send the asset to the view, which answers nothing, so a success
    /// says that the asset went out and not that the view drew it.
    fn push_asset(&mut self, id: u32, blob: &[u8], mime: Option<&str>) -> Result<(), PresentError> {
        self.link
            .send(|w| to_view::write_asset(w, UNROUTED, id, blob, mime))
    }

    /// Tell the peer that the session ended, unless the peer ended it.
    fn close(&mut self) {
        self.link.close();
    }
}

impl super::driver::sealed::Sealed for Stdio {}

/// The event of a message of the server. The display serves one view, so
/// it takes the input of every player as its own, and skips the players
/// that join and leave.
fn route(message: Message) -> Result<Option<Event>, ReadError> {
    match message {
        Message::Input { event, .. } => Ok(Some(Event::Input(event))),
        Message::Start(_) | Message::Join { .. } | Message::Leave { .. } => Ok(None),
        Message::Close => unreachable!("the link ends the session at a close"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Display;
    use crate::display::link::SharedWriter;
    use crate::event::{InputEvent, KeyEvent as IrKeyEvent, KeyKind, Modifiers};
    use crate::protocol_capnp::server_message;
    use crate::scene::{Paint, PathStyle};
    use crate::wire::framing::{Side, header};
    use crate::wire::to_engine::{self, encode_close, encode_input};
    use crate::wire::{self, to_view};
    use std::io::{BufReader, Cursor, PipeWriter};
    use std::num::NonZeroU32;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    /// `ev` as the server writes it.
    fn event(ev: &InputEvent) -> Vec<u8> {
        let mut out = Vec::new();
        to_engine::write_input(&mut out, UNROUTED, ev).unwrap();
        out
    }

    /// `payload` in the envelope of the server, for a payload that
    /// [`to_engine`] does not write.
    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut out = header(Side::Server, UNROUTED, payload.len() as u32).to_vec();
        out.extend_from_slice(payload);
        out
    }

    /// A display over `input`, which then ends.
    fn reading(input: Vec<u8>) -> Stdio {
        Stdio::with_streams(Cursor::new(input), Vec::<u8>::new()).unwrap()
    }

    /// A display whose input stays open while the returned writer lives.
    fn open_session() -> (Stdio, PipeWriter, SharedWriter) {
        let (r, w) = io::pipe().unwrap();
        let written = SharedWriter::default();
        let fr = Stdio::with_streams(BufReader::new(r), written.clone()).unwrap();
        (fr, w, written)
    }

    fn input(fr: &mut Stdio) -> InputEvent {
        match fr.wait_event(None) {
            Ok(Event::Input(ev)) => ev,
            other => panic!("got {other:?}"),
        }
    }

    /// Returns `true` if the next wait of `fr` gives Close, `false`
    /// otherwise.
    fn closes(fr: &mut Stdio) -> bool {
        matches!(fr.wait_event(None), Err(Interrupt::Close))
    }

    /// Returns `true` if the next wait of `fr` gives a read that broke and
    /// the one after gives Close, `false` otherwise.
    fn breaks(fr: &mut Stdio) -> bool {
        matches!(
            fr.wait_event(None),
            Err(Interrupt::Read(ReadError::Broken(_)))
        ) && closes(fr)
    }

    fn decode_messages(mut buf: &[u8]) -> Vec<to_view::Message> {
        let mut out = Vec::new();
        while let Some((player, m)) = to_view::read(&mut buf).expect("decode") {
            assert_eq!(player, UNROUTED);
            out.push(m);
        }
        out
    }

    #[test]
    fn present_writes_framed_frame_message() {
        let (mut fr, _input, written) = open_session();
        let mut scene = Scene::new(10.0, 10.0);
        {
            let mut p = scene.path(
                PathStyle {
                    fill: Paint::rgba(1, 2, 3, 1.0),
                    ..PathStyle::default()
                },
                0.0,
                0.0,
            );
            p.line_to(10.0, 10.0);
        }
        fr.present(scene).expect("the frame goes out");
        match &decode_messages(&written.bytes())[..] {
            [to_view::Message::Frame(d)] => {
                assert_eq!(d.width(), 10.0);
                assert!(!d.elements().is_empty());
            }
            other => panic!("expected one Frame, got {other:?}"),
        }
    }

    #[test]
    fn wait_event_reads_key_event() {
        let mut fr = reading(event(&InputEvent::Key(IrKeyEvent {
            kind: KeyKind::Press,
            key: "ArrowDown".into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })));
        match input(&mut fr) {
            InputEvent::Key(k) => {
                assert_eq!(k.key, "ArrowDown");
                assert_eq!(k.kind, KeyKind::Press);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn eof_closes_for_good() {
        let mut fr = reading(Vec::new());
        assert!(closes(&mut fr));
        assert!(closes(&mut fr));
    }

    #[test]
    fn wait_event_times_out_while_the_peer_is_silent() {
        let (mut fr, _input, _) = open_session();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert!(matches!(
            fr.wait_event(Some(deadline)),
            Err(Interrupt::Timeout)
        ));
    }

    #[test]
    fn a_sender_wakes_wait_event_while_the_peer_is_silent() {
        let (mut fr, _input, _) = open_session();
        let tx = fr.sender();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.wake().unwrap();
        });
        assert!(matches!(fr.wait_event(None), Err(Interrupt::Wake)));
        t.join().unwrap();
    }

    #[test]
    fn wait_event_skips_a_message_and_an_event_of_an_unknown_arm() {
        let unknown_message = wire::with_unknown_server_value(&encode_close(), |m| wire::tag_of(m));
        let unknown_event =
            wire::with_unknown_server_value(&encode_input(&InputEvent::Vsync), |m| {
                let Ok(server_message::Event(e)) = m.which() else {
                    panic!("not an event");
                };
                wire::tag_of(e.unwrap())
            });
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(&unknown_message));
        stream.extend_from_slice(&frame(&unknown_event));
        stream.extend_from_slice(&event(&InputEvent::Vsync));
        assert!(matches!(input(&mut reading(stream)), InputEvent::Vsync));
    }

    #[test]
    fn wait_event_reports_a_payload_that_does_not_decode() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(&[0xff; 8]));
        stream.extend_from_slice(&event(&InputEvent::Vsync));
        let mut fr = reading(stream);
        assert!(matches!(
            fr.wait_event(None),
            Err(Interrupt::Read(ReadError::Payload(_)))
        ));
        assert!(matches!(input(&mut fr), InputEvent::Vsync));
    }

    #[test]
    fn wait_event_skips_the_players() {
        let mut stream = Vec::new();
        let ana = to_engine::Member {
            player: NonZeroU32::MIN,
            nickname: "Ana".into(),
        };
        let beto = NonZeroU32::new(2).unwrap();
        let roster = to_engine::Roster::new(vec![ana]).unwrap();
        to_engine::write_start(&mut stream, &roster).unwrap();
        to_engine::write_join(&mut stream, beto, "Beto").unwrap();
        to_engine::write_leave(&mut stream, beto).unwrap();
        stream.extend_from_slice(&event(&InputEvent::Vsync));
        assert!(matches!(input(&mut reading(stream)), InputEvent::Vsync));
    }

    #[test]
    fn a_close_of_the_peer_surfaces_as_close() {
        let mut stream = Vec::new();
        to_engine::write_close(&mut stream).unwrap();
        let mut fr = reading(stream);
        assert!(closes(&mut fr));
    }

    #[test]
    fn missing_magic_is_an_error_not_a_panic() {
        let mut bad = header(Side::Server, UNROUTED, 0);
        bad[..4].copy_from_slice(b"junk");
        assert!(breaks(&mut reading(bad.to_vec())));
    }

    #[test]
    fn a_message_of_another_engine_ends_the_session() {
        let mut stream = Vec::new();
        to_view::write_close(&mut stream, UNROUTED).unwrap();
        stream.extend_from_slice(&event(&InputEvent::Vsync));
        assert!(breaks(&mut reading(stream)));
    }

    #[test]
    fn push_asset_then_present_share_writer() {
        let (mut fr, _input, written) = open_session();
        fr.push_asset(7, b"\x89PNG\r\n", Some("image/png"))
            .expect("the asset goes out");
        fr.present(Scene::new(8.0, 8.0))
            .expect("the frame goes out");
        match &decode_messages(&written.bytes())[..] {
            [to_view::Message::Asset { .. }, to_view::Message::Frame(_)] => {}
            other => panic!("expected an Asset and a Frame, got {other:?}"),
        }
    }

    #[test]
    fn close_tells_the_peer_once() {
        let (mut fr, _input, written) = open_session();
        fr.close();
        fr.close();
        assert!(matches!(
            fr.present(Scene::new(8.0, 8.0)),
            Err(PresentError::Closed)
        ));
        assert!(matches!(
            &decode_messages(&written.bytes())[..],
            [to_view::Message::Close]
        ));
        assert!(closes(&mut fr));
    }

    /// A writer whose peer stopped reading. It counts the attempts.
    #[derive(Clone, Default)]
    struct BrokenWriter(Arc<Mutex<usize>>);

    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            *self.0.lock().unwrap() += 1;
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_reports_the_error_and_keeps_the_session() {
        // The input stays open, so only the write can fail.
        let (r, _input) = io::pipe().unwrap();
        let broken = BrokenWriter::default();
        let mut fr = Stdio::with_streams(BufReader::new(r), broken.clone()).unwrap();
        assert!(matches!(
            fr.present(Scene::new(8.0, 8.0)),
            Err(PresentError::Io(_))
        ));
        // The caller decides what a lost frame means, so the next one
        // tries again.
        assert!(matches!(
            fr.present(Scene::new(8.0, 8.0)),
            Err(PresentError::Io(_))
        ));
        fr.close();
        assert_eq!(*broken.0.lock().unwrap(), 3);
    }

    #[test]
    fn close_after_the_peer_closed_writes_nothing() {
        let written = SharedWriter::default();
        let mut fr = Stdio::with_streams(Cursor::new(Vec::new()), written.clone()).unwrap();
        assert!(closes(&mut fr));
        fr.close();
        assert!(written.bytes().is_empty());
    }
}
