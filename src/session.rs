//! [`Session`] is the engine side of a session. The server, or the page
//! that plays the part of the server, writes a `ServerToEngine` stream to
//! the engine, and the session turns the bytes into [`SessionEvent`]s with
//! the rules of the protocol. [`Session::start`] writes the hello of the
//! engine and waits for the start of the server, so a session exists only
//! after its start. The engine writes its frames back with
//! [`Session::write_frame`], which sends each image once, before the first
//! frame that draws it. The session writes a tickTaken there as it hands
//! out each tick, so the server holds the next tick until then.
//!
//! The session reads the server from the [`Read`] and writes to the
//! [`Write`] that the engine passes to [`Session::start`], such as fd 3 and
//! fd 4. It opens no file or socket of its own, so it builds on wasm32.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use crate::asset::{RoomFull, check_room};
use crate::event::InputEvent;
use crate::scene::{Element, Image, Scene};
use crate::wire;
use crate::wire::engine_to_server;
use crate::wire::framing::{self, Side};
use crate::wire::server_to_engine::{self, Message};

/// The engine side of a session, from the bytes of the server to the
/// events of the engine.
///
/// The queue keeps one tick, so an engine that falls behind the server
/// gets one tick and not a burst. A move of the mouse or a resize replaces
/// one of the same player that still waits, since only the latest one
/// counts. It passes the input of the other players, and stops at anything
/// else.
#[derive(Debug)]
pub struct Session<R, W> {
    /// The stream of the server.
    r: R,
    /// The stream to the server. Every message of the engine goes out
    /// through the session, so nothing else writes between two of them.
    w: W,
    /// The bytes that do not make a whole message yet.
    bytes: Vec<u8>,
    /// The buffer of [`Session::wait`] for a read, zeroed once, since a
    /// read into `bytes` would zero its room at every read.
    scratch: Vec<u8>,
    /// The number of players, from the start.
    players: u32,
    /// The session ended, and reads nothing more.
    ended: bool,
    events: VecDeque<SessionEvent>,
    /// The id of each image that went out and that the server did not lose.
    sent: HashMap<Image, u32>,
    next_id: u32,
}

/// A player of the session. Only the session makes one, from the start, so
/// a player that the engine holds is in the game. On a display of this
/// process, the `Stage` of the displays makes player 1, the one player
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Player(NonZeroU32);

impl Player {
    /// The one player of a game on a display of this process.
    #[cfg_attr(
        not(all(feature = "terminal", feature = "window", not(target_arch = "wasm32"))),
        allow(dead_code)
    )]
    pub(crate) const LOCAL: Player = Player(NonZeroU32::MIN);

    /// The place of the player in the start, from 1.
    pub fn number(self) -> NonZeroU32 {
        self.0
    }
}

/// Who a frame goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    All,
    Player(Player),
}

/// The most players of a room. The start that names them stays far under
/// the cap of the framing, since a nickname has 64 bytes at most.
pub const MAX_PLAYERS: u32 = 1024;

/// The fewest and the most players that a game takes, from 1 to
/// [`MAX_PLAYERS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerRange {
    min: NonZeroU32,
    max: NonZeroU32,
}

impl PlayerRange {
    /// The range from `min` to `max`, or `None` if `min` is 0 or above
    /// `max`, or `max` is above [`MAX_PLAYERS`].
    pub const fn new(min: u32, max: u32) -> Option<PlayerRange> {
        match (NonZeroU32::new(min), NonZeroU32::new(max)) {
            (Some(min), Some(max)) if min.get() <= max.get() && max.get() <= MAX_PLAYERS => {
                Some(PlayerRange { min, max })
            }
            _ => None,
        }
    }

    pub fn min(self) -> NonZeroU32 {
        self.min
    }

    pub fn max(self) -> NonZeroU32 {
        self.max
    }

    /// Returns `true` if the game takes `players` players, `false`
    /// otherwise.
    pub fn contains(self, players: usize) -> bool {
        (self.min.get() as usize..=self.max.get() as usize).contains(&players)
    }
}

/// What [`Session::wait`] delivers.
#[derive(Debug)]
pub enum SessionEvent {
    /// Time to draw the next frames, for every player.
    Tick,
    /// The input of `player`.
    Input { player: Player, event: InputEvent },
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
    /// A start came after the first one.
    SecondStart,
    /// An input came for a player that is not in the start.
    NoPlayer(NonZeroU32),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Payload(e) => write!(f, "message does not decode: {e}"),
            SessionError::SecondStart => write!(f, "a start came after the first one"),
            SessionError::NoPlayer(player) => {
                write!(
                    f,
                    "an input came for player {player}, who is not in the start"
                )
            }
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SessionError::Payload(e) => Some(e),
            SessionError::SecondStart | SessionError::NoPlayer(_) => None,
        }
    }
}

/// Why [`Session::start`] did not start a session.
#[derive(Debug)]
pub enum StartError {
    /// A read or a write failed.
    Io(io::Error),
    /// The stream ended before the start. The error is `Some` when the
    /// stream broke, with a header that is not one of the server or with
    /// the end inside a message.
    End(Option<io::Error>),
    /// The first message does not decode.
    Payload(wire::Error),
    /// The first message is not a start.
    NoStart,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Io(e) => write!(f, "cannot start the session: {e}"),
            StartError::End(None) => write!(f, "the server ended before the start"),
            StartError::End(Some(e)) => write!(f, "the stream broke before the start: {e}"),
            StartError::Payload(e) => write!(f, "the first message does not decode: {e}"),
            StartError::NoStart => write!(f, "the first message is not a start"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartError::Io(e) | StartError::End(Some(e)) => Some(e),
            StartError::Payload(e) => Some(e),
            StartError::End(None) | StartError::NoStart => None,
        }
    }
}

impl From<io::Error> for StartError {
    fn from(e: io::Error) -> Self {
        StartError::Io(e)
    }
}

/// Why [`Session::write_frame`] did not write a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The images of the frame go over the limits of a room together, so
    /// the server would lose one of them each frame. Nothing went out.
    Full(RoomFull),
    /// A write failed. Part of the frame may have gone out.
    Io(io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Full(e) => e.fmt(f),
            FrameError::Io(e) => write!(f, "cannot write the frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Full(e) => Some(e),
            FrameError::Io(e) => Some(e),
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// How much a session asks of the reader at a time.
const READ_BYTES: usize = 64 * 1024;

impl<R: Read, W: Write> Session<R, W> {
    /// The session of a game of `players`, which reads the server from `r`
    /// and writes to it on `w`, with each player of the start and its
    /// nickname. It writes the hello to `w`, flushes `w`, since
    /// the server sends nothing until it reads the hello, and blocks until
    /// the start. The hello is the first message of the engine, and the
    /// only one. The players are the same until the end. A message before
    /// the start breaks the rules of the server, and the session does not
    /// start. An error of `r` ends the wait as well, since the hello went
    /// out and cannot go out again.
    pub fn start(
        players: PlayerRange,
        r: R,
        mut w: W,
    ) -> Result<(Self, Vec<(Player, String)>), StartError> {
        engine_to_server::write_hello(&mut w, players)?;
        w.flush()?;
        let mut session = Session {
            r,
            w,
            bytes: Vec::new(),
            scratch: Vec::new(),
            players: 0,
            ended: false,
            events: VecDeque::new(),
            sent: HashMap::new(),
            next_id: 0,
        };
        let nicknames = session.read_start()?;
        session.players = u32::try_from(nicknames.len()).expect("a start has at most 1024 players");
        let players = (1..)
            .map_while(NonZeroU32::new)
            .map(Player)
            .zip(nicknames)
            .collect();
        // The bytes after the start in the same read.
        session.take_messages();
        Ok((session, players))
    }

    /// The next event, with bytes from `r` when none waits. It blocks for
    /// as long as `r` blocks. The end of `r` ends the session, and the part
    /// of a message that is left breaks the stream. After the end, it
    /// returns `End(None)` without a read. An error of `r` other than
    /// [`io::ErrorKind::Interrupted`] comes back as is, and the session
    /// keeps the bytes that it read before. A tick writes a tickTaken to `w`
    /// and flushes `w` first, since the server sends no other tick until it
    /// reads the tickTaken, and a tick may draw no frame that would flush
    /// it. An error of `w` comes back in place of the tick and ends the
    /// session, since `w` may hold part of the tickTaken.
    pub fn wait(&mut self) -> io::Result<SessionEvent> {
        loop {
            if let Some(event) = self.events.pop_front() {
                let taken = matches!(event, SessionEvent::Tick).then(|| {
                    engine_to_server::write_tick_taken(&mut self.w).and_then(|()| self.w.flush())
                });
                if let Some(Err(e)) = taken {
                    self.ended = true;
                    self.bytes = Vec::new();
                    self.events.clear();
                    return Err(e);
                }
                return Ok(event);
            }
            if self.ended {
                return Ok(SessionEvent::End(None));
            }
            if self.read_more()? {
                self.take_messages();
            } else {
                let broken = self.end_of_stream();
                self.finish(broken);
            }
        }
    }

    /// Write `scene` to `w` as a frame for `to`. The asset of each image that it draws goes
    /// out before, unless it went out before and the server did not lose
    /// it.
    pub fn write_frame(&mut self, to: Target, scene: &Scene) -> Result<(), FrameError> {
        let images = images_of(scene);
        check_room(images.iter().copied()).map_err(FrameError::Full)?;
        for image in images {
            if !self.sent.contains_key(image) {
                let id = self.next_id;
                self.next_id = id
                    .checked_add(1)
                    .expect("an engine sends fewer than 2^32 images");
                engine_to_server::write_asset(&mut self.w, id, image.blob())?;
                self.sent.insert(image.clone(), id);
            }
        }
        let sent = &self.sent;
        let id = |image: &Image| *sent.get(image).expect("every image of the scene went out");
        let player = match to {
            Target::All => None,
            Target::Player(player) => Some(player.number()),
        };
        engine_to_server::write_frame(&mut self.w, player, scene, &id)?;
        Ok(())
    }

    /// Read up to the start, the first message that the session knows, and
    /// keep the bytes after it.
    fn read_start(&mut self) -> Result<Vec<String>, StartError> {
        loop {
            match framing::split_message(&self.bytes, Side::Server) {
                Ok(Some((payload, after))) => {
                    let message = server_to_engine::decode(payload);
                    let taken = self.bytes.len() - after.len();
                    self.bytes.drain(..taken);
                    match message {
                        Ok(Some(Message::Start(nicknames))) => return Ok(nicknames),
                        Ok(Some(Message::Tick | Message::Input { .. } | Message::Lost(_))) => {
                            return Err(StartError::NoStart);
                        }
                        Ok(None) => {}
                        Err(e) => return Err(StartError::Payload(e)),
                    }
                }
                Ok(None) => {
                    if !self.read_more()? {
                        return Err(StartError::End(self.end_of_stream()));
                    }
                }
                Err(e) => return Err(StartError::End(Some(e))),
            }
        }
    }

    /// Read the next bytes of `r` into `bytes`. Returns `true` if it read
    /// some, `false` at the end of `r`.
    fn read_more(&mut self) -> io::Result<bool> {
        if self.scratch.is_empty() {
            self.scratch = vec![0; READ_BYTES];
        }
        loop {
            match self.r.read(&mut self.scratch) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    let read = self.scratch.get(..n).expect("a read fits its buffer");
                    self.bytes.extend_from_slice(read);
                    return Ok(true);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The error of the end of `r`, which breaks the stream inside a
    /// message.
    fn end_of_stream(&self) -> Option<io::Error> {
        (!self.bytes.is_empty()).then(|| io::ErrorKind::UnexpectedEof.into())
    }

    /// Turn every whole message in `bytes` into events.
    fn take_messages(&mut self) {
        // Out of `self`, so the loop reads it while the events go in.
        let mut bytes = std::mem::take(&mut self.bytes);
        let mut rest = bytes.as_slice();
        loop {
            if self.ended {
                return;
            }
            let (payload, after) = match framing::split_message(rest, Side::Server) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => return self.finish(Some(e)),
            };
            rest = after;
            match server_to_engine::decode(payload) {
                Ok(Some(message)) => self.receive(message),
                Ok(None) => {}
                Err(e) => self.push(SessionEvent::Error(SessionError::Payload(e))),
            }
        }
        let taken = bytes.len() - rest.len();
        bytes.drain(..taken);
        self.bytes = bytes;
    }

    /// Turn `message` into an event.
    fn receive(&mut self, message: Message) {
        let event = match message {
            Message::Start(_) => SessionEvent::Error(SessionError::SecondStart),
            Message::Tick => SessionEvent::Tick,
            // The next frame that draws the image sends it again, with a new
            // id, as the schema says.
            Message::Lost(id) => {
                self.sent.retain(|_, sent| *sent != id);
                return;
            }
            Message::Input { player, event } if player.get() <= self.players => {
                SessionEvent::Input {
                    player: Player(player),
                    event,
                }
            }
            Message::Input { player, .. } => SessionEvent::Error(SessionError::NoPlayer(player)),
        };
        self.push(event);
    }

    /// Queue `event`, dropping a second tick and a move or a resize that
    /// `event` replaces.
    fn push(&mut self, event: SessionEvent) {
        match &event {
            SessionEvent::Tick => {
                if self.events.iter().any(|e| matches!(e, SessionEvent::Tick)) {
                    return;
                }
            }
            SessionEvent::Input { player, event: new } => {
                if let Some(old) = self.superseded(*player, new) {
                    *old = event;
                    return;
                }
            }
            SessionEvent::Error(_) | SessionEvent::End(_) => {}
        }
        self.events.push_back(event);
    }

    /// The waiting input of `player` that `new` replaces, looking from the
    /// back past the input of the other players. The events of one player
    /// keep their order, and nothing moves across a tick.
    fn superseded(&mut self, player: Player, new: &InputEvent) -> Option<&mut SessionEvent> {
        for old in self.events.iter_mut().rev() {
            match old {
                SessionEvent::Input { player: p, event } if *p == player => {
                    return new.supersedes(event).then_some(old);
                }
                SessionEvent::Input { .. } => {}
                SessionEvent::Tick | SessionEvent::Error(_) | SessionEvent::End(_) => return None,
            }
        }
        None
    }

    /// End the session, with the error of a broken stream. Nothing comes
    /// after the `End`.
    fn finish(&mut self, broken: Option<io::Error>) {
        self.ended = true;
        self.bytes = Vec::new();
        self.events.push_back(SessionEvent::End(broken));
    }
}

/// The images that `scene` draws, each once, in the order of their first
/// bitmap.
fn images_of(scene: &Scene) -> Vec<&Image> {
    fn walk<'a>(elements: &'a [Element], seen: &mut HashSet<&'a Image>, out: &mut Vec<&'a Image>) {
        for element in elements {
            match element {
                Element::Bitmap(b) => {
                    if seen.insert(&b.image) {
                        out.push(&b.image);
                    }
                }
                Element::Clipped { elements, .. } | Element::Layer { elements, .. } => {
                    walk(elements, seen, out)
                }
                Element::Path(_) | Element::Text(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(scene.elements(), &mut HashSet::new(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyKind, Modifiers, MouseAction, MouseButtons, MouseEvent};
    use crate::scene::Bitmap;
    use crate::wire::framing::HEADER_BYTES;
    use crate::wire::testing::{self, Pipe};

    fn player(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
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

    type TestSession = Session<Pipe, Pipe>;

    fn players() -> PlayerRange {
        PlayerRange::new(1, 2).unwrap()
    }

    /// A start with Ana as player 1 and Beto as player 2.
    fn start() -> Vec<u8> {
        let mut out = Vec::new();
        server_to_engine::write_start(&mut out, &["Ana", "Beto"]).unwrap();
        out
    }

    fn tick() -> Vec<u8> {
        let mut out = Vec::new();
        server_to_engine::write_tick(&mut out).unwrap();
        out
    }

    fn input(out: &mut Vec<u8>, p: u32, ev: &InputEvent) {
        server_to_engine::write_input(out, player(p), ev).unwrap();
    }

    /// `payload` with the envelope of the server.
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = framing::header(Side::Server, payload.len() as u32).to_vec();
        out.extend_from_slice(payload);
        out
    }

    /// The next event, or `None` while the next one waits for more bytes.
    fn next<R: Read, W: Write>(session: &mut Session<R, W>) -> Option<SessionEvent> {
        match session.wait() {
            Ok(event) => Some(event),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("{e}"),
        }
    }

    /// A session after its start, with `bytes` after the start in the same
    /// read, the pipe from the server, and the pipe to the server, which the
    /// hello already left.
    fn started(bytes: &[u8]) -> (TestSession, Pipe, Pipe) {
        let (server, engine) = (Pipe::default(), Pipe::default());
        server.push(&start());
        server.push(bytes);
        let (session, players) =
            Session::start(players(), server.clone(), engine.clone()).expect("the session starts");
        assert_eq!(members(&players), ["1 Ana", "2 Beto"]);
        engine.drain();
        (session, server, engine)
    }

    /// Each of `players`, in a short form.
    fn members(players: &[(Player, String)]) -> Vec<String> {
        players
            .iter()
            .map(|(player, nickname)| format!("{} {nickname}", player.number()))
            .collect()
    }

    /// Why a session does not start when the server sends `stream` and
    /// ends.
    fn no_start(stream: &[u8]) -> StartError {
        let server = Pipe::default();
        server.push(stream);
        server.close();
        match Session::start(players(), server, io::sink()) {
            Ok(_) => panic!("the session started"),
            Err(e) => e,
        }
    }

    /// Every event that waits, up to the end, in a short form.
    fn names<R: Read, W: Write>(session: &mut Session<R, W>) -> Vec<String> {
        let mut ended = false;
        std::iter::from_fn(|| {
            if ended {
                return None;
            }
            let event = next(session)?;
            ended = matches!(event, SessionEvent::End(_));
            Some(event)
        })
        .map(|e| match e {
            SessionEvent::Tick => "tick".into(),
            SessionEvent::Input { player, event } => match (player.number(), event) {
                (player, InputEvent::Key(k)) => format!("{player} key {}", k.key),
                (player, InputEvent::Mouse(m)) => format!("{player} move {}", m.x),
                (player, InputEvent::Resize { width, .. }) => format!("{player} resize {width}"),
                (player, event @ InputEvent::Pad(_)) => format!("{player} {event:?}"),
            },
            SessionEvent::Error(e) => format!("error {e}"),
            SessionEvent::End(None) => "end".into(),
            SessionEvent::End(Some(e)) => format!("end {:?}", e.kind()),
        })
        .collect()
    }

    #[test]
    fn the_start_writes_the_hello_and_flushes_it() {
        let engine = Pipe::default();
        Session::start(players(), &start()[..], io::BufWriter::new(engine.clone())).unwrap();
        let hello = engine.drain();
        let mut r = &hello[..];
        match testing::read(&mut r).unwrap() {
            Some(testing::Message::Hello(got)) => assert_eq!(got, players()),
            other => panic!("got {other:?}"),
        }
        assert!(r.is_empty());
    }

    #[test]
    fn the_events_come_out_whole_from_bytes_fed_one_at_a_time() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        input(&mut stream, 2, &key("a"));
        let r = Trickle {
            bytes: &stream,
            read: 0,
            fail_at: usize::MAX,
            interrupt: false,
        };
        let (mut session, players) = Session::start(players(), r, io::sink()).unwrap();
        assert_eq!(members(&players), ["1 Ana", "2 Beto"]);
        assert_eq!(names(&mut session), ["tick", "2 key a", "end"]);
    }

    #[test]
    fn one_tick_waits_at_most() {
        let mut stream = tick();
        input(&mut stream, 1, &key("a"));
        stream.extend_from_slice(&tick());
        let (mut session, server, _) = started(&stream);
        assert_eq!(names(&mut session), ["tick", "1 key a"]);
        server.push(&tick());
        assert_eq!(names(&mut session), ["tick"]);
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
        let (mut session, _, _) = started(&stream);
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
    fn a_move_stops_at_a_tick_and_a_key_of_its_player() {
        let mut stream = Vec::new();
        input(&mut stream, 1, &at(1.0));
        stream.extend_from_slice(&tick());
        input(&mut stream, 1, &at(2.0));
        input(&mut stream, 1, &key("a"));
        input(&mut stream, 1, &at(3.0));
        let (mut session, _, _) = started(&stream);
        assert_eq!(
            names(&mut session),
            ["1 move 1", "tick", "1 move 2", "1 key a", "1 move 3"]
        );
    }

    #[test]
    fn a_message_before_the_start_stops_the_start() {
        let mut lost = Vec::new();
        server_to_engine::write_lost(&mut lost, 7).unwrap();
        for first in [tick(), lost] {
            let mut stream = first;
            stream.extend_from_slice(&start());
            assert!(matches!(no_start(&stream), StartError::NoStart));
        }
    }

    #[test]
    fn a_start_that_does_not_come_whole_stops_the_start() {
        let start = start();
        assert!(matches!(no_start(&[]), StartError::End(None)));
        assert!(matches!(
            no_start(&start[..start.len() - 1]),
            StartError::End(Some(e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            no_start(b"SIE1\0\0\0\0"),
            StartError::End(Some(e)) if e.kind() == io::ErrorKind::InvalidData
        ));
        assert!(matches!(
            no_start(&framed(&testing::encode_input(0, &key("a")))),
            StartError::Payload(wire::Error::NoPlayer)
        ));
    }

    #[test]
    fn a_message_of_an_unknown_arm_before_the_start_is_skipped() {
        let unknown =
            testing::with_unknown_server_value(&tick()[HEADER_BYTES..], |m| testing::tag_of(m));
        let mut stream = framed(&unknown);
        stream.extend_from_slice(&start());
        let server = Pipe::default();
        server.push(&stream);
        let (_, players) = Session::start(players(), server, io::sink()).unwrap();
        assert_eq!(members(&players), ["1 Ana", "2 Beto"]);
    }

    #[test]
    fn an_input_for_a_player_who_is_not_in_the_start_is_an_error() {
        let mut stream = Vec::new();
        input(&mut stream, 3, &key("a"));
        input(&mut stream, 2, &key("b"));
        let (mut session, _, _) = started(&stream);
        assert_eq!(
            names(&mut session),
            [
                "error an input came for player 3, who is not in the start",
                "2 key b"
            ]
        );
    }

    #[test]
    fn a_second_start_is_an_error_and_the_session_goes_on() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        let (mut session, _, _) = started(&stream);
        assert_eq!(
            names(&mut session),
            ["error a start came after the first one", "tick"]
        );
    }

    /// A scene that draws the images of `images`, the last one inside a
    /// layer.
    fn drawing(images: &[Image]) -> Scene {
        let rect = crate::scene::RotatedRect {
            cx: 1.0,
            cy: 1.0,
            w: 2.0,
            h: 2.0,
            angle_deg: 0.0,
        };
        let mut scene = Scene::new(4.0, 4.0);
        let Some((last, rest)) = images.split_last() else {
            return scene;
        };
        for image in rest {
            scene.add_bitmap(Bitmap::fit(image.clone(), rect));
        }
        scene.layer(0.5, |layer| {
            layer.add_bitmap(Bitmap::fit(last.clone(), rect))
        });
        scene
    }

    /// The messages that `session` writes for `scene`, in a short form.
    fn written(session: &mut TestSession, engine: &Pipe, scene: &Scene) -> Vec<String> {
        session.write_frame(Target::All, scene).unwrap();
        let out = engine.drain();
        let mut r = &out[..];
        std::iter::from_fn(|| testing::read(&mut r).unwrap())
            .map(|m| match m {
                testing::Message::Asset { id, .. } => format!("asset {id}"),
                testing::Message::Frame { player: None, .. } => "frame".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_frame_for_a_player_goes_out_with_the_player() {
        let (server, engine) = (Pipe::default(), Pipe::default());
        server.push(&start());
        let (mut session, players) = Session::start(players(), server, engine.clone()).unwrap();
        engine.drain();
        let (beto, _) = players[1];
        session
            .write_frame(Target::Player(beto), &drawing(&[]))
            .unwrap();
        let out = engine.drain();
        assert!(matches!(
            testing::read(&mut &out[..]).unwrap(),
            Some(testing::Message::Frame { player: Some(p), .. }) if p.get() == 2
        ));
    }

    #[test]
    fn an_image_goes_out_once_before_the_first_frame_that_draws_it() {
        let a = crate::asset::png_image(4, 4);
        let b = crate::asset::png_image(5, 5);
        let (mut session, _, engine) = started(&[]);
        let again = crate::asset::png_image(4, 4);
        assert_eq!(
            written(&mut session, &engine, &drawing(&[a.clone(), b, again])),
            ["asset 0", "asset 1", "frame"]
        );
        assert_eq!(written(&mut session, &engine, &drawing(&[])), ["frame"]);
        assert_eq!(written(&mut session, &engine, &drawing(&[a])), ["frame"]);
    }

    #[test]
    fn a_lost_image_goes_out_again_with_a_new_id() {
        let a = crate::asset::png_image(4, 4);
        let b = crate::asset::png_image(5, 5);
        let (mut session, server, engine) = started(&[]);
        written(&mut session, &engine, &drawing(&[a.clone(), b.clone()]));
        let mut lost = Vec::new();
        server_to_engine::write_lost(&mut lost, 0).unwrap();
        server_to_engine::write_lost(&mut lost, 99).unwrap();
        server.push(&lost);
        assert!(next(&mut session).is_none());
        assert_eq!(
            written(&mut session, &engine, &drawing(&[a, b])),
            ["asset 2", "frame"]
        );
    }

    #[test]
    fn a_frame_whose_images_go_over_the_limits_of_a_room_writes_nothing() {
        let images: Vec<Image> = (0..9)
            .map(|k| crate::asset::png_image(2048, 2048 - k))
            .collect();
        let (mut session, _, engine) = started(&[]);
        assert!(matches!(
            session.write_frame(Target::All, &drawing(&images)),
            Err(FrameError::Full(RoomFull { .. }))
        ));
        assert!(engine.drain().is_empty());
        assert_eq!(
            written(&mut session, &engine, &drawing(&images[..8])).len(),
            9
        );
    }

    #[test]
    fn a_message_that_does_not_decode_is_an_error_and_the_session_goes_on() {
        let mut stream = framed(&testing::encode_input(0, &key("a")));
        stream.extend_from_slice(&tick());
        let (mut session, _, _) = started(&stream);
        assert!(matches!(
            next(&mut session),
            Some(SessionEvent::Error(SessionError::Payload(
                wire::Error::NoPlayer
            )))
        ));
        assert!(matches!(next(&mut session), Some(SessionEvent::Tick)));
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_skipped() {
        let unknown =
            testing::with_unknown_server_value(&tick()[HEADER_BYTES..], |m| testing::tag_of(m));
        let mut stream = framed(&unknown);
        stream.extend_from_slice(&tick());
        let (mut session, _, _) = started(&stream);
        assert_eq!(names(&mut session), ["tick"]);
    }

    #[test]
    fn a_header_of_another_side_breaks_the_stream() {
        let mut stream = b"SIE1\0\0\0\0".to_vec();
        stream.extend_from_slice(&tick());
        let (mut session, server, _) = started(&stream);
        assert_eq!(names(&mut session), ["end InvalidData"]);
        server.push(&tick());
        assert_eq!(names(&mut session), ["end"]);
    }

    #[test]
    fn the_end_inside_a_message_breaks_the_stream() {
        let stream = tick();
        let (mut session, server, _) = started(&stream[..stream.len() - 1]);
        server.close();
        assert_eq!(names(&mut session), ["end UnexpectedEof"]);
    }

    #[test]
    fn the_end_between_messages_is_the_end() {
        let (mut session, server, _) = started(&tick());
        server.close();
        assert_eq!(names(&mut session), ["tick", "end"]);
    }

    #[test]
    fn nothing_comes_after_the_end() {
        let (mut session, server, _) = started(&[]);
        server.close();
        assert_eq!(names(&mut session), ["end"]);
        server.push(&tick());
        assert_eq!(names(&mut session), ["end"]);
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
        let r = Trickle {
            bytes: &stream,
            read: 0,
            fail_at: stream.len() - 4,
            interrupt: false,
        };
        let (mut session, _) = Session::start(players(), r, io::sink()).unwrap();
        let e = session.wait().expect_err("the error of the reader");
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(session.wait(), Ok(SessionEvent::Tick)));
        assert!(matches!(session.wait(), Ok(SessionEvent::End(None))));
        assert!(matches!(session.wait(), Ok(SessionEvent::End(None))));
    }

    #[test]
    fn wait_flushes_the_tick_taken_through_a_buffered_writer() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        let engine = Pipe::default();
        let (mut session, _) =
            Session::start(players(), &stream[..], io::BufWriter::new(engine.clone())).unwrap();
        engine.drain();
        assert!(matches!(session.wait(), Ok(SessionEvent::Tick)));
        assert!(matches!(
            testing::read(&mut &engine.drain()[..]),
            Ok(Some(testing::Message::TickTaken))
        ));
    }

    #[test]
    fn an_error_of_the_writer_at_a_tick_ends_the_session() {
        let mut stream = tick();
        stream.extend_from_slice(&tick());
        let (mut session, _, engine) = started(&stream);
        engine.break_writes();
        let e = session.wait().expect_err("the error of the writer");
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(session.wait(), Ok(SessionEvent::End(None))));
    }
}
