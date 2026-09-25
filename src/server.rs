//! [`ServerCore`] holds the rules of a room, with no I/O and no clock. The
//! server and the page that plays the part of the server keep the sockets,
//! the pipes and the timer, and call the core for each thing that happens.
//! The core writes the messages for the engine into a buffer, and the host
//! takes them with [`ServerCore::take_engine_output`] and writes them to the
//! engine. The order of the messages is the order of the calls, as long as
//! one task of the host takes the output. The host passes the bytes of the
//! engine to [`ServerCore::from_engine`], and asks
//! [`ServerCore::next_for`] what to send to each view.
//!
//! A player whose view drops comes back with [`ServerCore::reconnect`]. The
//! host gives each player a token of its own at the join, such as 16
//! random bytes that the page keeps in its `sessionStorage`, and calls
//! `reconnect` for a connection that brings the token back.
//!
//! A host with tasks wakes the task that writes to the engine with a
//! `notify_one` after each call that leaves output, since a
//! `notify_waiters` is lost when the task is not waiting yet. It wakes the
//! tasks of the views with a `watch` of the whole room after
//! `from_engine`, `engine_ended`, `leave` and `reconnect`, and each task of
//! a view subscribes before its first `next_for`. The page runs on one
//! thread, and after each call it sends what waits for every view and for
//! the engine.

use std::collections::BTreeMap;
use std::io;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::event::{
    InputEvent, KeyEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent,
    PadButton, PadEvent,
};
use crate::wire;
use crate::wire::framing::{self, Side};
use crate::wire::to_engine::{self, Member, Roster};
use crate::wire::to_server;
use crate::wire::to_view::{self, Arm};

/// The rules of a room, from the players and the timer of the host to the
/// messages for the engine, and from the engine to the views.
///
/// The room waits in the lobby until [`ServerCore::start`], then plays until
/// [`ServerCore::close`], and is over when the engine ends. Only a playing
/// room writes to the engine. The players join in the lobby, and the start
/// tells the engine who they are. A player who leaves the game keeps the
/// seat, so the engine sees the same players until the end.
#[derive(Debug)]
pub struct ServerCore {
    /// A number never returns, so a late message for a player who left the
    /// lobby never goes to a new one.
    next_player: NonZeroU32,
    seats: BTreeMap<NonZeroU32, Seat>,
    phase: Phase,
    /// The messages for the engine that the host has not taken yet.
    to_engine: Vec<u8>,
    /// The bytes of the engine that do not make a whole message yet.
    from_engine: Vec<u8>,
    /// The assets of the engine, in its order. Each view gets all of them.
    assets: Vec<Arc<[u8]>>,
    /// The newest frame for every player, for a view that joins after it.
    frame_for_all: Option<Arc<[u8]>>,
}

/// The connection of a view to the seat of a player. A reconnect gives the
/// seat a new generation, so the old connection of the seat, which may
/// still look alive to the host, gets nothing and changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Conn {
    player: NonZeroU32,
    generation: u32,
}

impl Conn {
    /// The player of the seat.
    pub fn player(self) -> NonZeroU32 {
        self.player
    }
}

/// What [`ServerCore::next_for`] has for a view.
#[derive(Debug)]
pub enum Next {
    /// A message of the engine, with no envelope, as a WebSocket carries
    /// it.
    Send(Arc<[u8]>),
    /// Nothing for now.
    Idle,
    /// Nothing ever again, so the host closes the connection.
    Gone,
}

/// What went wrong with the bytes of the engine.
#[derive(Debug)]
pub enum EngineError {
    /// A message does not decode. The core drops it and goes on.
    Payload(wire::Error),
    /// The stream broke, with a header that is not one of the engine or
    /// with the end inside a message. The room is over, and the engine may
    /// still run, blocked on a write, so the host closes its pipe or kills
    /// it.
    Broken(io::Error),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Payload(e) => write!(f, "message does not decode: {e}"),
            EngineError::Broken(e) => write!(f, "broken stream: {e}"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Payload(e) => Some(e),
            EngineError::Broken(e) => Some(e),
        }
    }
}

/// Why [`ServerCore::from_view`] dropped a message of a view.
#[derive(Debug)]
pub enum ViewError {
    /// The message has more bytes than [`MAX_VIEW_BYTES`].
    TooLong(usize),
    /// The message does not decode.
    Payload(wire::Error),
}

impl std::fmt::Display for ViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewError::TooLong(len) => {
                write!(f, "message of {len} bytes exceeds cap {MAX_VIEW_BYTES}")
            }
            ViewError::Payload(e) => write!(f, "message does not decode: {e}"),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ViewError::Payload(e) => Some(e),
            ViewError::TooLong(_) => None,
        }
    }
}

/// The cap on a message of a view. Input is small, and the cap keeps what
/// the core writes for the engine under the cap of the framing.
pub const MAX_VIEW_BYTES: usize = 64 * 1024;

/// The cap on a nickname, in bytes of UTF-8.
const MAX_NICKNAME_BYTES: usize = 64;

/// A message for the engine is far below the cap of the framing, since the
/// nicknames and the messages of the views have a cap.
const UNDER_THE_CAP: &str = "a message for the engine is under the cap of the framing";

impl ServerCore {
    pub fn new() -> Self {
        ServerCore {
            next_player: NonZeroU32::MIN,
            seats: BTreeMap::new(),
            phase: Phase::Lobby,
            to_engine: Vec::new(),
            from_engine: Vec::new(),
            assets: Vec::new(),
            frame_for_all: None,
        }
    }

    /// Seat a new player as `nickname` in the lobby, and return the
    /// connection of its view, or `None` after the start. The nickname loses
    /// its control characters, so it cannot move the cursor of a terminal
    /// that prints it, and is cut to 64 bytes. The view gets the assets and
    /// the newest frame for every player.
    pub fn join(&mut self, nickname: &str) -> Option<Conn> {
        if self.phase != Phase::Lobby {
            return None;
        }
        let player = self.next_player;
        self.next_player = player
            .checked_add(1)
            .expect("a room has fewer than 2^32 joins");
        self.seats.insert(
            player,
            Seat {
                nickname: clean_nickname(nickname),
                generation: 0,
                lobby_size: None,
                held: Held::default(),
                frame: self.frame_for_all.clone(),
                frame_sent: false,
                assets_sent: 0,
            },
        );
        Some(Conn {
            player,
            generation: 0,
        })
    }

    /// Say that the WebSocket of `conn` closed. In the lobby the player
    /// loses the seat. After the start the seat stays, and its input and
    /// frames stop until a reconnect. The engine gets an `Up` for each key
    /// and button that the view held. A second call, and a call for an old
    /// connection of the seat, do nothing.
    pub fn leave(&mut self, conn: Conn) {
        if seat_of(&mut self.seats, conn).is_none() {
            return;
        }
        if self.phase == Phase::Lobby {
            self.seats.remove(&conn.player);
        } else {
            self.release_held(conn.player);
            new_generation(&mut self.seats, conn.player);
        }
    }

    /// A new connection to the seat of `player`, or `None` if the room has
    /// no such seat or is over. The host checks the token of the player
    /// first. The old connection of the seat gets [`Next::Gone`], and the
    /// new one gets every asset and the newest frame again. The engine gets
    /// an `Up` for each key and button that the old view held, since the
    /// old view may never have left.
    pub fn reconnect(&mut self, player: NonZeroU32) -> Option<Conn> {
        if self.phase == Phase::Over {
            return None;
        }
        self.release_held(player);
        let seat = new_generation(&mut self.seats, player)?;
        seat.assets_sent = 0;
        seat.frame_sent = false;
        Some(Conn {
            player,
            generation: seat.generation,
        })
    }

    /// Returns `true` if the room goes from the lobby to the game, `false`
    /// otherwise, as for a second click on start. The engine gets a start
    /// with the players of the seats, then the last resize of each view in
    /// the lobby.
    pub fn start(&mut self) -> bool {
        match self.phase {
            Phase::Lobby => {}
            Phase::Playing | Phase::Closing | Phase::Over => return false,
        }
        let members = self
            .seats
            .iter()
            .map(|(&player, seat)| Member {
                player,
                nickname: seat.nickname.clone(),
            })
            .collect();
        let roster = Roster::new(members).expect("a map has each player once");
        to_engine::write_start(&mut self.to_engine, &roster).expect(UNDER_THE_CAP);
        for (&player, seat) in &mut self.seats {
            if let Some((width, height)) = seat.lobby_size.take() {
                let event = InputEvent::Resize { width, height };
                to_engine::write_input(&mut self.to_engine, player, &event).expect(UNDER_THE_CAP);
            }
        }
        self.phase = Phase::Playing;
        true
    }

    /// Tell the engine to draw the next frames, in the game.
    pub fn tick(&mut self) {
        if self.phase == Phase::Playing {
            to_engine::write_tick(&mut self.to_engine).expect(UNDER_THE_CAP);
        }
    }

    /// Stop writing to the engine, and drop what the host has not taken.
    /// The host then closes the pipe of the engine, and the end of the pipe
    /// tells the engine to end. The engine may still send its last frames.
    pub fn close(&mut self) {
        match self.phase {
            Phase::Lobby | Phase::Playing => {
                self.to_engine = Vec::new();
                self.phase = Phase::Closing;
            }
            Phase::Closing | Phase::Over => {}
        }
    }

    /// Returns `true` if the engine ended, `false` otherwise. The timer of
    /// the tick and the task that reads the engine stop here.
    pub fn is_over(&self) -> bool {
        self.phase == Phase::Over
    }

    /// Take a message that the view of `conn` sent over its WebSocket, and
    /// pass its event to [`Self::input`]. A message from a newer schema is
    /// dropped.
    pub fn from_view(&mut self, conn: Conn, payload: &[u8]) -> Result<(), ViewError> {
        if payload.len() > MAX_VIEW_BYTES {
            return Err(ViewError::TooLong(payload.len()));
        }
        if let Some(event) = to_server::decode(payload).map_err(ViewError::Payload)? {
            self.input(conn, &event);
        }
        Ok(())
    }

    /// Pass `event` of the view of `conn` to the engine, in the game. The
    /// tick paces the engine, so a Vsync of the view is dropped. In the
    /// lobby the engine has not started, and the room keeps only the size
    /// of the last resize, for the start. The input of a player who left is
    /// dropped.
    pub fn input(&mut self, conn: Conn, event: &InputEvent) {
        let Some(seat) = seat_of(&mut self.seats, conn) else {
            return;
        };
        if matches!(event, InputEvent::Vsync) {
            return;
        }
        match self.phase {
            Phase::Lobby => {
                if let InputEvent::Resize { width, height } = *event {
                    seat.lobby_size = Some((width, height));
                }
            }
            Phase::Playing => {
                seat.held.track(event);
                to_engine::write_input(&mut self.to_engine, conn.player, event)
                    .expect(UNDER_THE_CAP);
            }
            Phase::Closing | Phase::Over => {}
        }
    }

    /// Take the next bytes of the engine. They may end anywhere, inside a
    /// message too. An asset goes to every view, and a frame to its player,
    /// or to every player. A broken stream ends the room, and the core
    /// ignores what comes after the end. Returns what went wrong, in the
    /// order of the stream.
    pub fn from_engine(&mut self, bytes: &[u8]) -> Vec<EngineError> {
        let mut errors = Vec::new();
        if self.phase == Phase::Over {
            return errors;
        }
        self.from_engine.extend_from_slice(bytes);
        let mut at = 0;
        loop {
            let bytes = self.from_engine.get(at..).expect("at is inside the bytes");
            let (payload, rest) = match framing::split_frame(bytes, Side::Engine) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => {
                    errors.push(EngineError::Broken(e));
                    self.end();
                    return errors;
                }
            };
            let arm = to_view::arm(payload);
            let payload = Arc::<[u8]>::from(payload);
            at = self.from_engine.len() - rest.len();
            match arm {
                Ok(Some(Arm::Asset)) => self.assets.push(payload),
                Ok(Some(Arm::Frame { player: None })) => {
                    for seat in self.seats.values_mut() {
                        seat.frame = Some(payload.clone());
                        seat.frame_sent = false;
                    }
                    self.frame_for_all = Some(payload);
                }
                Ok(Some(Arm::Frame {
                    player: Some(player),
                })) => {
                    if let Some(seat) = self.seats.get_mut(&player) {
                        seat.frame = Some(payload);
                        seat.frame_sent = false;
                    }
                }
                // The core does not start the session at the hello yet.
                Ok(Some(Arm::Hello(_)) | None) => {}
                Err(e) => errors.push(EngineError::Payload(e)),
            }
        }
        self.from_engine.drain(..at);
        errors
    }

    /// Say that the stream of the engine ended, which ends the room. The
    /// part of a message that is left breaks the stream.
    pub fn engine_ended(&mut self) -> Option<EngineError> {
        if self.phase == Phase::Over {
            return None;
        }
        let broken = (!self.from_engine.is_empty())
            .then(|| EngineError::Broken(io::ErrorKind::UnexpectedEof.into()));
        self.end();
        broken
    }

    /// The next message for the view of `conn`. The assets come first, so
    /// an asset arrives before the frame that draws it. Then comes the
    /// newest frame that the view has not got, and a view that falls behind
    /// skips the frames in between. When the room is over, the view gets
    /// [`Next::Gone`] after its last frame, as does the view of a player
    /// who left.
    pub fn next_for(&mut self, conn: Conn) -> Next {
        let Some(seat) = seat_of(&mut self.seats, conn) else {
            return Next::Gone;
        };
        if let Some(asset) = self.assets.get(seat.assets_sent) {
            seat.assets_sent += 1;
            return Next::Send(asset.clone());
        }
        if let Some(frame) = seat.frame.as_ref().filter(|_| !seat.frame_sent) {
            seat.frame_sent = true;
            return Next::Send(frame.clone());
        }
        match self.phase {
            Phase::Lobby | Phase::Playing | Phase::Closing => Next::Idle,
            Phase::Over => Next::Gone,
        }
    }

    /// Move the messages for the engine to the end of `buf`. A host that
    /// cannot write all of them keeps the rest in `buf` for the next write.
    pub fn take_engine_output(&mut self, buf: &mut Vec<u8>) {
        buf.append(&mut self.to_engine);
    }

    /// Send the engine an `Up` for each key and button that the view of
    /// `player` holds, since a view that drops never sends them.
    fn release_held(&mut self, player: NonZeroU32) {
        let Some(seat) = self.seats.get_mut(&player) else {
            return;
        };
        let released = seat.held.release();
        if self.phase != Phase::Playing {
            return;
        }
        for event in &released {
            to_engine::write_input(&mut self.to_engine, player, event).expect(UNDER_THE_CAP);
        }
    }

    /// End the room. The engine no longer reads, so the messages for it
    /// are dropped.
    fn end(&mut self) {
        self.phase = Phase::Over;
        self.to_engine = Vec::new();
        self.from_engine = Vec::new();
    }
}

impl Default for ServerCore {
    fn default() -> Self {
        Self::new()
    }
}

/// The seat of a player of the room.
#[derive(Debug)]
struct Seat {
    nickname: String,
    /// The generation of the connection that the seat takes. A leave and a
    /// reconnect move it on, so no older connection matches.
    generation: u32,
    /// The size of the last resize in the lobby, for the start.
    lobby_size: Option<(f32, f32)>,
    /// What the view holds down, as the engine saw it.
    held: Held,
    /// The newest frame for the player, kept for a reconnect.
    frame: Option<Arc<[u8]>>,
    /// Whether the view got `frame`.
    frame_sent: bool,
    /// How many of the assets of the room the view got.
    assets_sent: usize,
}

/// The keys and the buttons that a view holds down.
#[derive(Debug, Default)]
struct Held {
    /// Each key by the name from its `Down`. An `Up` with another name, as
    /// after a Shift, leaves the key here, and the engine later gets one
    /// `Up` too many, which does no harm. A missing `Up` does.
    keys: Vec<String>,
    /// The last position of the mouse and the buttons that it holds.
    mouse: (f32, f32, MouseButtons),
    pad: Vec<PadButton>,
}

impl Held {
    fn track(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key(k) => match k.kind {
                KeyKind::Down if !self.keys.contains(&k.key) => self.keys.push(k.key.clone()),
                KeyKind::Up => self.keys.retain(|key| *key != k.key),
                KeyKind::Down | KeyKind::Press => {}
            },
            InputEvent::Mouse(m) => self.mouse = (m.x, m.y, m.buttons),
            InputEvent::Pad(PadEvent::Down(b)) if !self.pad.contains(b) => self.pad.push(*b),
            InputEvent::Pad(PadEvent::Up(b)) => self.pad.retain(|p| p != b),
            InputEvent::Pad(_) | InputEvent::Resize { .. } | InputEvent::Vsync => {}
        }
    }

    /// An `Up` for each key and button, which leaves nothing held.
    fn release(&mut self) -> Vec<InputEvent> {
        let mut ups: Vec<InputEvent> = self
            .keys
            .drain(..)
            .map(|key| {
                InputEvent::Key(KeyEvent {
                    kind: KeyKind::Up,
                    key,
                    modifiers: Modifiers::default(),
                    repeat: false,
                })
            })
            .collect();
        let (x, y, mut buttons) = self.mouse;
        for button in [
            MouseButton::Left,
            MouseButton::Middle,
            MouseButton::Right,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            if buttons.contains(button) {
                buttons = buttons.without(button);
                ups.push(InputEvent::Mouse(MouseEvent {
                    action: MouseAction::Up(button),
                    x,
                    y,
                    modifiers: Modifiers::default(),
                    buttons,
                }));
            }
        }
        self.mouse.2 = buttons;
        ups.extend(self.pad.drain(..).map(|b| InputEvent::Pad(PadEvent::Up(b))));
        ups
    }
}

/// Whether the room waits for its start, plays, told the engine to end, or
/// the engine ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Lobby,
    Playing,
    Closing,
    Over,
}

/// The seat of `conn`, if `conn` is the connection that the seat takes.
fn seat_of(seats: &mut BTreeMap<NonZeroU32, Seat>, conn: Conn) -> Option<&mut Seat> {
    seats
        .get_mut(&conn.player)
        .filter(|seat| seat.generation == conn.generation)
}

/// Move the seat of `player` to a new generation, and return the seat, or
/// `None` if the room has no such seat.
fn new_generation(seats: &mut BTreeMap<NonZeroU32, Seat>, player: NonZeroU32) -> Option<&mut Seat> {
    let seat = seats.get_mut(&player)?;
    seat.generation = seat
        .generation
        .checked_add(1)
        .expect("a seat has fewer than 2^32 connections");
    Some(seat)
}

/// `nickname` without its control characters, cut at a character to
/// [`MAX_NICKNAME_BYTES`].
fn clean_nickname(nickname: &str) -> String {
    let mut clean = String::new();
    for c in nickname.chars().filter(|c| !c.is_control()) {
        if clean.len() + c.len_utf8() > MAX_NICKNAME_BYTES {
            break;
        }
        clean.push(c);
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;
    use crate::session::{Session, SessionEvent};

    /// A core with a session that reads what the core writes, which also
    /// checks that the core writes nothing that a session rejects.
    struct Room {
        core: ServerCore,
        engine: Session,
    }

    impl Room {
        fn new() -> Self {
            Room {
                core: ServerCore::new(),
                engine: Session::new(),
            }
        }

        /// The events that the engine got since the last call, in a short
        /// form.
        fn events(&mut self) -> Vec<String> {
            let mut buf = Vec::new();
            self.core.take_engine_output(&mut buf);
            self.engine.feed(&buf);
            std::iter::from_fn(|| self.engine.next_event())
                .map(|e| match e {
                    SessionEvent::Start(roster) => {
                        let members: Vec<_> = roster
                            .members()
                            .iter()
                            .map(|m| format!("{} {}", m.player, m.nickname))
                            .collect();
                        format!("start {}", members.join(", "))
                    }
                    SessionEvent::Vsync => "tick".into(),
                    SessionEvent::Input { player, event } => match event {
                        InputEvent::Key(k) if k.kind == KeyKind::Up => {
                            format!("{player} up {}", k.key)
                        }
                        InputEvent::Key(k) => format!("{player} key {}", k.key),
                        InputEvent::Mouse(MouseEvent {
                            action: MouseAction::Up(b),
                            x,
                            y,
                            buttons,
                            ..
                        }) => format!("{player} mouse up {b:?} {x},{y} {buttons:?}"),
                        InputEvent::Pad(PadEvent::Up(b)) => format!("{player} pad up {b:?}"),
                        InputEvent::Resize { width, height } => {
                            format!("{player} resize {width}x{height}")
                        }
                        InputEvent::Mouse(_) | InputEvent::Vsync | InputEvent::Pad(_) => {
                            format!("{player} {event:?}")
                        }
                    },
                    SessionEvent::Error(e) => format!("error {e}"),
                    SessionEvent::End(None) => "end".into(),
                    SessionEvent::End(Some(e)) => format!("broken {e}"),
                })
                .collect()
        }
    }

    fn key_as(kind: KeyKind, name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn mouse(action: MouseAction, x: f32, buttons: MouseButtons) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
            action,
            x,
            y: 0.0,
            modifiers: Modifiers::default(),
            buttons,
        })
    }

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn resize(width: f32, height: f32) -> InputEvent {
        InputEvent::Resize { width, height }
    }

    /// A frame of width `width` for `player`, or for every player, with
    /// its envelope.
    fn frame(player: u32, width: f32) -> Vec<u8> {
        let mut out = Vec::new();
        to_view::write_frame(&mut out, NonZeroU32::new(player), &Scene::new(width, 1.0)).unwrap();
        out
    }

    fn asset(id: u32) -> Vec<u8> {
        let mut out = Vec::new();
        to_view::write_asset(&mut out, id, &[1, 2, 3], None).unwrap();
        out
    }

    /// Everything that waits for the view of `conn`, in a short form, up to
    /// the first `Idle` or `Gone`.
    fn sent(core: &mut ServerCore, conn: Conn) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            match core.next_for(conn) {
                Next::Send(payload) => {
                    let mut framed = framing::header(Side::Engine, payload.len() as u32).to_vec();
                    framed.extend_from_slice(&payload);
                    out.push(match to_view::read(&mut &framed[..]).unwrap().unwrap() {
                        to_view::Message::Asset { id, .. } => format!("asset {id}"),
                        to_view::Message::Frame { player, scene } => match player {
                            Some(p) => format!("frame {p} {}", scene.width()),
                            None => format!("frame all {}", scene.width()),
                        },
                        to_view::Message::Hello(_) => "hello".into(),
                    });
                }
                Next::Idle => {
                    out.push("idle".into());
                    return out;
                }
                Next::Gone => {
                    out.push("gone".into());
                    return out;
                }
            }
        }
    }

    #[test]
    fn the_start_has_the_players_of_the_lobby() {
        let mut room = Room::new();
        room.core.join("Ana").unwrap();
        let beto = room.core.join("Beto").unwrap();
        room.core.join("Caio").unwrap();
        room.core.leave(beto);
        room.core.tick();
        assert!(room.events().is_empty());
        assert!(room.core.start());
        assert_eq!(room.events(), ["start 1 Ana, 3 Caio"]);
        assert!(!room.core.start());
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_join_after_the_start_is_refused_and_a_leave_keeps_the_seat() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        room.core.start();
        assert!(room.core.join("Beto").is_none());
        room.core.tick();
        room.core.leave(ana);
        room.core.leave(ana);
        room.core.input(ana, &key("a"));
        assert_eq!(room.events(), ["start 1 Ana", "tick"]);
        assert_eq!(sent(&mut room.core, ana), ["gone"]);
    }

    #[test]
    fn a_number_never_returns() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.leave(ana);
        assert_eq!(core.join("Beto").unwrap().player().get(), 2);
    }

    #[test]
    fn the_close_drops_what_the_host_has_not_taken_and_nothing_follows() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        room.core.start();
        assert_eq!(room.events(), ["start 1 Ana"]);
        room.core.tick();
        room.core.close();
        room.core.close();
        room.core.tick();
        assert!(room.core.join("Beto").is_none());
        room.core.leave(ana);
        assert!(!room.core.start());
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_room_closed_in_the_lobby_never_starts() {
        let mut room = Room::new();
        room.core.close();
        assert!(!room.core.start());
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_nickname_loses_its_control_characters_and_is_cut_at_a_character() {
        assert_eq!(clean_nickname("A\u{1b}[2Jna\n"), "A[2Jna");
        let long = "é".repeat(40);
        assert_eq!(clean_nickname(&long), "é".repeat(32));
    }

    #[test]
    fn the_output_goes_after_what_the_buffer_holds() {
        let mut core = ServerCore::new();
        core.start();
        let mut buf = b"rest".to_vec();
        core.take_engine_output(&mut buf);
        assert_eq!(&buf[..4], b"rest");
        assert!(buf.len() > 4);
        let mut again = Vec::new();
        core.take_engine_output(&mut again);
        assert!(again.is_empty());
    }

    #[test]
    fn the_input_of_a_view_goes_with_its_player() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        let beto = room.core.join("Beto").unwrap();
        room.core.start();
        room.core
            .from_view(beto, &to_server::encode_input(&key("b")))
            .unwrap();
        room.core.input(ana, &key("a"));
        room.core.input(ana, &InputEvent::Vsync);
        assert_eq!(room.events(), ["start 1 Ana, 2 Beto", "2 key b", "1 key a"]);
    }

    #[test]
    fn the_lobby_keeps_the_last_resize_for_the_start() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        let beto = room.core.join("Beto").unwrap();
        room.core.input(ana, &resize(10.0, 10.0));
        room.core.input(ana, &key("a"));
        room.core.input(ana, &resize(20.0, 30.0));
        room.core.input(beto, &key("b"));
        room.core.start();
        assert_eq!(room.events(), ["start 1 Ana, 2 Beto", "1 resize 20x30"]);
        room.core.input(ana, &resize(40.0, 50.0));
        assert_eq!(room.events(), ["1 resize 40x50"]);
    }

    #[test]
    fn a_message_of_a_view_that_is_too_long_or_does_not_decode_is_an_error() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        let long = vec![0; MAX_VIEW_BYTES + 1];
        assert!(matches!(
            core.from_view(ana, &long),
            Err(ViewError::TooLong(_))
        ));
        assert!(matches!(
            core.from_view(ana, b"junk"),
            Err(ViewError::Payload(_))
        ));
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_dropped() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        room.core.start();
        let unknown = wire::with_unknown_view_value(&to_server::encode_input(&key("a")), |m| {
            wire::tag_of(m.get_event().unwrap())
        });
        room.core.from_view(ana, &unknown).unwrap();
        assert_eq!(room.events(), ["start 1 Ana"]);
    }

    #[test]
    fn a_view_gets_the_assets_then_its_newest_frame() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        let beto = core.join("Beto").unwrap();
        core.start();
        let mut stream = frame(0, 1.0);
        stream.extend_from_slice(&asset(7));
        stream.extend_from_slice(&frame(1, 2.0));
        stream.extend_from_slice(&frame(0, 3.0));
        stream.extend_from_slice(&frame(2, 4.0));
        assert!(core.from_engine(&stream).is_empty());
        assert_eq!(sent(&mut core, ana), ["asset 7", "frame all 3", "idle"]);
        assert_eq!(sent(&mut core, beto), ["asset 7", "frame 2 4", "idle"]);
        core.from_engine(&frame(1, 5.0));
        assert_eq!(sent(&mut core, ana), ["frame 1 5", "idle"]);
        assert_eq!(sent(&mut core, beto), ["idle"]);
    }

    #[test]
    fn a_view_that_joins_after_a_frame_gets_the_assets_and_the_frame_for_all() {
        let mut core = ServerCore::new();
        let mut stream = asset(1);
        stream.extend_from_slice(&frame(0, 1.0));
        stream.extend_from_slice(&frame(1, 2.0));
        core.from_engine(&stream);
        let ana = core.join("Ana").unwrap();
        assert_eq!(sent(&mut core, ana), ["asset 1", "frame all 1", "idle"]);
    }

    #[test]
    fn the_messages_come_out_whole_from_bytes_fed_one_at_a_time() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.start();
        let mut stream = asset(1);
        stream.extend_from_slice(&frame(1, 2.0));
        for byte in &stream {
            assert!(core.from_engine(std::slice::from_ref(byte)).is_empty());
        }
        assert_eq!(sent(&mut core, ana), ["asset 1", "frame 1 2", "idle"]);
    }

    #[test]
    fn the_end_of_the_engine_ends_the_room_after_the_last_frame() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        room.core.start();
        room.core.tick();
        room.core.from_engine(&frame(0, 1.0));
        assert!(room.core.engine_ended().is_none());
        assert!(room.core.is_over());
        room.core.tick();
        room.core.from_engine(&frame(0, 2.0));
        assert!(room.core.engine_ended().is_none());
        assert!(room.events().is_empty());
        assert_eq!(sent(&mut room.core, ana), ["frame all 1", "gone"]);
        assert_eq!(sent(&mut room.core, ana), ["gone"]);
    }

    #[test]
    fn the_view_of_a_player_who_left_is_gone() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.start();
        core.leave(ana);
        assert!(core.from_engine(&frame(1, 1.0)).is_empty());
        assert_eq!(sent(&mut core, ana), ["gone"]);
    }

    #[test]
    fn a_view_that_reconnects_gets_every_asset_and_the_newest_frame_again() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.start();
        let mut stream = asset(1);
        stream.extend_from_slice(&frame(1, 2.0));
        core.from_engine(&stream);
        assert_eq!(sent(&mut core, ana), ["asset 1", "frame 1 2", "idle"]);
        core.leave(ana);
        let again = core.reconnect(ana.player()).unwrap();
        assert_eq!(sent(&mut core, again), ["asset 1", "frame 1 2", "idle"]);
        core.from_engine(&frame(0, 3.0));
        assert_eq!(sent(&mut core, again), ["frame all 3", "idle"]);
    }

    #[test]
    fn the_old_connection_of_a_seat_gets_nothing_and_changes_nothing() {
        for start in [false, true] {
            let mut room = Room::new();
            let ana = room.core.join("Ana").unwrap();
            if start {
                room.core.start();
                room.events();
            }
            let again = room.core.reconnect(ana.player()).unwrap();
            room.core.input(ana, &key("a"));
            room.core.leave(ana);
            assert_eq!(sent(&mut room.core, ana), ["gone"]);
            assert_eq!(sent(&mut room.core, again), ["idle"]);
            room.core.input(again, &key("b"));
            room.core.start();
            // In the lobby the start shows that the seat stayed, and in the
            // game the key of the new connection goes through.
            let expected: &[&str] = if start {
                &["1 key b"]
            } else {
                &["start 1 Ana"]
            };
            assert_eq!(room.events(), expected);
        }
    }

    #[test]
    fn a_leave_in_the_game_releases_what_the_view_held() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        room.core.start();
        let left = MouseButtons::default().with(MouseButton::Left);
        let both = left.with(MouseButton::Right);
        for event in [
            key_as(KeyKind::Down, "a"),
            key_as(KeyKind::Down, "ArrowRight"),
            key_as(KeyKind::Up, "a"),
            mouse(MouseAction::Down(MouseButton::Left), 1.0, left),
            mouse(MouseAction::Down(MouseButton::Right), 2.0, both),
            mouse(MouseAction::Move, 3.0, both),
            InputEvent::Pad(PadEvent::Down(PadButton::A)),
            InputEvent::Pad(PadEvent::Down(PadButton::B)),
            InputEvent::Pad(PadEvent::Up(PadButton::B)),
        ] {
            room.core.input(ana, &event);
        }
        room.events();
        room.core.leave(ana);
        assert_eq!(
            room.events(),
            [
                "1 up ArrowRight",
                format!(
                    "1 mouse up Left 3,0 {:?}",
                    MouseButtons::default().with(MouseButton::Right)
                )
                .as_str(),
                format!("1 mouse up Right 3,0 {:?}", MouseButtons::default()).as_str(),
                "1 pad up A",
            ]
        );
    }

    #[test]
    fn a_reconnect_releases_what_the_old_view_held_once() {
        let mut room = Room::new();
        let ana = room.core.join("Ana").unwrap();
        room.core.start();
        room.core.input(ana, &key_as(KeyKind::Down, "a"));
        room.events();
        let again = room.core.reconnect(ana.player()).unwrap();
        assert_eq!(room.events(), ["1 up a"]);
        room.core.leave(ana);
        room.core.leave(again);
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_reconnect_to_no_seat_or_to_a_room_that_is_over_is_refused() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.leave(ana);
        assert!(core.reconnect(ana.player()).is_none());
        let beto = core.join("Beto").unwrap();
        core.start();
        assert!(core.engine_ended().is_none());
        assert!(core.reconnect(beto.player()).is_none());
    }

    #[test]
    fn a_message_of_the_engine_that_does_not_decode_is_an_error_and_the_stream_goes_on() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.start();
        let mut stream = framing::header(Side::Engine, 8).to_vec();
        stream.extend_from_slice(&[0xff; 8]);
        stream.extend_from_slice(&frame(0, 1.0));
        let errors = core.from_engine(&stream);
        assert!(matches!(errors[..], [EngineError::Payload(_)]));
        assert_eq!(sent(&mut core, ana), ["frame all 1", "idle"]);
    }

    #[test]
    fn a_header_of_another_side_breaks_the_stream() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.start();
        let mut stream = frame(0, 1.0);
        stream.extend_from_slice(b"SIS1\0\0\0\0");
        let errors = core.from_engine(&stream);
        assert!(matches!(errors[..], [EngineError::Broken(_)]));
        assert!(core.is_over());
        assert_eq!(sent(&mut core, ana), ["frame all 1", "gone"]);
    }

    #[test]
    fn the_end_inside_a_message_breaks_the_stream() {
        let mut core = ServerCore::new();
        let stream = frame(0, 1.0);
        core.from_engine(&stream[..stream.len() - 1]);
        let error = core.engine_ended();
        assert!(
            matches!(error, Some(EngineError::Broken(e)) if e.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert!(core.is_over());
    }

    #[test]
    fn the_end_between_messages_ends_the_room() {
        let mut core = ServerCore::new();
        let ana = core.join("Ana").unwrap();
        core.from_engine(&frame(0, 1.0));
        assert!(core.engine_ended().is_none());
        assert!(core.is_over());
        assert_eq!(sent(&mut core, ana), ["frame all 1", "gone"]);
    }
}
