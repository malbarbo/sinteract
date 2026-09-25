//! [`Session`] is the engine side of a session. The server, or the page
//! that plays the part of the server, writes a `ServerMessage` stream to
//! the engine, and the session turns the bytes into [`SessionEvent`]s with
//! the rules of the protocol. The engine writes its frames back with
//! [`crate::wire::to_view`].
//!
//! The session does no I/O of its own. A host that reads the bytes itself,
//! such as JavaScript through wasm, calls [`Session::feed`] and
//! [`Session::next_event`]. A host that reads a [`Read`], such as an
//! engine on fd 3, calls [`Session::wait`].

use std::collections::VecDeque;
use std::io::{self, Read};
use std::num::NonZeroU32;

use crate::event::InputEvent;
use crate::wire;
use crate::wire::framing::{self, Side};
use crate::wire::to_engine::{self, Message, Roster};

/// The engine side of a session, from the bytes of the server to the
/// events of the engine.
///
/// The queue keeps one Vsync, so an engine that falls behind the server
/// gets one Vsync and not a burst. A move of the mouse or a resize replaces
/// one of the same player that still waits, since only the latest one
/// counts. It passes the input of the other players, and stops at anything
/// else.
#[derive(Debug, Default)]
pub struct Session {
    /// The bytes that do not make a whole message yet.
    bytes: Vec<u8>,
    state: State,
    events: VecDeque<SessionEvent>,
}

/// What [`Session::next_event`] and [`Session::wait`] deliver.
#[derive(Debug)]
pub enum SessionEvent {
    /// The players of the session, who are the same until its end. It comes
    /// before every other event of the server, and once.
    Start(Roster),
    /// Time to draw the next frames, for every player.
    Vsync,
    /// The input of `player`, never [`InputEvent::Vsync`].
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
    /// The server dropped the asset of this id, so the engine sends the
    /// image again before a frame that draws it.
    Lost(u32),
    /// The session dropped a message that broke a rule, and goes on.
    Error(SessionError),
    /// The stream ended, which is how the server ends the session. It is
    /// the last event. The error is `Some` when the stream broke, with a
    /// header that is not one of the server or with the end inside a
    /// message, since the session cannot find the next message.
    End(Option<io::Error>),
}

/// Why the session dropped a message.
#[derive(Debug)]
pub enum SessionError {
    /// The message does not decode.
    Payload(wire::Error),
    /// A message came before the start.
    BeforeStart,
    /// A start came after the first one.
    SecondStart,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Payload(e) => write!(f, "message does not decode: {e}"),
            SessionError::BeforeStart => write!(f, "a message came before the start"),
            SessionError::SecondStart => write!(f, "a start came after the first one"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Payload(e) => Some(e),
            SessionError::BeforeStart | SessionError::SecondStart => None,
        }
    }
}

/// How much [`Session::wait`] asks of the reader at a time.
const READ_BYTES: usize = 64 * 1024;

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next bytes of the server. They may end anywhere, inside a
    /// message too. The session ignores what comes after its end.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.state == State::Ended {
            return;
        }
        self.bytes.extend_from_slice(bytes);
        self.take_messages();
    }

    /// Say that the stream ended. The part of a message that is left breaks
    /// the stream.
    pub fn end(&mut self) {
        if self.state == State::Ended {
            return;
        }
        let broken = (!self.bytes.is_empty()).then(|| io::ErrorKind::UnexpectedEof.into());
        self.finish(broken);
    }

    /// The next event, or `None` while the next one waits for more bytes
    /// and after the end.
    pub fn next_event(&mut self) -> Option<SessionEvent> {
        self.events.pop_front()
    }

    /// The next event, with bytes from `r` when none waits. It blocks for
    /// as long as `r` blocks. After the end, it returns `End(None)` without
    /// a read. An error of `r` other than [`io::ErrorKind::Interrupted`]
    /// comes back as is, and the session keeps the bytes that it read
    /// before.
    pub fn wait(&mut self, r: &mut impl Read) -> io::Result<SessionEvent> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            if self.state == State::Ended {
                return Ok(SessionEvent::End(None));
            }
            let len = self.bytes.len();
            self.bytes.resize(len + READ_BYTES, 0);
            match r.read(self.bytes.get_mut(len..).expect("the bytes just added")) {
                Ok(n) => {
                    self.bytes.truncate(len + n);
                    if n == 0 {
                        self.end();
                    } else {
                        self.take_messages();
                    }
                }
                Err(e) => {
                    self.bytes.truncate(len);
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Turn every whole message in `bytes` into events.
    fn take_messages(&mut self) {
        let mut at = 0;
        while self.state != State::Ended {
            let bytes = self.bytes.get(at..).expect("at is inside the bytes");
            let (payload, rest) = match framing::split_frame(bytes, Side::Server) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => return self.finish(Some(e)),
            };
            let decoded = to_engine::decode(payload);
            at = self.bytes.len() - rest.len();
            match decoded {
                Ok(Some(message)) => self.receive(message),
                Ok(None) => {}
                Err(e) => self.push(SessionEvent::Error(SessionError::Payload(e))),
            }
        }
        // The end already dropped every byte.
        if self.state != State::Ended {
            self.bytes.drain(..at);
        }
    }

    /// Turn `message` into an event, by the state of the session.
    fn receive(&mut self, message: Message) {
        let event = match (self.state, message) {
            (State::BeforeStart, Message::Start(roster)) => {
                self.state = State::Started;
                SessionEvent::Start(roster)
            }
            (State::BeforeStart, Message::Input { .. } | Message::Tick | Message::Lost(_)) => {
                SessionEvent::Error(SessionError::BeforeStart)
            }
            (State::Started, Message::Start(_)) => SessionEvent::Error(SessionError::SecondStart),
            (State::Started, Message::Tick) => SessionEvent::Vsync,
            (State::Started, Message::Lost(id)) => SessionEvent::Lost(id),
            (State::Started, Message::Input { player, event }) => {
                SessionEvent::Input { player, event }
            }
            (State::Ended, message) => {
                unreachable!("take_messages stops at the end, not at {message:?}")
            }
        };
        self.push(event);
    }

    /// Queue `event`, dropping a second Vsync and a move or a resize that
    /// `event` replaces.
    fn push(&mut self, event: SessionEvent) {
        match &event {
            SessionEvent::Vsync => {
                if self.events.iter().any(|e| matches!(e, SessionEvent::Vsync)) {
                    return;
                }
            }
            SessionEvent::Input { player, event: new } => {
                if let Some(old) = self.superseded(*player, new) {
                    *old = event;
                    return;
                }
            }
            SessionEvent::Start(_)
            | SessionEvent::Lost(_)
            | SessionEvent::Error(_)
            | SessionEvent::End(_) => {}
        }
        self.events.push_back(event);
    }

    /// The waiting input of `player` that `new` replaces, looking from the
    /// back past the input of the other players. The events of one player
    /// keep their order, and nothing moves across a Vsync.
    fn superseded(&mut self, player: NonZeroU32, new: &InputEvent) -> Option<&mut SessionEvent> {
        for old in self.events.iter_mut().rev() {
            match old {
                SessionEvent::Input { player: p, event } if *p == player => {
                    return new.supersedes(event).then_some(old);
                }
                SessionEvent::Input { .. } => {}
                SessionEvent::Lost(_) => {}
                SessionEvent::Start(_)
                | SessionEvent::Vsync
                | SessionEvent::Error(_)
                | SessionEvent::End(_) => return None,
            }
        }
        None
    }

    /// End the session, with the error of a broken stream. Nothing comes
    /// after the `End`.
    fn finish(&mut self, broken: Option<io::Error>) {
        self.state = State::Ended;
        self.bytes = Vec::new();
        self.events.push_back(SessionEvent::End(broken));
    }
}

/// Whether the session waits for its start, runs, or ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    BeforeStart,
    Started,
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyKind, Modifiers, MouseAction, MouseButtons, MouseEvent};
    use crate::wire::framing::HEADER_BYTES;
    use crate::wire::to_engine::Member;

    fn player(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn at(x: f32) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
            action: MouseAction::Move,
            x,
            y: 0.0,
            modifiers: Modifiers::default(),
            buttons: MouseButtons::default(),
        })
    }

    fn resize(width: f32) -> InputEvent {
        InputEvent::Resize { width, height: 1.0 }
    }

    /// A start with Ana as player 1 and Beto as player 2.
    fn start() -> Vec<u8> {
        let roster = Roster::new(vec![
            Member {
                player: player(1),
                nickname: "Ana".into(),
            },
            Member {
                player: player(2),
                nickname: "Beto".into(),
            },
        ])
        .unwrap();
        let mut out = Vec::new();
        to_engine::write_start(&mut out, &roster).unwrap();
        out
    }

    fn tick() -> Vec<u8> {
        let mut out = Vec::new();
        to_engine::write_tick(&mut out).unwrap();
        out
    }

    fn input(out: &mut Vec<u8>, p: u32, ev: &InputEvent) {
        to_engine::write_input(out, player(p), ev).unwrap();
    }

    /// `payload` with the envelope of the server.
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = framing::header(Side::Server, payload.len() as u32).to_vec();
        out.extend_from_slice(payload);
        out
    }

    /// A session after its start, fed with `bytes`.
    fn started(bytes: &[u8]) -> Session {
        let mut session = Session::new();
        session.feed(&start());
        assert!(matches!(session.next_event(), Some(SessionEvent::Start(_))));
        session.feed(bytes);
        session
    }

    /// Every event that waits, in a short form.
    fn names(session: &mut Session) -> Vec<String> {
        std::iter::from_fn(|| session.next_event())
            .map(|e| match e {
                SessionEvent::Start(r) => format!("start {}", r.members().len()),
                SessionEvent::Vsync => "vsync".into(),
                SessionEvent::Lost(id) => format!("lost {id}"),
                SessionEvent::Input { player, event } => match event {
                    InputEvent::Key(k) => format!("{player} key {}", k.key),
                    InputEvent::Mouse(m) => format!("{player} move {}", m.x),
                    InputEvent::Resize { width, .. } => format!("{player} resize {width}"),
                    InputEvent::Vsync | InputEvent::Pad(_) => format!("{player} {event:?}"),
                },
                SessionEvent::Error(e) => format!("error {e}"),
                SessionEvent::End(None) => "end".into(),
                SessionEvent::End(Some(e)) => format!("end {:?}", e.kind()),
            })
            .collect()
    }

    #[test]
    fn the_events_come_out_whole_from_bytes_fed_one_at_a_time() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        input(&mut stream, 2, &key("a"));
        let mut session = Session::new();
        let mut events = Vec::new();
        for byte in &stream {
            session.feed(std::slice::from_ref(byte));
            events.extend(names(&mut session));
        }
        session.end();
        events.extend(names(&mut session));
        assert_eq!(events, ["start 2", "vsync", "2 key a", "end"]);
    }

    #[test]
    fn one_vsync_waits_at_most() {
        let mut stream = tick();
        input(&mut stream, 1, &key("a"));
        stream.extend_from_slice(&tick());
        let mut session = started(&stream);
        assert_eq!(names(&mut session), ["vsync", "1 key a"]);
        session.feed(&tick());
        assert_eq!(names(&mut session), ["vsync"]);
    }

    #[test]
    fn a_move_or_a_resize_replaces_one_of_its_player_past_the_input_of_another() {
        let mut stream = Vec::new();
        input(&mut stream, 1, &at(1.0));
        input(&mut stream, 2, &at(1.0));
        input(&mut stream, 2, &key("b"));
        input(&mut stream, 1, &at(2.0));
        input(&mut stream, 2, &at(2.0));
        input(&mut stream, 2, &resize(1.0));
        input(&mut stream, 1, &key("c"));
        input(&mut stream, 2, &resize(2.0));
        let mut session = started(&stream);
        assert_eq!(
            names(&mut session),
            [
                "1 move 2",
                "2 move 1",
                "2 key b",
                "2 move 2",
                "2 resize 2",
                "1 key c"
            ]
        );
    }

    #[test]
    fn a_move_stops_at_a_vsync_and_a_key_of_its_player() {
        let mut stream = Vec::new();
        input(&mut stream, 1, &at(1.0));
        stream.extend_from_slice(&tick());
        input(&mut stream, 1, &at(2.0));
        input(&mut stream, 1, &key("a"));
        input(&mut stream, 1, &at(3.0));
        let mut session = started(&stream);
        assert_eq!(
            names(&mut session),
            ["1 move 1", "vsync", "1 move 2", "1 key a", "1 move 3"]
        );
    }

    #[test]
    fn a_message_before_the_start_and_a_second_start_are_errors() {
        let mut stream = tick();
        stream.extend_from_slice(&start());
        stream.extend_from_slice(&start());
        stream.extend_from_slice(&tick());
        let mut session = Session::new();
        session.feed(&stream);
        assert!(matches!(
            session.next_event(),
            Some(SessionEvent::Error(SessionError::BeforeStart))
        ));
        assert!(matches!(session.next_event(), Some(SessionEvent::Start(_))));
        assert!(matches!(
            session.next_event(),
            Some(SessionEvent::Error(SessionError::SecondStart))
        ));
        assert!(matches!(session.next_event(), Some(SessionEvent::Vsync)));
    }

    #[test]
    fn a_lost_comes_out_after_the_start_and_is_an_error_before_it() {
        let mut lost = Vec::new();
        to_engine::write_lost(&mut lost, 7).unwrap();
        let mut stream = lost.clone();
        stream.extend_from_slice(&start());
        stream.extend_from_slice(&tick());
        stream.extend_from_slice(&lost);
        stream.extend_from_slice(&tick());
        let mut session = Session::new();
        session.feed(&stream);
        assert!(matches!(
            session.next_event(),
            Some(SessionEvent::Error(SessionError::BeforeStart))
        ));
        assert!(matches!(session.next_event(), Some(SessionEvent::Start(_))));
        assert!(matches!(session.next_event(), Some(SessionEvent::Vsync)));
        assert!(matches!(session.next_event(), Some(SessionEvent::Lost(7))));
        assert!(session.next_event().is_none());
    }

    #[test]
    fn the_end_before_the_start_is_the_end() {
        let mut session = Session::new();
        session.end();
        assert_eq!(names(&mut session), ["end"]);
    }

    #[test]
    fn a_message_that_does_not_decode_is_an_error_and_the_session_goes_on() {
        let mut stream = framed(&to_engine::encode_input(0, &key("a")));
        stream.extend_from_slice(&tick());
        let mut session = started(&stream);
        assert!(matches!(
            session.next_event(),
            Some(SessionEvent::Error(SessionError::Payload(
                wire::Error::NoPlayer
            )))
        ));
        assert!(matches!(session.next_event(), Some(SessionEvent::Vsync)));
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_skipped() {
        let unknown = wire::with_unknown_server_value(&tick()[HEADER_BYTES..], |m| wire::tag_of(m));
        let mut stream = framed(&unknown);
        stream.extend_from_slice(&tick());
        let mut session = started(&stream);
        assert_eq!(names(&mut session), ["vsync"]);
    }

    #[test]
    fn a_header_of_another_side_breaks_the_stream() {
        let mut stream = b"SIE1\0\0\0\0".to_vec();
        stream.extend_from_slice(&tick());
        let mut session = started(&stream);
        assert_eq!(names(&mut session), ["end InvalidData"]);
        session.feed(&tick());
        assert!(session.next_event().is_none());
    }

    #[test]
    fn the_end_inside_a_message_breaks_the_stream() {
        let stream = tick();
        let mut session = started(&stream[..stream.len() - 1]);
        session.end();
        assert_eq!(names(&mut session), ["end UnexpectedEof"]);
    }

    #[test]
    fn the_end_between_messages_is_the_end() {
        let mut session = started(&tick());
        session.end();
        assert_eq!(names(&mut session), ["vsync", "end"]);
    }

    #[test]
    fn nothing_comes_after_the_end() {
        let mut session = started(&[]);
        session.end();
        session.feed(&tick());
        assert_eq!(names(&mut session), ["end"]);
        session.end();
        assert!(session.next_event().is_none());
    }

    /// A reader that hands out one byte at a time. It fails at the byte of
    /// `fail_at` once, and is interrupted before every other byte.
    struct Trickle<'a> {
        bytes: &'a [u8],
        read: usize,
        fail_at: usize,
        interrupt: bool,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.read == self.fail_at {
                self.fail_at = usize::MAX;
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let Some((first, rest)) = self.bytes.split_first() else {
                return Ok(0);
            };
            buf[0] = *first;
            self.bytes = rest;
            self.read += 1;
            Ok(1)
        }
    }

    #[test]
    fn wait_reads_until_an_event_and_keeps_its_bytes_over_an_error() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        let mut r = Trickle {
            bytes: &stream,
            read: 0,
            fail_at: stream.len() - 4,
            interrupt: false,
        };
        let mut session = Session::new();
        assert!(matches!(session.wait(&mut r), Ok(SessionEvent::Start(_))));
        let e = session.wait(&mut r).expect_err("the error of the reader");
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(session.wait(&mut r), Ok(SessionEvent::Vsync)));
        assert!(matches!(session.wait(&mut r), Ok(SessionEvent::End(None))));
        assert!(matches!(session.wait(&mut r), Ok(SessionEvent::End(None))));
    }
}
