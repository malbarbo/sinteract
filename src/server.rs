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
//! The core has no lobby. The engine says in its hello how many players
//! the game takes, [`ServerCore::players`] hands that to the lobby of the
//! host, and the host passes the players to [`ServerCore::start`]. Then
//! the host gives each player a token of its own, such as 16 random bytes
//! in the link of the player, which the page keeps in its
//! `sessionStorage`. A connection that brings the token takes the seat
//! with [`ServerCore::connect`], the first time and after a drop alike.
//!
//! A host with tasks wakes the task that writes to the engine with a
//! `notify_one` after each call that leaves output, since a
//! `notify_waiters` is lost when the task is not waiting yet. It wakes the
//! tasks of the views with a `watch` of the whole room after
//! `from_engine`, `engine_ended`, `leave` and `connect`, and each task of a
//! view subscribes before its first `next_for`. The page runs on one
//! thread, and after each call it sends what waits for every view and for
//! the engine.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::asset::{Cache, Footprint, ImageError};
use crate::event::{
    InputEvent, KeyEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent,
    PadButton, PadEvent,
};
use crate::wire;
use crate::wire::engine_message::{self, Arm, PlayerRange};
use crate::wire::framing::{self, Side};
use crate::wire::server_message;
use crate::wire::view_message;

/// The rules of a room, from the players and the timer of the host to the
/// messages for the engine, and from the engine to the views.
///
/// The room waits for the hello of the engine, then for the start from
/// the host, then plays until [`ServerCore::close`], and is over when the
/// engine ends. Only a playing room writes to the engine. A player whose
/// view drops keeps the seat, so the engine sees the same players until
/// the end.
#[derive(Debug)]
pub struct ServerCore {
    phase: Phase,
    /// The bytes of the engine that do not make a whole message yet.
    engine_in: Vec<u8>,
    /// The live assets, under the limits of the room, with the message of
    /// each, as a view gets it.
    cache: Cache<Arc<[u8]>>,
}

type Assets = BTreeMap<u32, Arc<[u8]>>;

type Seats = BTreeMap<NonZeroU32, Seat>;

/// The connection of a view to the seat of a player. A connect gives the
/// seat a new generation, so the old connection of the seat, which may
/// still look alive to the host, gets nothing and changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Conn {
    player: NonZeroU32,
    generation: u64,
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
    /// it, as for the errors below.
    Broken(io::Error),
    /// The first message of the engine is not a hello. The room is over.
    NoHello,
    /// A hello came after the first one. The core drops it and goes on.
    SecondHello,
    /// The asset `id` is not an image that a view decodes, or is larger
    /// than an image can be. The core drops it and goes on.
    Asset { id: u32, error: ImageError },
    /// An asset came for an id that names a live asset. The core drops it
    /// and goes on.
    LiveId(u32),
    /// A forget came from the engine, which never sends one. The core drops
    /// it and goes on.
    Forget(u32),
    /// A frame came for a player that has no seat in the room. The core
    /// drops it and goes on.
    NoSeat(NonZeroU32),
}

/// Why [`ServerCore::start`] did not start the room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// The engine has not said its hello yet.
    NoHello,
    /// The game does not take that many players.
    Players { players: usize, range: PlayerRange },
    /// The room started, closed or ended before.
    PastStart,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::NoHello => f.write_str("the engine has not said its hello"),
            StartError::Players { players, range } => write!(
                f,
                "the game takes from {} to {} players, not {players}",
                range.min(),
                range.max()
            ),
            StartError::PastStart => f.write_str("the room is past its start"),
        }
    }
}

impl std::error::Error for StartError {}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Payload(e) => write!(f, "message does not decode: {e}"),
            EngineError::Broken(e) => write!(f, "broken stream: {e}"),
            EngineError::NoHello => f.write_str("the first message is not a hello"),
            EngineError::SecondHello => f.write_str("a hello came after the first one"),
            EngineError::Asset { id, error } => write!(f, "asset {id}: {error}"),
            EngineError::LiveId(id) => write!(f, "asset {id}: the id already names a live asset"),
            EngineError::Forget(id) => write!(f, "a forget of {id} came from the engine"),
            EngineError::NoSeat(player) => {
                write!(f, "a frame came for player {player}, who has no seat")
            }
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Payload(e) => Some(e),
            EngineError::Broken(e) => Some(e),
            EngineError::Asset { error, .. } => Some(error),
            EngineError::NoHello
            | EngineError::SecondHello
            | EngineError::LiveId(_)
            | EngineError::Forget(_)
            | EngineError::NoSeat(_) => None,
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

/// The cap on the keys that a view holds down at once. A keyboard holds
/// far fewer, and the cap bounds what a view grows in the core.
const MAX_HELD_KEYS: usize = 32;

/// A message for the engine is far below the cap of the framing, since the
/// players, their nicknames and the messages of the views have a cap.
const UNDER_THE_CAP: &str = "a message for the engine is under the cap of the framing";

impl ServerCore {
    pub fn new() -> Self {
        ServerCore {
            phase: Phase::Waiting,
            engine_in: Vec::new(),
            cache: Cache::new(),
        }
    }

    /// The players that the game takes, from the hello of the engine, or
    /// `None` before the hello and after the start.
    pub fn players(&self) -> Option<PlayerRange> {
        match self.phase {
            Phase::Ready { range, .. } => Some(range),
            Phase::Waiting | Phase::Playing { .. } | Phase::Closing { .. } | Phase::Over { .. } => {
                None
            }
        }
    }

    /// Start the room with a player for each of `nicknames`, numbered from
    /// 1 in their order, and give the engine the start. A nickname loses
    /// its control characters, so it cannot move the cursor of a terminal
    /// that prints it, and is cut to 64 bytes.
    pub fn start<S: AsRef<str>>(&mut self, nicknames: &[S]) -> Result<(), StartError> {
        let Phase::Ready { range, lost } = &mut self.phase else {
            return Err(if matches!(self.phase, Phase::Waiting) {
                StartError::NoHello
            } else {
                StartError::PastStart
            });
        };
        if !range.contains(nicknames.len()) {
            return Err(StartError::Players {
                players: nicknames.len(),
                range: *range,
            });
        }
        let seats: Seats = (1..)
            .map_while(NonZeroU32::new)
            .zip(nicknames)
            .map(|(player, nickname)| (player, Seat::new(clean_nickname(nickname.as_ref()))))
            .collect();
        let nicknames: Vec<&str> = seats.values().map(|s| s.nickname.as_str()).collect();
        let mut engine_out = Vec::new();
        server_message::write_start(&mut engine_out, &nicknames).expect(UNDER_THE_CAP);
        for id in lost.drain(..) {
            server_message::write_lost(&mut engine_out, id).expect(UNDER_THE_CAP);
        }
        self.phase = Phase::Playing {
            seats,
            engine_out,
            tick_pending: false,
        };
        Ok(())
    }

    /// Say that the WebSocket of `conn` closed. The seat stays, and its
    /// input and frames stop until a connect. The engine gets an `Up` for
    /// each key and button that the view held. A second call, and a call
    /// for an old connection of the seat, do nothing.
    pub fn leave(&mut self, conn: Conn) {
        let Some(seat) = self
            .phase
            .seats()
            .and_then(|seats| seats.get_mut(&conn.player))
        else {
            return;
        };
        if seat.generation != conn.generation {
            return;
        }
        if let Some(old) = seat.view.take() {
            self.release(conn.player, old.held);
        }
    }

    /// A connection of a view to the seat of `player`, or `None` if the
    /// room has no such seat, as before the start, or is over. The host checks
    /// the token of the player first. The old connection of the seat gets
    /// [`Next::Gone`], and the new one starts with no asset and gets the
    /// newest frame. The engine gets an `Up` for each key and button that
    /// the old view held, since the old view may never have left.
    pub fn connect(&mut self, player: NonZeroU32) -> Option<Conn> {
        if matches!(self.phase, Phase::Over { .. }) {
            return None;
        }
        let seat = self.phase.seats()?.get_mut(&player)?;
        seat.generation = seat
            .generation
            .checked_add(1)
            .expect("a seat has fewer than 2^64 connections");
        let conn = Conn {
            player,
            generation: seat.generation,
        };
        if let Some(old) = seat.view.replace(View::default()) {
            self.release(player, old.held);
        }
        Some(conn)
    }

    /// Tell the engine to draw the next frames, in the game. The tick is
    /// dropped while the engine has not taken the last one, so the ticks
    /// of an engine slower than the timer do not pile up.
    pub fn tick(&mut self) {
        if let Phase::Playing {
            engine_out,
            tick_pending: tick_pending @ false,
            ..
        } = &mut self.phase
        {
            server_message::write_tick(engine_out).expect(UNDER_THE_CAP);
            *tick_pending = true;
        }
    }

    /// Stop writing to the engine, and drop what the host has not taken.
    /// The next [`Self::take_engine_output`] returns `false`, the host then
    /// closes the pipe of the engine, and the end of the pipe tells the
    /// engine to end. The engine may still send its last frames.
    pub fn close(&mut self) {
        match &mut self.phase {
            Phase::Waiting | Phase::Ready { .. } => {
                self.phase = Phase::Closing {
                    seats: Seats::new(),
                }
            }
            Phase::Playing { seats, .. } => {
                self.phase = Phase::Closing {
                    seats: std::mem::take(seats),
                }
            }
            Phase::Closing { .. } | Phase::Over { .. } => {}
        }
    }

    /// Returns `true` if the engine ended, `false` otherwise. The timer of
    /// the tick and the task that reads the engine stop here.
    pub fn is_over(&self) -> bool {
        matches!(self.phase, Phase::Over { .. })
    }

    /// Take a message that the view of `conn` sent over its WebSocket, and
    /// pass its event to [`Self::input`]. The core drops a message from a
    /// newer schema.
    pub fn from_view(&mut self, conn: Conn, payload: &[u8]) -> Result<(), ViewError> {
        if payload.len() > MAX_VIEW_BYTES {
            return Err(ViewError::TooLong(payload.len()));
        }
        if let Some(event) = view_message::decode(payload).map_err(ViewError::Payload)? {
            self.input(conn, &event);
        }
        Ok(())
    }

    /// Pass `event` of the view of `conn` to the engine, in the game. The
    /// core drops the input of an old connection, and a `Down` of a new key
    /// when the view holds 32 keys, since it could not release the key.
    pub fn input(&mut self, conn: Conn, event: &InputEvent) {
        let Some((seats, engine_out)) = self.phase.playing() else {
            return;
        };
        let Some((view, _)) = view_of(seats, conn) else {
            return;
        };
        if !view.held.track(event) {
            return;
        }
        server_message::write_input(engine_out, conn.player, event).expect(UNDER_THE_CAP);
    }

    /// Take the next bytes of the engine. They may end anywhere, inside a
    /// message too. The first message is the hello. Then the core keeps an
    /// asset, and a frame goes to its player, or to every player, with the
    /// assets that it draws. At each frame the core drops the assets that
    /// the frames used longest ago to fit the limits of the room, with a
    /// lost to the engine for each. Before the start a frame for every
    /// player goes to no one, and the core drops it. A frame for a player
    /// with no seat is an error. A broken stream ends the room, and the
    /// core ignores what comes after the end. Returns what went wrong, in
    /// the order of the stream.
    pub fn from_engine(&mut self, bytes: &[u8]) -> Vec<EngineError> {
        let mut errors = Vec::new();
        if matches!(self.phase, Phase::Over { .. }) {
            return errors;
        }
        // Out of `self`, so the loop reads it while the messages go in.
        let mut buffer = std::mem::take(&mut self.engine_in);
        buffer.extend_from_slice(bytes);
        let mut rest = buffer.as_slice();
        loop {
            let (payload, after) = match framing::split_message(rest, Side::Engine) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => {
                    errors.push(EngineError::Broken(e));
                    self.end();
                    return errors;
                }
            };
            let arm = engine_message::arm(payload);
            let payload = Arc::<[u8]>::from(payload);
            rest = after;
            if matches!(self.phase, Phase::Waiting) && !matches!(arm, Ok(Some(Arm::Hello(_)))) {
                if let Err(e) = arm {
                    errors.push(EngineError::Payload(e));
                }
                errors.push(EngineError::NoHello);
                self.end();
                return errors;
            }
            match arm {
                Ok(Some(Arm::Hello(range))) => match self.phase {
                    Phase::Waiting => {
                        self.phase = Phase::Ready {
                            range,
                            lost: Vec::new(),
                        }
                    }
                    Phase::Ready { .. } | Phase::Playing { .. } => {
                        errors.push(EngineError::SecondHello)
                    }
                    // A room that closed before the hello never starts.
                    Phase::Closing { .. } | Phase::Over { .. } => {}
                },
                Ok(Some(Arm::Asset { id, footprint })) => {
                    if let Err(e) = self.keep_asset(id, footprint, payload) {
                        errors.push(e);
                    }
                }
                Ok(Some(Arm::Forget(id))) => errors.push(EngineError::Forget(id)),
                Ok(Some(Arm::TickTaken)) => {
                    if let Phase::Playing { tick_pending, .. } = &mut self.phase {
                        *tick_pending = false;
                    }
                }
                Ok(Some(Arm::Frame { player, ids })) => {
                    if let Err(e) = self.keep_frame(player, &ids, payload) {
                        errors.push(e);
                    }
                }
                Ok(None) => {}
                Err(e) => errors.push(EngineError::Payload(e)),
            }
        }
        let taken = buffer.len() - rest.len();
        buffer.drain(..taken);
        self.engine_in = buffer;
        errors
    }

    /// Say that the stream of the engine ended, which ends the room. The
    /// part of a message that is left breaks the stream.
    pub fn engine_ended(&mut self) -> Option<EngineError> {
        if matches!(self.phase, Phase::Over { .. }) {
            return None;
        }
        let broken = (!self.engine_in.is_empty())
            .then(|| EngineError::Broken(io::ErrorKind::UnexpectedEof.into()));
        self.end();
        broken
    }

    /// The next message for the view of `conn`. The view gets the newest
    /// frame that it has not got, after the assets of the frame that the
    /// view lacks. The core keeps that frame as
    /// the next one of the view until the view gets it, so a view that falls
    /// behind skips the frames in between and still gets a frame now and
    /// then. The view gets a forget for an asset that is neither in the
    /// assets of the frame on its screen nor in those of its next frame.
    /// When the room is over, the view gets [`Next::Gone`] after its last
    /// frame, as does an old connection.
    pub fn next_for(&mut self, conn: Conn) -> Next {
        let Some((view, frame)) = self.phase.seats().and_then(|seats| view_of(seats, conn)) else {
            return Next::Gone;
        };
        if view.next.is_none() && !view.frame_taken {
            view.next = frame.cloned();
            view.frame_taken = true;
        }
        let needs = |id: &u32| {
            view.shown_assets.contains(id)
                || view
                    .next
                    .as_ref()
                    .is_some_and(|next| next.assets.contains_key(id))
        };
        if let Some(&id) = view.assets.iter().find(|id| !needs(id)) {
            view.assets.remove(&id);
            return Next::Send(engine_message::encode_forget(id).into());
        }
        if let Some(next) = view.next.take() {
            if let Some((&id, asset)) = next.assets.iter().find(|(id, _)| !view.assets.contains(id))
            {
                view.assets.insert(id);
                let payload = asset.clone();
                view.next = Some(next);
                return Next::Send(payload);
            }
            view.shown_assets = next.assets.keys().copied().collect();
            return Next::Send(next.payload);
        }
        match self.phase {
            Phase::Waiting
            | Phase::Ready { .. }
            | Phase::Playing { .. }
            | Phase::Closing { .. } => Next::Idle,
            Phase::Over { .. } => Next::Gone,
        }
    }

    /// Move the messages for the engine to the end of `buf`. A host that
    /// cannot write all of them keeps the rest in `buf` for the next write.
    /// Returns `false` once the core writes nothing more to the engine,
    /// after [`Self::close`] or at the end of the room, so the task that
    /// writes to the engine closes its pipe, and `true` otherwise.
    pub fn take_engine_output(&mut self, buf: &mut Vec<u8>) -> bool {
        match &mut self.phase {
            Phase::Playing { engine_out, .. } => {
                buf.append(engine_out);
                true
            }
            Phase::Waiting | Phase::Ready { .. } => true,
            Phase::Closing { .. } | Phase::Over { .. } => false,
        }
    }

    /// Keep the asset `id`, or refuse it if the id is live or the image is
    /// too large. An asset that does not fit beside the others that came
    /// after the last frame is lost at once.
    fn keep_asset(
        &mut self,
        id: u32,
        footprint: Result<Footprint, ImageError>,
        payload: Arc<[u8]>,
    ) -> Result<(), EngineError> {
        let footprint = footprint.map_err(|error| EngineError::Asset { id, error })?;
        if self.cache.contains(id) {
            return Err(EngineError::LiveId(id));
        }
        if self.cache.insert(id, footprint, payload).is_err() {
            self.lose(id);
        }
        Ok(())
    }

    /// Keep the frame in `payload` for `player`, or for every player when
    /// `player` is `None`, with the assets of its bitmap `ids`, and drop
    /// the assets that the frames used longest ago to fit the limits. A
    /// frame for every player before the start reaches no screen, so it
    /// changes nothing.
    fn keep_frame(
        &mut self,
        player: Option<NonZeroU32>,
        ids: &BTreeSet<u32>,
        payload: Arc<[u8]>,
    ) -> Result<(), EngineError> {
        let Some(seats) = self.phase.seats() else {
            return player.map_or(Ok(()), |player| Err(EngineError::NoSeat(player)));
        };
        if let Some(player) = player
            && !seats.contains_key(&player)
        {
            return Err(EngineError::NoSeat(player));
        }
        let assets: Assets = ids
            .iter()
            .filter_map(|id| Some((*id, self.cache.get(*id)?.clone())))
            .collect();
        let gone = self.cache.use_frame(ids);
        let frame = Frame {
            payload,
            assets: Arc::new(assets),
        };
        for (_, seat) in seats
            .iter_mut()
            .filter(|(p, _)| player.is_none_or(|player| **p == player))
        {
            seat.frame = Some(frame.clone());
            if let Some(view) = &mut seat.view {
                view.frame_taken = false;
            }
        }
        for id in gone {
            self.lose(id);
        }
        Ok(())
    }

    /// Tell the engine that the asset `id` is gone. Before the start, the
    /// lost waits for the start, which comes first.
    fn lose(&mut self, id: u32) {
        match &mut self.phase {
            Phase::Playing { engine_out, .. } => {
                server_message::write_lost(engine_out, id).expect(UNDER_THE_CAP);
            }
            Phase::Ready { lost, .. } => lost.push(id),
            // No asset comes before the hello.
            Phase::Waiting | Phase::Closing { .. } | Phase::Over { .. } => {}
        }
    }

    /// Send the engine an `Up` for each key and button in `held`, what a
    /// view of `player` held when it went, since a view that drops never
    /// sends them.
    fn release(&mut self, player: NonZeroU32, mut held: Held) {
        let Phase::Playing { engine_out, .. } = &mut self.phase else {
            return;
        };
        for event in held.release() {
            server_message::write_input(engine_out, player, &event).expect(UNDER_THE_CAP);
        }
    }

    /// End the room. The engine no longer reads, so the core drops the
    /// messages for it.
    fn end(&mut self) {
        let seats = self.phase.seats().map(std::mem::take).unwrap_or_default();
        self.phase = Phase::Over { seats };
        self.engine_in = Vec::new();
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
    /// The generation of the last connect, so no older connection matches.
    /// A view that connects a million times a second takes 500,000 years
    /// to run out of them.
    generation: u64,
    /// The newest frame for the player, kept for the next connect.
    frame: Option<Frame>,
    /// The view that takes the seat, until it leaves.
    view: Option<View>,
}

/// A view that takes a seat. A connect starts a new one.
#[derive(Debug, Default)]
struct View {
    /// What the view holds down, as the engine saw it.
    held: Held,
    /// Whether the view took the frame of the seat, into `next` or onto its
    /// screen, so a new frame of the seat clears it.
    frame_taken: bool,
    /// The next frame of the view, until the view has its assets and it.
    next: Option<Frame>,
    /// The ids of the assets that the view has.
    assets: BTreeSet<u32>,
    /// The ids of the assets of the frame on the screen of the view.
    shown_assets: BTreeSet<u32>,
}

/// A frame, with the assets that it draws, as they were at its arrival. The
/// frame keeps them for a view that has not got them, after the cache drops
/// them.
#[derive(Clone, Debug)]
struct Frame {
    payload: Arc<[u8]>,
    assets: Arc<Assets>,
}

impl Seat {
    /// A seat with no view yet. No connection has generation 0.
    fn new(nickname: String) -> Seat {
        Seat {
            nickname,
            generation: 0,
            frame: None,
            view: None,
        }
    }
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
    /// Returns `true` if `event` goes to the engine, `false` otherwise. A
    /// `Down` of a new key past [`MAX_HELD_KEYS`] does not.
    fn track(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Key(k) => match k.kind {
                KeyKind::Down if !self.keys.contains(&k.key) => {
                    if self.keys.len() == MAX_HELD_KEYS {
                        return false;
                    }
                    self.keys.push(k.key.clone());
                }
                KeyKind::Up => self.keys.retain(|key| *key != k.key),
                KeyKind::Down | KeyKind::Press => {}
            },
            InputEvent::Mouse(m) => self.mouse = (m.x, m.y, m.buttons),
            InputEvent::Pad(PadEvent::Down(b)) if !self.pad.contains(b) => self.pad.push(*b),
            InputEvent::Pad(PadEvent::Up(b)) => self.pad.retain(|p| p != b),
            InputEvent::Pad(_) | InputEvent::Resize { .. } => {}
        }
        true
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
                })
            })
            .collect();
        let (x, y, mut buttons) = self.mouse;
        for button in MouseButton::ALL {
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

/// Whether the room waits for the hello, waits for the start, plays, told
/// the engine to end, or the engine ended.
#[derive(Debug)]
enum Phase {
    Waiting,
    Ready {
        /// The players that the game takes.
        range: PlayerRange,
        /// The ids of the assets that the cache dropped, for the lost that
        /// follows the start.
        lost: Vec<u32>,
    },
    Playing {
        seats: Seats,
        /// The messages for the engine that the host has not taken yet.
        engine_out: Vec<u8>,
        /// The engine did not take the last tick yet.
        tick_pending: bool,
    },
    /// The seats stay, so a view still gets the last frames. A room that
    /// closes before the start has none.
    Closing {
        seats: Seats,
    },
    Over {
        seats: Seats,
    },
}

impl Phase {
    /// The seats of the room, or `None` while it waits for the hello or for
    /// the start.
    fn seats(&mut self) -> Option<&mut Seats> {
        match self {
            Phase::Playing { seats, .. } | Phase::Closing { seats } | Phase::Over { seats } => {
                Some(seats)
            }
            Phase::Waiting | Phase::Ready { .. } => None,
        }
    }

    /// The seats and the messages for the engine, or `None` if the room
    /// does not play.
    fn playing(&mut self) -> Option<(&mut Seats, &mut Vec<u8>)> {
        match self {
            Phase::Playing {
                seats, engine_out, ..
            } => Some((seats, engine_out)),
            Phase::Waiting | Phase::Ready { .. } | Phase::Closing { .. } | Phase::Over { .. } => {
                None
            }
        }
    }
}

/// The view of `conn` and the newest frame of its seat, if `conn` is the
/// connection that takes the seat and has not left.
fn view_of(seats: &mut Seats, conn: Conn) -> Option<(&mut View, Option<&Frame>)> {
    let seat = seats
        .get_mut(&conn.player)
        .filter(|seat| seat.generation == conn.generation)?;
    Some((seat.view.as_mut()?, seat.frame.as_ref()))
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
    use crate::scene::{Bitmap, Image, RotatedRect, Scene};
    use crate::session::{Session, SessionEvent};
    use crate::wire::testing;

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

        /// A room of `nicknames` after the hello and the start, with a
        /// connection to each seat, and with the start already taken.
        fn playing(nicknames: &[&str]) -> (Self, Vec<Conn>) {
            let mut room = Room::new();
            assert!(room.core.from_engine(&hello(1, 9)).is_empty());
            room.core.start(nicknames).unwrap();
            let conns = (1..=nicknames.len() as u32)
                .map(|p| room.core.connect(NonZeroU32::new(p).unwrap()).unwrap())
                .collect();
            room.events();
            (room, conns)
        }

        /// The events that the engine got since the last call, in a short
        /// form, with each lost, which the session keeps to itself. The
        /// tickTaken of the engine goes back to the core.
        fn events(&mut self) -> Vec<String> {
            let mut buf = Vec::new();
            self.core.take_engine_output(&mut buf);
            let mut back = Vec::new();
            let mut events = Vec::new();
            let mut rest = &buf[..];
            while let Some((payload, after)) = framing::split_message(rest, Side::Server).unwrap() {
                let message = rest.get(..rest.len() - after.len()).unwrap();
                if let Ok(Some(server_message::Message::Lost(id))) = server_message::decode(payload)
                {
                    events.push(format!("lost {id}"));
                }
                self.engine.feed(message);
                events.extend(self.session_events(&mut back));
                rest = after;
            }
            assert!(rest.is_empty());
            assert!(self.core.from_engine(&back).is_empty());
            events
        }

        /// The events that wait in the session of the engine, in a short
        /// form. A tickTaken goes to `back`.
        fn session_events(&mut self, back: &mut Vec<u8>) -> Vec<String> {
            std::iter::from_fn(|| self.engine.next_event(back))
                .map(|e| match e {
                    SessionEvent::Start(nicknames) => {
                        let members: Vec<_> = (1..)
                            .zip(nicknames)
                            .map(|(player, nickname)| format!("{player} {nickname}"))
                            .collect();
                        format!("start {}", members.join(", "))
                    }
                    SessionEvent::Tick => "tick".into(),
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
                        InputEvent::Mouse(_) | InputEvent::Pad(_) => {
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
        })
    }

    /// A frame of width `width` for `player`, or for every player, with
    /// its envelope.
    fn frame(player: u32, width: f32) -> Vec<u8> {
        drawing(player, width, &[])
    }

    /// A frame like [`frame`] that draws the bitmaps of `ids`.
    fn drawing(player: u32, width: f32, ids: &[u32]) -> Vec<u8> {
        let mut scene = Scene::new(width, 1.0);
        for &id in ids {
            let rect = RotatedRect {
                cx: 0.0,
                cy: 0.0,
                w: 1.0,
                h: 1.0,
                angle_deg: 0.0,
            };
            scene.add_bitmap(Bitmap::fit(crate::asset::png_image(id, 1), rect));
        }
        let mut out = Vec::new();
        engine_message::write_frame(&mut out, NonZeroU32::new(player), &scene, &Image::width)
            .unwrap();
        out
    }

    /// An asset of a PNG of `side` by `side`, with its envelope.
    fn asset_of(id: u32, side: u32) -> Vec<u8> {
        let mut out = Vec::new();
        engine_message::write_asset(&mut out, id, &crate::asset::png_head(side, side)).unwrap();
        out
    }

    fn asset(id: u32) -> Vec<u8> {
        asset_of(id, 1)
    }

    /// The eight largest assets from `first`, which fill a room.
    fn fill(first: u32) -> Vec<u8> {
        (first..first + 8)
            .flat_map(|id| asset_of(id, 2048))
            .collect()
    }

    /// Everything that waits for the view of `conn`, in a short form, up to
    /// the first `Idle` or `Gone`.
    fn sent(core: &mut ServerCore, conn: Conn) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let next = one(core, conn);
            let last = next == "idle" || next == "gone";
            out.push(next);
            if last {
                return out;
            }
        }
    }

    /// The next message for the view of `conn`, in a short form.
    fn one(core: &mut ServerCore, conn: Conn) -> String {
        let payload = match core.next_for(conn) {
            Next::Send(payload) => payload,
            Next::Idle => return "idle".into(),
            Next::Gone => return "gone".into(),
        };
        let mut framed = framing::header(Side::Engine, payload.len() as u32).to_vec();
        framed.extend_from_slice(&payload);
        match testing::read(&mut &framed[..]).unwrap().unwrap() {
            testing::Message::Asset { id, .. } => format!("asset {id}"),
            testing::Message::Frame { player, scene } => match player {
                Some(p) => format!("frame {p} {}", scene.width()),
                None => format!("frame all {}", scene.width()),
            },
            testing::Message::Hello(_) => "hello".into(),
            testing::Message::Forget(id) => format!("forget {id}"),
            testing::Message::TickTaken => "tick taken".into(),
        }
    }

    fn hello(min: u32, max: u32) -> Vec<u8> {
        let mut out = Vec::new();
        engine_message::write_hello(&mut out, PlayerRange::new(min, max).unwrap()).unwrap();
        out
    }

    fn player(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    #[test]
    fn the_start_takes_the_players_after_the_hello_and_once() {
        let mut room = Room::new();
        assert_eq!(room.core.players(), None);
        assert_eq!(room.core.start(&["Ana"]), Err(StartError::NoHello));
        assert!(room.core.from_engine(&hello(2, 4)).is_empty());
        let range = PlayerRange::new(2, 4).unwrap();
        assert_eq!(room.core.players(), Some(range));
        room.core.tick();
        assert!(room.events().is_empty());
        assert_eq!(
            room.core.start(&["Ana"]),
            Err(StartError::Players { players: 1, range })
        );
        assert_eq!(room.core.start(&["Ana", "Beto"]), Ok(()));
        assert_eq!(
            room.core.start(&["Ana", "Beto"]),
            Err(StartError::PastStart)
        );
        assert_eq!(room.core.players(), None);
        room.core.tick();
        assert_eq!(room.events(), ["start 1 Ana, 2 Beto", "tick"]);
    }

    #[test]
    fn a_view_connects_only_after_the_start() {
        let mut core = ServerCore::new();
        core.from_engine(&hello(1, 1));
        assert!(core.connect(player(1)).is_none());
        core.start(&["Ana"]).unwrap();
        assert!(core.connect(player(1)).is_some());
        assert!(core.connect(player(2)).is_none());
    }

    #[test]
    fn a_first_message_that_is_not_a_hello_ends_the_room() {
        let mut room = Room::new();
        let mut stream = frame(0, 1.0);
        stream.extend_from_slice(&hello(1, 1));
        assert!(matches!(
            room.core.from_engine(&stream)[..],
            [EngineError::NoHello]
        ));
        assert!(room.core.is_over());
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_first_hello_that_does_not_decode_ends_the_room_with_its_error() {
        let mut core = ServerCore::new();
        let mut out = Vec::new();
        let payload = testing::encode_hello(0, 1);
        out.extend_from_slice(&framing::header(Side::Engine, payload.len() as u32));
        out.extend_from_slice(&payload);
        assert!(matches!(
            core.from_engine(&out)[..],
            [
                EngineError::Payload(wire::Error::PlayerRange { min: 0, max: 1 }),
                EngineError::NoHello
            ]
        ));
        assert!(core.is_over());
    }

    #[test]
    fn a_second_hello_is_an_error_and_the_room_goes_on() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = hello(1, 1);
        stream.extend_from_slice(&frame(0, 1.0));
        assert!(matches!(
            room.core.from_engine(&stream)[..],
            [EngineError::SecondHello]
        ));
        assert_eq!(sent(&mut room.core, conns[0]), ["frame all 1", "idle"]);
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_tick_waits_until_the_engine_takes_the_last_one() {
        let (mut room, _) = Room::playing(&["Ana"]);
        room.core.tick();
        room.core.tick();
        assert_eq!(room.events(), ["tick"]);
        room.core.tick();
        assert_eq!(room.events(), ["tick"]);
    }

    #[test]
    fn the_close_drops_what_the_host_has_not_taken_and_nothing_follows() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        room.core.tick();
        room.core.close();
        room.core.close();
        room.core.tick();
        room.core.input(conns[0], &key("a"));
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_room_closed_before_the_hello_never_starts() {
        let mut room = Room::new();
        room.core.close();
        assert!(room.core.from_engine(&hello(1, 1)).is_empty());
        assert_eq!(room.core.start(&["Ana"]), Err(StartError::PastStart));
        room.core.tick();
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
        core.from_engine(&hello(1, 1));
        core.start(&["Ana"]).unwrap();
        let mut buf = b"rest".to_vec();
        assert!(core.take_engine_output(&mut buf));
        assert_eq!(&buf[..4], b"rest");
        assert!(buf.len() > 4);
        let mut again = Vec::new();
        assert!(core.take_engine_output(&mut again));
        assert!(again.is_empty());
    }

    #[test]
    fn the_output_ends_at_the_close_and_at_the_end_of_the_engine() {
        let mut core = ServerCore::new();
        let mut buf = Vec::new();
        assert!(core.take_engine_output(&mut buf));
        core.close();
        assert!(!core.take_engine_output(&mut buf));
        let mut core = ServerCore::new();
        assert!(core.engine_ended().is_none());
        assert!(!core.take_engine_output(&mut buf));
        assert!(buf.is_empty());
    }

    #[test]
    fn the_input_of_a_view_goes_with_its_player() {
        let (mut room, conns) = Room::playing(&["Ana", "Beto"]);
        room.core
            .from_view(conns[1], &view_message::encode_input(&key("b")))
            .unwrap();
        room.core.input(conns[0], &key("a"));
        assert_eq!(room.events(), ["2 key b", "1 key a"]);
    }

    #[test]
    fn a_message_of_a_view_that_is_too_long_or_does_not_decode_is_an_error() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let long = vec![0; MAX_VIEW_BYTES + 1];
        assert!(matches!(
            room.core.from_view(conns[0], &long),
            Err(ViewError::TooLong(_))
        ));
        assert!(matches!(
            room.core.from_view(conns[0], b"junk"),
            Err(ViewError::Payload(_))
        ));
    }

    #[test]
    fn a_message_of_an_unknown_arm_is_dropped() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let unknown =
            testing::with_unknown_view_value(&view_message::encode_input(&key("a")), |m| {
                testing::tag_of(m.get_event().unwrap())
            });
        room.core.from_view(conns[0], &unknown).unwrap();
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_view_gets_the_assets_of_its_newest_frame_then_the_frame() {
        let (mut room, conns) = Room::playing(&["Ana", "Beto"]);
        let core = &mut room.core;
        let mut stream = frame(0, 1.0);
        stream.extend_from_slice(&asset(7));
        stream.extend_from_slice(&asset(8));
        stream.extend_from_slice(&drawing(1, 2.0, &[8]));
        stream.extend_from_slice(&drawing(0, 3.0, &[7]));
        stream.extend_from_slice(&drawing(2, 4.0, &[7, 8]));
        assert!(core.from_engine(&stream).is_empty());
        assert_eq!(sent(core, conns[0]), ["asset 7", "frame all 3", "idle"]);
        assert_eq!(
            sent(core, conns[1]),
            ["asset 7", "asset 8", "frame 2 4", "idle"]
        );
        core.from_engine(&drawing(1, 5.0, &[7]));
        assert_eq!(sent(core, conns[0]), ["frame 1 5", "idle"]);
        assert_eq!(sent(core, conns[1]), ["idle"]);
    }

    #[test]
    fn a_view_that_connects_late_gets_the_assets_and_its_newest_frame() {
        let mut core = ServerCore::new();
        let mut stream = hello(2, 2);
        stream.extend_from_slice(&asset(1));
        stream.extend_from_slice(&drawing(0, 9.0, &[1]));
        core.from_engine(&stream);
        core.start(&["Ana", "Beto"]).unwrap();
        // An asset before the start stays, and a frame is dropped.
        let beto = core.connect(player(2)).unwrap();
        assert_eq!(sent(&mut core, beto), ["idle"]);
        let mut stream = drawing(0, 1.0, &[1]);
        stream.extend_from_slice(&frame(2, 2.0));
        core.from_engine(&stream);
        let ana = core.connect(player(1)).unwrap();
        assert_eq!(sent(&mut core, ana), ["asset 1", "frame all 1", "idle"]);
    }

    #[test]
    fn a_forget_goes_to_a_view_after_the_frame_that_stops_drawing_the_asset() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let core = &mut room.core;
        let mut stream = asset(1);
        stream.extend_from_slice(&drawing(1, 1.0, &[1]));
        core.from_engine(&stream);
        assert_eq!(sent(core, conns[0]), ["asset 1", "frame 1 1", "idle"]);
        let mut stream = asset(2);
        stream.extend_from_slice(&drawing(1, 2.0, &[1, 2]));
        stream.extend_from_slice(&drawing(1, 3.0, &[2]));
        assert!(core.from_engine(&stream).is_empty());
        assert_eq!(
            sent(core, conns[0]),
            ["asset 2", "frame 1 3", "forget 1", "idle"]
        );
        // The asset stays in the room, and comes back with a frame that
        // draws it.
        core.from_engine(&drawing(1, 4.0, &[1]));
        assert_eq!(
            sent(core, conns[0]),
            ["asset 1", "frame 1 4", "forget 2", "idle"]
        );
    }

    #[test]
    fn a_slow_view_gets_a_frame_now_and_then_while_each_frame_brings_an_asset() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let core = &mut room.core;
        let mut got = Vec::new();
        for i in 1..=12 {
            let mut stream = asset(i);
            stream.extend_from_slice(&drawing(1, i as f32, &[i]));
            assert!(core.from_engine(&stream).is_empty());
            // The view takes one message for each frame of the engine.
            got.push(one(core, conns[0]));
        }
        let frames = got.iter().filter(|m| m.starts_with("frame")).count();
        assert!(frames >= 3, "{got:?}");
    }

    #[test]
    fn a_view_that_connects_again_gets_what_its_frame_draws_and_no_forget() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let core = &mut room.core;
        let mut stream = asset(1);
        stream.extend_from_slice(&drawing(1, 1.0, &[1]));
        core.from_engine(&stream);
        assert_eq!(sent(core, conns[0]), ["asset 1", "frame 1 1", "idle"]);
        let mut stream = asset(2);
        stream.extend_from_slice(&drawing(1, 2.0, &[2]));
        core.from_engine(&stream);
        let again = core.connect(player(1)).unwrap();
        assert_eq!(sent(core, again), ["asset 2", "frame 1 2", "idle"]);
        core.from_engine(&drawing(1, 3.0, &[2]));
        assert_eq!(sent(core, again), ["frame 1 3", "idle"]);
    }

    #[test]
    fn a_full_room_drops_the_asset_that_the_frames_used_longest_ago() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = fill(1);
        stream.extend_from_slice(&drawing(1, 1.0, &[3]));
        stream.extend_from_slice(&drawing(1, 2.0, &[]));
        stream.extend_from_slice(&asset(9));
        stream.extend_from_slice(&drawing(1, 3.0, &[9, 3]));
        assert!(room.core.from_engine(&stream).is_empty());
        assert_eq!(room.events(), ["lost 1"]);
        assert_eq!(
            sent(&mut room.core, conns[0]),
            ["asset 3", "asset 9", "frame 1 3", "idle"]
        );
    }

    #[test]
    fn a_frame_keeps_an_old_asset_that_it_draws_beside_a_new_one() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = fill(1);
        stream.extend_from_slice(&drawing(1, 1.0, &[2, 3, 4, 5, 6, 7, 8]));
        stream.extend_from_slice(&asset(9));
        stream.extend_from_slice(&drawing(1, 2.0, &[1, 9]));
        assert!(room.core.from_engine(&stream).is_empty());
        assert_eq!(room.events(), ["lost 2"]);
        assert_eq!(
            sent(&mut room.core, conns[0]),
            ["asset 1", "asset 9", "frame 1 2", "idle"]
        );
    }

    #[test]
    fn an_asset_that_does_not_fit_beside_the_new_ones_is_lost_at_once() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = fill(1);
        stream.extend_from_slice(&asset(9));
        stream.extend_from_slice(&drawing(1, 1.0, &[9]));
        assert!(room.core.from_engine(&stream).is_empty());
        assert_eq!(room.events(), ["lost 9"]);
        // The frame goes without the image.
        assert_eq!(sent(&mut room.core, conns[0]), ["frame 1 1", "idle"]);
    }

    #[test]
    fn a_lost_before_the_start_follows_the_start() {
        let mut room = Room::new();
        let mut stream = hello(1, 1);
        stream.extend_from_slice(&fill(1));
        stream.extend_from_slice(&asset(9));
        assert!(room.core.from_engine(&stream).is_empty());
        assert!(room.events().is_empty());
        room.core.start(&["Ana"]).unwrap();
        assert_eq!(room.events(), ["start 1 Ana", "lost 9"]);
    }

    #[test]
    fn a_bad_asset_and_a_forget_of_the_engine_are_errors_and_the_room_goes_on() {
        let (mut room, _) = Room::playing(&["Ana"]);
        let mut stream = asset_of(1, 2049);
        engine_message::write_asset(&mut stream, 2, b"GIF89a").unwrap();
        stream.extend_from_slice(&asset(3));
        stream.extend_from_slice(&asset(3));
        let forget = engine_message::encode_forget(3);
        stream.extend_from_slice(&framing::header(Side::Engine, forget.len() as u32));
        stream.extend_from_slice(&forget);
        assert!(matches!(
            room.core.from_engine(&stream)[..],
            [
                EngineError::Asset {
                    id: 1,
                    error: ImageError::TooManyPixels { .. }
                },
                EngineError::Asset {
                    id: 2,
                    error: ImageError::Unsupported
                },
                EngineError::LiveId(3),
                EngineError::Forget(3),
            ]
        ));
        assert!(!room.core.is_over());
    }

    #[test]
    fn a_frame_for_a_player_with_no_seat_is_an_error_and_the_room_goes_on() {
        let mut core = ServerCore::new();
        assert!(core.from_engine(&hello(1, 2)).is_empty());
        assert!(core.from_engine(&frame(0, 1.0)).is_empty());
        assert!(matches!(
            core.from_engine(&frame(1, 1.0))[..],
            [EngineError::NoSeat(p)] if p == player(1)
        ));
        core.start(&["Ana"]).unwrap();
        let ana = core.connect(player(1)).unwrap();
        let mut stream = frame(2, 1.0);
        stream.extend_from_slice(&frame(1, 2.0));
        assert!(matches!(
            core.from_engine(&stream)[..],
            [EngineError::NoSeat(p)] if p == player(2)
        ));
        assert_eq!(sent(&mut core, ana), ["frame 1 2", "idle"]);
    }

    #[test]
    fn a_frame_with_a_damaged_scene_is_an_error_before_the_start_too() {
        let mut core = ServerCore::new();
        assert!(core.from_engine(&hello(1, 1)).is_empty());
        // Cut the segment after the root, the message and the frame, so the
        // pointer of the scene points out of it.
        let mut payload = frame(0, 1.0)[framing::HEADER_BYTES..].to_vec();
        payload[4..8].copy_from_slice(&5u32.to_le_bytes());
        payload.truncate(8 + 5 * 8);
        let mut stream = framing::header(Side::Engine, payload.len() as u32).to_vec();
        stream.extend_from_slice(&payload);
        let errors = core.from_engine(&stream);
        assert!(matches!(errors[..], [EngineError::Payload(_)]));
    }

    #[test]
    fn the_messages_come_out_whole_from_bytes_fed_one_at_a_time() {
        let mut core = ServerCore::new();
        let feed = |core: &mut ServerCore, stream: &[u8]| {
            for byte in stream {
                assert!(core.from_engine(std::slice::from_ref(byte)).is_empty());
            }
        };
        feed(&mut core, &hello(1, 1));
        core.start(&["Ana"]).unwrap();
        let ana = core.connect(player(1)).unwrap();
        let mut stream = asset(1);
        stream.extend_from_slice(&drawing(1, 2.0, &[1]));
        feed(&mut core, &stream);
        assert_eq!(sent(&mut core, ana), ["asset 1", "frame 1 2", "idle"]);
    }

    #[test]
    fn the_end_of_the_engine_ends_the_room_after_the_last_frame() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        room.core.tick();
        room.core.from_engine(&frame(0, 1.0));
        assert!(room.core.engine_ended().is_none());
        assert!(room.core.is_over());
        room.core.tick();
        room.core.from_engine(&frame(0, 2.0));
        assert!(room.core.engine_ended().is_none());
        assert!(room.events().is_empty());
        assert_eq!(sent(&mut room.core, conns[0]), ["frame all 1", "gone"]);
        assert_eq!(sent(&mut room.core, conns[0]), ["gone"]);
    }

    #[test]
    fn the_view_of_a_player_who_left_is_gone_and_the_seat_stays() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        room.core.leave(conns[0]);
        room.core.leave(conns[0]);
        room.core.input(conns[0], &key("a"));
        assert!(room.core.from_engine(&frame(1, 1.0)).is_empty());
        assert_eq!(sent(&mut room.core, conns[0]), ["gone"]);
        assert!(room.events().is_empty());
        let again = room.core.connect(player(1)).unwrap();
        assert_eq!(sent(&mut room.core, again), ["frame 1 1", "idle"]);
    }

    #[test]
    fn a_view_that_connects_again_gets_the_assets_and_the_newest_frame_again() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let core = &mut room.core;
        let mut stream = asset(1);
        stream.extend_from_slice(&drawing(1, 2.0, &[1]));
        core.from_engine(&stream);
        assert_eq!(sent(core, conns[0]), ["asset 1", "frame 1 2", "idle"]);
        core.leave(conns[0]);
        let again = core.connect(player(1)).unwrap();
        assert_eq!(sent(core, again), ["asset 1", "frame 1 2", "idle"]);
        core.from_engine(&frame(0, 3.0));
        assert_eq!(sent(core, again), ["frame all 3", "forget 1", "idle"]);
    }

    #[test]
    fn the_old_connection_of_a_seat_gets_nothing_and_changes_nothing() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let again = room.core.connect(player(1)).unwrap();
        room.core.input(conns[0], &key("a"));
        room.core.leave(conns[0]);
        assert_eq!(sent(&mut room.core, conns[0]), ["gone"]);
        assert_eq!(sent(&mut room.core, again), ["idle"]);
        room.core.input(again, &key("b"));
        assert_eq!(room.events(), ["1 key b"]);
    }

    #[test]
    fn a_connect_to_no_seat_or_to_a_room_that_is_over_is_refused() {
        let mut core = ServerCore::new();
        core.from_engine(&hello(1, 1));
        core.start(&["Ana"]).unwrap();
        assert!(core.connect(player(2)).is_none());
        assert!(core.engine_ended().is_none());
        assert!(core.connect(player(1)).is_none());
    }

    #[test]
    fn a_leave_releases_what_the_view_held() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let ana = conns[0];
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
        let right = MouseButtons::default().with(MouseButton::Right);
        assert_eq!(
            room.events(),
            [
                "1 up ArrowRight".to_string(),
                format!("1 mouse up Left 3,0 {right:?}"),
                format!("1 mouse up Right 3,0 {:?}", MouseButtons::default()),
                "1 pad up A".to_string(),
            ]
        );
    }

    #[test]
    fn a_down_of_a_key_past_the_cap_does_not_go_to_the_engine() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        for i in 0..=MAX_HELD_KEYS {
            room.core
                .input(conns[0], &key_as(KeyKind::Down, &format!("k{i}")));
        }
        assert_eq!(room.events().len(), MAX_HELD_KEYS);
        room.core.input(conns[0], &key_as(KeyKind::Up, "k0"));
        let last = format!("k{MAX_HELD_KEYS}");
        room.core.input(conns[0], &key_as(KeyKind::Down, &last));
        assert_eq!(
            room.events(),
            ["1 up k0".to_string(), format!("1 key {last}")]
        );
        room.core.leave(conns[0]);
        assert_eq!(room.events().len(), MAX_HELD_KEYS);
    }

    #[test]
    fn a_connect_releases_what_the_old_view_held_once() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        room.core.input(conns[0], &key_as(KeyKind::Down, "a"));
        room.events();
        let again = room.core.connect(player(1)).unwrap();
        assert_eq!(room.events(), ["1 up a"]);
        room.core.leave(conns[0]);
        room.core.leave(again);
        assert!(room.events().is_empty());
    }

    #[test]
    fn a_message_of_the_engine_that_does_not_decode_is_an_error_and_the_stream_goes_on() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = framing::header(Side::Engine, 8).to_vec();
        stream.extend_from_slice(&[0xff; 8]);
        stream.extend_from_slice(&frame(0, 1.0));
        let errors = room.core.from_engine(&stream);
        assert!(matches!(errors[..], [EngineError::Payload(_)]));
        assert_eq!(sent(&mut room.core, conns[0]), ["frame all 1", "idle"]);
    }

    #[test]
    fn a_header_of_another_side_breaks_the_stream() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = frame(0, 1.0);
        stream.extend_from_slice(b"SIS1\0\0\0\0");
        let errors = room.core.from_engine(&stream);
        assert!(matches!(errors[..], [EngineError::Broken(_)]));
        assert!(room.core.is_over());
        assert_eq!(sent(&mut room.core, conns[0]), ["frame all 1", "gone"]);
    }

    #[test]
    fn the_end_inside_a_message_breaks_the_stream() {
        let mut core = ServerCore::new();
        let stream = hello(1, 1);
        core.from_engine(&stream[..stream.len() - 1]);
        let error = core.engine_ended();
        assert!(
            matches!(error, Some(EngineError::Broken(e)) if e.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert!(core.is_over());
    }
}
