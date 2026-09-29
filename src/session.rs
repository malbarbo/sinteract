//! [`Session`] is the engine side of a session. The server, or the page
//! that plays the part of the server, writes a `ServerToEngine` stream to
//! the engine, and the session turns the bytes into [`SessionEvent`]s with
//! the rules of the protocol. [`Session::new`] writes the hello of the
//! engine, and the engine writes its frames back with
//! [`Session::write_frame`], which sends each image once, before the first
//! frame that draws it. The session writes a tickTaken there as it hands
//! out each tick, so the server holds the next tick until then.
//!
//! The session reads the server from the [`Read`] and writes to the
//! [`Write`] that the engine passes to [`Session::wait`], such as fd 3 and
//! fd 4. It opens no file or socket of its own, so it builds on wasm32.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use crate::asset::{RoomFull, check_room};
use crate::event::InputEvent;
use crate::scene::{Element, Image, Scene};
use crate::wire;
use crate::wire::engine_to_server::{self, PlayerRange};
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
pub struct Session {
    /// The bytes that do not make a whole message yet.
    bytes: Vec<u8>,
    /// The buffer of [`Session::wait`] for a read, zeroed once, since a
    /// read into `bytes` would zero its room at every read.
    scratch: Vec<u8>,
    state: State,
    events: VecDeque<SessionEvent>,
    /// The id of each image that went out and that the server did not lose.
    sent: HashMap<Image, u32>,
    next_id: u32,
}

/// What [`Session::wait`] delivers.
#[derive(Debug)]
pub enum SessionEvent {
    /// The nicknames of the players of the session, the first of player 1.
    /// The players are the same until the end. It comes before every other
    /// event of the server, and once.
    Start(Vec<String>),
    /// Time to draw the next frames, for every player.
    Tick,
    /// The input of `player`.
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
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

/// How much [`Session::wait`] asks of the reader at a time.
const READ_BYTES: usize = 64 * 1024;

impl Session {
    /// A session for a game of `players`. It writes the hello to `w` and
    /// flushes `w`, since the server sends nothing until it reads the
    /// hello. The hello is the first message of the engine, and the only
    /// one.
    pub fn new(players: PlayerRange, w: &mut impl Write) -> io::Result<Session> {
        engine_to_server::write_hello(w, players)?;
        w.flush()?;
        Ok(Session {
            bytes: Vec::new(),
            scratch: Vec::new(),
            state: State::BeforeStart,
            events: VecDeque::new(),
            sent: HashMap::new(),
            next_id: 0,
        })
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
    pub fn wait(&mut self, r: &mut impl Read, w: &mut impl Write) -> io::Result<SessionEvent> {
        loop {
            if let Some(event) = self.events.pop_front() {
                let taken = matches!(event, SessionEvent::Tick)
                    .then(|| engine_to_server::write_tick_taken(&mut *w).and_then(|()| w.flush()));
                if let Some(Err(e)) = taken {
                    self.state = State::Ended;
                    self.bytes = Vec::new();
                    self.events.clear();
                    return Err(e);
                }
                return Ok(event);
            }
            if self.state == State::Ended {
                return Ok(SessionEvent::End(None));
            }
            if self.scratch.is_empty() {
                self.scratch = vec![0; READ_BYTES];
            }
            match r.read(&mut self.scratch) {
                Ok(0) => {
                    let broken =
                        (!self.bytes.is_empty()).then(|| io::ErrorKind::UnexpectedEof.into());
                    self.finish(broken);
                }
                Ok(n) => {
                    let read = self.scratch.get(..n).expect("a read fits its buffer");
                    self.bytes.extend_from_slice(read);
                    self.take_messages();
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Write `scene` to `w` as a frame for `player`, or for every player
    /// when `player` is `None`. The asset of each image that it draws goes
    /// out before, unless it went out before and the server did not lose
    /// it.
    pub fn write_frame(
        &mut self,
        w: &mut impl Write,
        player: Option<NonZeroU32>,
        scene: &Scene,
    ) -> Result<(), FrameError> {
        let images = images_of(scene);
        check_room(images.iter().copied()).map_err(FrameError::Full)?;
        for image in images {
            if !self.sent.contains_key(image) {
                let id = self.next_id;
                self.next_id = id
                    .checked_add(1)
                    .expect("an engine sends fewer than 2^32 images");
                engine_to_server::write_asset(w, id, image.blob())?;
                self.sent.insert(image.clone(), id);
            }
        }
        let id = |image: &Image| {
            *self
                .sent
                .get(image)
                .expect("every image of the scene went out")
        };
        engine_to_server::write_frame(w, player, scene, &id)?;
        Ok(())
    }

    /// Turn every whole message in `bytes` into events.
    fn take_messages(&mut self) {
        // Out of `self`, so the loop reads it while the events go in.
        let mut bytes = std::mem::take(&mut self.bytes);
        let mut rest = bytes.as_slice();
        loop {
            let started = match self.state {
                State::BeforeStart => false,
                State::Started => true,
                State::Ended => return,
            };
            let (payload, after) = match framing::split_message(rest, Side::Server) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => return self.finish(Some(e)),
            };
            rest = after;
            match server_to_engine::decode(payload) {
                Ok(Some(message)) => self.receive(started, message),
                Ok(None) => {}
                Err(e) => self.push(SessionEvent::Error(SessionError::Payload(e))),
            }
        }
        let taken = bytes.len() - rest.len();
        bytes.drain(..taken);
        self.bytes = bytes;
    }

    /// Turn `message` into an event, by whether the start came before it.
    fn receive(&mut self, started: bool, message: Message) {
        let event = match (started, message) {
            (false, Message::Start(nicknames)) => {
                self.state = State::Started;
                SessionEvent::Start(nicknames)
            }
            (false, Message::Input { .. } | Message::Tick | Message::Lost(_)) => {
                SessionEvent::Error(SessionError::BeforeStart)
            }
            (true, Message::Start(_)) => SessionEvent::Error(SessionError::SecondStart),
            (true, Message::Tick) => SessionEvent::Tick,
            // The next frame that draws the image sends it again, with a new
            // id, as the schema says.
            (true, Message::Lost(id)) => {
                self.sent.retain(|_, sent| *sent != id);
                return;
            }
            (true, Message::Input { player, event }) => SessionEvent::Input { player, event },
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
            SessionEvent::Start(_) | SessionEvent::Error(_) | SessionEvent::End(_) => {}
        }
        self.events.push_back(event);
    }

    /// The waiting input of `player` that `new` replaces, looking from the
    /// back past the input of the other players. The events of one player
    /// keep their order, and nothing moves across a tick.
    fn superseded(&mut self, player: NonZeroU32, new: &InputEvent) -> Option<&mut SessionEvent> {
        for old in self.events.iter_mut().rev() {
            match old {
                SessionEvent::Input { player: p, event } if *p == player => {
                    return new.supersedes(event).then_some(old);
                }
                SessionEvent::Input { .. } => {}
                SessionEvent::Start(_)
                | SessionEvent::Tick
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

    /// A session for 1 or 2 players, with its hello thrown away.
    fn session() -> Session {
        Session::new(PlayerRange::new(1, 2).unwrap(), &mut io::sink()).unwrap()
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

    /// The next event, or `None` while the next one waits for more bytes,
    /// with the tickTaken dropped.
    fn next(session: &mut Session, pipe: &mut Pipe) -> Option<SessionEvent> {
        match session.wait(pipe, &mut io::sink()) {
            Ok(event) => Some(event),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("{e}"),
        }
    }

    /// A session after its start, with `bytes` in its pipe.
    fn started(bytes: &[u8]) -> (Session, Pipe) {
        let mut session = session();
        let mut pipe = Pipe::default();
        pipe.push(&start());
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Start(_))
        ));
        pipe.push(bytes);
        (session, pipe)
    }

    /// Every event that waits, up to the end, in a short form.
    fn names(session: &mut Session, pipe: &mut Pipe) -> Vec<String> {
        let mut ended = false;
        std::iter::from_fn(|| {
            if ended {
                return None;
            }
            let event = next(session, pipe)?;
            ended = matches!(event, SessionEvent::End(_));
            Some(event)
        })
        .map(|e| match e {
            SessionEvent::Start(nicknames) => format!("start {}", nicknames.len()),
            SessionEvent::Tick => "tick".into(),
            SessionEvent::Input { player, event } => match event {
                InputEvent::Key(k) => format!("{player} key {}", k.key),
                InputEvent::Mouse(m) => format!("{player} move {}", m.x),
                InputEvent::Resize { width, .. } => format!("{player} resize {width}"),
                InputEvent::Pad(_) => format!("{player} {event:?}"),
            },
            SessionEvent::Error(e) => format!("error {e}"),
            SessionEvent::End(None) => "end".into(),
            SessionEvent::End(Some(e)) => format!("end {:?}", e.kind()),
        })
        .collect()
    }

    #[test]
    fn a_new_session_writes_the_hello_and_flushes_it() {
        let players = PlayerRange::new(1, 2).unwrap();
        let mut w = io::BufWriter::new(Vec::new());
        Session::new(players, &mut w).unwrap();
        match testing::read(&mut &w.get_ref()[..]).unwrap() {
            Some(testing::Message::Hello(got)) => assert_eq!(got, players),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn the_events_come_out_whole_from_bytes_fed_one_at_a_time() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        input(&mut stream, 2, &key("a"));
        let mut session = session();
        let mut pipe = Pipe::default();
        let mut events = Vec::new();
        for byte in &stream {
            pipe.push(std::slice::from_ref(byte));
            events.extend(names(&mut session, &mut pipe));
        }
        pipe.close();
        events.extend(names(&mut session, &mut pipe));
        assert_eq!(events, ["start 2", "tick", "2 key a", "end"]);
    }

    #[test]
    fn one_tick_waits_at_most() {
        let mut stream = tick();
        input(&mut stream, 1, &key("a"));
        stream.extend_from_slice(&tick());
        let (mut session, mut pipe) = started(&stream);
        assert_eq!(names(&mut session, &mut pipe), ["tick", "1 key a"]);
        pipe.push(&tick());
        assert_eq!(names(&mut session, &mut pipe), ["tick"]);
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
        let (mut session, mut pipe) = started(&stream);
        assert_eq!(
            names(&mut session, &mut pipe),
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
        let (mut session, mut pipe) = started(&stream);
        assert_eq!(
            names(&mut session, &mut pipe),
            ["1 move 1", "tick", "1 move 2", "1 key a", "1 move 3"]
        );
    }

    #[test]
    fn a_message_before_the_start_and_a_second_start_are_errors() {
        let mut stream = tick();
        stream.extend_from_slice(&start());
        stream.extend_from_slice(&start());
        stream.extend_from_slice(&tick());
        let mut session = session();
        let mut pipe = Pipe::default();
        pipe.push(&stream);
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Error(SessionError::BeforeStart))
        ));
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Start(_))
        ));
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Error(SessionError::SecondStart))
        ));
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Tick)
        ));
    }

    #[test]
    fn a_lost_is_an_error_before_the_start() {
        let mut lost = Vec::new();
        server_to_engine::write_lost(&mut lost, 7).unwrap();
        let mut stream = lost.clone();
        stream.extend_from_slice(&start());
        stream.extend_from_slice(&tick());
        stream.extend_from_slice(&lost);
        stream.extend_from_slice(&tick());
        let mut session = session();
        let mut pipe = Pipe::default();
        pipe.push(&stream);
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Error(SessionError::BeforeStart))
        ));
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Start(_))
        ));
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Tick)
        ));
        assert!(next(&mut session, &mut pipe).is_none());
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
    fn written(session: &mut Session, scene: &Scene) -> Vec<String> {
        let mut out = Vec::new();
        session.write_frame(&mut out, None, scene).unwrap();
        let mut r = &out[..];
        std::iter::from_fn(|| testing::read(&mut r).unwrap())
            .map(|m| match m {
                testing::Message::Asset { id, .. } => format!("asset {id}"),
                testing::Message::Frame { .. } => "frame".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn an_image_goes_out_once_before_the_first_frame_that_draws_it() {
        let a = crate::asset::png_image(4, 4);
        let b = crate::asset::png_image(5, 5);
        let (mut session, _) = started(&[]);
        let again = crate::asset::png_image(4, 4);
        assert_eq!(
            written(&mut session, &drawing(&[a.clone(), b, again])),
            ["asset 0", "asset 1", "frame"]
        );
        assert_eq!(written(&mut session, &drawing(&[])), ["frame"]);
        assert_eq!(written(&mut session, &drawing(&[a])), ["frame"]);
    }

    #[test]
    fn a_lost_image_goes_out_again_with_a_new_id() {
        let a = crate::asset::png_image(4, 4);
        let b = crate::asset::png_image(5, 5);
        let (mut session, mut pipe) = started(&[]);
        written(&mut session, &drawing(&[a.clone(), b.clone()]));
        let mut lost = Vec::new();
        server_to_engine::write_lost(&mut lost, 0).unwrap();
        server_to_engine::write_lost(&mut lost, 99).unwrap();
        pipe.push(&lost);
        assert!(next(&mut session, &mut pipe).is_none());
        assert_eq!(
            written(&mut session, &drawing(&[a, b])),
            ["asset 2", "frame"]
        );
    }

    #[test]
    fn a_frame_whose_images_go_over_the_limits_of_a_room_writes_nothing() {
        let images: Vec<Image> = (0..9)
            .map(|k| crate::asset::png_image(2048, 2048 - k))
            .collect();
        let (mut session, _) = started(&[]);
        let mut out = Vec::new();
        assert!(matches!(
            session.write_frame(&mut out, None, &drawing(&images)),
            Err(FrameError::Full(RoomFull { .. }))
        ));
        assert!(out.is_empty());
        assert_eq!(written(&mut session, &drawing(&images[..8])).len(), 9);
    }

    #[test]
    fn the_end_before_the_start_is_the_end() {
        let mut session = session();
        let mut pipe = Pipe::default();
        pipe.close();
        assert_eq!(names(&mut session, &mut pipe), ["end"]);
    }

    #[test]
    fn a_message_that_does_not_decode_is_an_error_and_the_session_goes_on() {
        let mut stream = framed(&testing::encode_input(0, &key("a")));
        stream.extend_from_slice(&tick());
        let (mut session, mut pipe) = started(&stream);
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Error(SessionError::Payload(
                wire::Error::NoPlayer
            )))
        ));
        assert!(matches!(
            next(&mut session, &mut pipe),
            Some(SessionEvent::Tick)
        ));
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_skipped() {
        let unknown =
            testing::with_unknown_server_value(&tick()[HEADER_BYTES..], |m| testing::tag_of(m));
        let mut stream = framed(&unknown);
        stream.extend_from_slice(&tick());
        let (mut session, mut pipe) = started(&stream);
        assert_eq!(names(&mut session, &mut pipe), ["tick"]);
    }

    #[test]
    fn a_header_of_another_side_breaks_the_stream() {
        let mut stream = b"SIE1\0\0\0\0".to_vec();
        stream.extend_from_slice(&tick());
        let (mut session, mut pipe) = started(&stream);
        assert_eq!(names(&mut session, &mut pipe), ["end InvalidData"]);
        pipe.push(&tick());
        assert_eq!(names(&mut session, &mut pipe), ["end"]);
    }

    #[test]
    fn the_end_inside_a_message_breaks_the_stream() {
        let stream = tick();
        let (mut session, mut pipe) = started(&stream[..stream.len() - 1]);
        pipe.close();
        assert_eq!(names(&mut session, &mut pipe), ["end UnexpectedEof"]);
    }

    #[test]
    fn the_end_between_messages_is_the_end() {
        let (mut session, mut pipe) = started(&tick());
        pipe.close();
        assert_eq!(names(&mut session, &mut pipe), ["tick", "end"]);
    }

    #[test]
    fn nothing_comes_after_the_end() {
        let (mut session, mut pipe) = started(&[]);
        pipe.close();
        assert_eq!(names(&mut session, &mut pipe), ["end"]);
        pipe.push(&tick());
        assert_eq!(names(&mut session, &mut pipe), ["end"]);
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
        let mut session = session();
        assert!(matches!(
            session.wait(&mut r, &mut io::sink()),
            Ok(SessionEvent::Start(_))
        ));
        let e = session
            .wait(&mut r, &mut io::sink())
            .expect_err("the error of the reader");
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(
            session.wait(&mut r, &mut io::sink()),
            Ok(SessionEvent::Tick)
        ));
        assert!(matches!(
            session.wait(&mut r, &mut io::sink()),
            Ok(SessionEvent::End(None))
        ));
        assert!(matches!(
            session.wait(&mut r, &mut io::sink()),
            Ok(SessionEvent::End(None))
        ));
    }

    #[test]
    fn a_tick_writes_a_tick_taken_and_a_start_does_not() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        let mut session = session();
        let mut pipe = Pipe::default();
        pipe.push(&stream);
        let mut out = Vec::new();
        assert!(matches!(
            session.wait(&mut pipe, &mut out),
            Ok(SessionEvent::Start(_))
        ));
        assert!(out.is_empty());
        assert!(matches!(
            session.wait(&mut pipe, &mut out),
            Ok(SessionEvent::Tick)
        ));
        assert!(matches!(
            testing::read(&mut &out[..]),
            Ok(Some(testing::Message::TickTaken))
        ));
    }

    #[test]
    fn wait_flushes_the_tick_taken_through_a_buffered_writer() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        let mut r = &stream[..];
        let mut w = io::BufWriter::new(Vec::new());
        let mut session = session();
        assert!(matches!(
            session.wait(&mut r, &mut w),
            Ok(SessionEvent::Start(_))
        ));
        assert!(w.get_ref().is_empty());
        assert!(matches!(
            session.wait(&mut r, &mut w),
            Ok(SessionEvent::Tick)
        ));
        assert!(matches!(
            testing::read(&mut &w.get_ref()[..]),
            Ok(Some(testing::Message::TickTaken))
        ));
    }

    /// A writer that fails every write.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_error_of_the_writer_at_a_tick_ends_the_session() {
        let mut stream = start();
        stream.extend_from_slice(&tick());
        stream.extend_from_slice(&tick());
        let mut r = &stream[..];
        let mut session = session();
        assert!(matches!(
            session.wait(&mut r, &mut Broken),
            Ok(SessionEvent::Start(_))
        ));
        let e = session
            .wait(&mut r, &mut Broken)
            .expect_err("the error of the writer");
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(
            session.wait(&mut r, &mut io::sink()),
            Ok(SessionEvent::End(None))
        ));
    }
}
