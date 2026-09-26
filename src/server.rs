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

use crate::asset::{AssetError, Cache, Footprint};
use crate::event::{
    InputEvent, KeyEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent,
    PadButton, PadEvent,
};
use crate::wire;
use crate::wire::framing::{self, Side};
use crate::wire::to_engine::{self, Member, Roster};
use crate::wire::to_server;
use crate::wire::to_view::{self, Arm, PlayerRange};

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
    seats: BTreeMap<NonZeroU32, Seat>,
    phase: Phase,
    /// The messages for the engine that the host has not taken yet.
    to_engine: Vec<u8>,
    /// The bytes of the engine that do not make a whole message yet.
    from_engine: Vec<u8>,
    /// The messages of the live assets, by id, as a view gets them.
    assets: Assets,
    /// Which assets are live, under the limits of the room.
    cache: Cache,
    /// The ids of the assets that the cache dropped before the start, for
    /// the lost that follows the start.
    lost_before_start: Vec<u32>,
    /// The engine did not take the last tick yet.
    tick_pending: bool,
}

type Assets = BTreeMap<u32, Arc<[u8]>>;

/// The connection of a view to the seat of a player. A connect gives the
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
    /// it, as for the errors below.
    Broken(io::Error),
    /// The first message of the engine is not a hello. The room is over.
    NoHello,
    /// A hello came after the first one. The core drops it and goes on.
    SecondHello,
    /// The asset `id` is not a PNG or is larger than an image can be. The
    /// core drops it and goes on.
    Asset { id: u32, error: AssetError },
    /// An asset came for an id that names a live asset. The core drops it
    /// and goes on.
    LiveId(u32),
    /// A forget came from the engine, which never sends one. The core drops
    /// it and goes on.
    Forget(u32),
}

/// Why [`ServerCore::start`] did not start the room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// The engine has not said its hello yet.
    NoHello,
    /// The game does not take that many players.
    Players { players: usize, takes: PlayerRange },
    /// The room started, closed or ended before.
    PastStart,
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::NoHello => f.write_str("the engine has not said its hello"),
            StartError::Players { players, takes } => write!(
                f,
                "the game takes from {} to {} players, not {players}",
                takes.min(),
                takes.max()
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
            EngineError::LiveId(id) => write!(f, "an asset came for the live id {id}"),
            EngineError::Forget(id) => write!(f, "a forget of {id} came from the engine"),
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
            | EngineError::Forget(_) => None,
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
/// nicknames and the messages of the views have a cap.
const UNDER_THE_CAP: &str = "a message for the engine is under the cap of the framing";

impl ServerCore {
    pub fn new() -> Self {
        ServerCore {
            seats: BTreeMap::new(),
            phase: Phase::Waiting,
            to_engine: Vec::new(),
            from_engine: Vec::new(),
            assets: Assets::new(),
            cache: Cache::new(),
            lost_before_start: Vec::new(),
            tick_pending: false,
        }
    }

    /// The players that the game takes, from the hello of the engine, or
    /// `None` before the hello.
    pub fn players(&self) -> Option<PlayerRange> {
        match self.phase {
            Phase::Ready(takes) => Some(takes),
            Phase::Waiting | Phase::Playing | Phase::Closing | Phase::Over => None,
        }
    }

    /// Start the room with a player for each of `nicknames`, numbered from
    /// 1 in their order, and give the engine the start. A nickname loses
    /// its control characters, so it cannot move the cursor of a terminal
    /// that prints it, and is cut to 64 bytes.
    pub fn start<S: AsRef<str>>(&mut self, nicknames: &[S]) -> Result<(), StartError> {
        let takes = match self.phase {
            Phase::Ready(takes) => takes,
            Phase::Waiting => return Err(StartError::NoHello),
            Phase::Playing | Phase::Closing | Phase::Over => return Err(StartError::PastStart),
        };
        if !takes.contains(nicknames.len()) {
            return Err(StartError::Players {
                players: nicknames.len(),
                takes,
            });
        }
        self.seats = (1..)
            .map_while(NonZeroU32::new)
            .zip(nicknames)
            .map(|(player, nickname)| (player, Seat::new(clean_nickname(nickname.as_ref()))))
            .collect();
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
        self.phase = Phase::Playing;
        for id in std::mem::take(&mut self.lost_before_start) {
            self.lose(id);
        }
        Ok(())
    }

    /// Say that the WebSocket of `conn` closed. The seat stays, and its
    /// input and frames stop until a connect. The engine gets an `Up` for
    /// each key and button that the view held. A second call, and a call
    /// for an old connection of the seat, do nothing.
    pub fn leave(&mut self, conn: Conn) {
        if seat_of(&mut self.seats, conn).is_none() {
            return;
        }
        self.release_held(conn.player);
        new_generation(&mut self.seats, conn.player);
    }

    /// A connection of a view to the seat of `player`, or `None` if the
    /// room has no such seat, as before the start, or is over. The host checks the token of the
    /// player first. The old connection of the seat gets [`Next::Gone`],
    /// and the new one starts with no asset and gets the newest frame. The
    /// engine gets an `Up` for each key and button that the old view held,
    /// since the old view may never have left.
    pub fn connect(&mut self, player: NonZeroU32) -> Option<Conn> {
        if self.phase == Phase::Over {
            return None;
        }
        self.release_held(player);
        let seat = new_generation(&mut self.seats, player)?;
        seat.frame_sent = false;
        seat.target = None;
        seat.has.clear();
        seat.shown.clear();
        Some(Conn {
            player,
            generation: seat.generation,
        })
    }

    /// Tell the engine to draw the next frames, in the game. The tick is
    /// dropped while the engine has not taken the last one, so the ticks
    /// of an engine slower than the timer do not pile up.
    pub fn tick(&mut self) {
        if self.phase == Phase::Playing && !self.tick_pending {
            to_engine::write_tick(&mut self.to_engine).expect(UNDER_THE_CAP);
            self.tick_pending = true;
        }
    }

    /// Stop writing to the engine, and drop what the host has not taken.
    /// The host then closes the pipe of the engine, and the end of the pipe
    /// tells the engine to end. The engine may still send its last frames.
    pub fn close(&mut self) {
        match self.phase {
            Phase::Waiting | Phase::Ready(_) | Phase::Playing => {
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
    /// tick of the server paces the engine, so a tick of the view is
    /// dropped. The input
    /// of an old connection is dropped. The core drops a `Down` of a new
    /// key when the view holds 32 keys, since it could not release it.
    pub fn input(&mut self, conn: Conn, event: &InputEvent) {
        let Some(seat) = seat_of(&mut self.seats, conn) else {
            return;
        };
        if self.phase != Phase::Playing || matches!(event, InputEvent::Tick) {
            return;
        }
        if !seat.held.track(event) {
            return;
        }
        to_engine::write_input(&mut self.to_engine, conn.player, event).expect(UNDER_THE_CAP);
    }

    /// Take the next bytes of the engine. They may end anywhere, inside a
    /// message too. The first message is the hello. Then the core keeps an
    /// asset under the limits of the room, and drops the assets that the
    /// frames used longest ago to fit it, with a lost to the engine for
    /// each. A frame goes to its player, or to every player, with the
    /// assets that it draws. Before the start a frame has no player, and is
    /// dropped. A broken stream ends the room, and the core ignores what
    /// comes after the end. Returns what went wrong, in the order of the
    /// stream.
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
            if self.phase == Phase::Waiting && !matches!(arm, Ok(Some(Arm::Hello(_)))) {
                errors.push(EngineError::NoHello);
                self.end();
                return errors;
            }
            match arm {
                Ok(Some(Arm::Hello(takes))) => match self.phase {
                    Phase::Waiting => self.phase = Phase::Ready(takes),
                    Phase::Ready(_) | Phase::Playing => errors.push(EngineError::SecondHello),
                    // A room that closed before the hello never starts.
                    Phase::Closing | Phase::Over => {}
                },
                Ok(Some(Arm::Asset { id, size, bytes })) => {
                    if let Err(e) = self.keep_asset(id, size, bytes, payload) {
                        errors.push(e);
                    }
                }
                Ok(Some(Arm::Forget(id))) => errors.push(EngineError::Forget(id)),
                Ok(Some(Arm::TickTaken)) => self.tick_pending = false,
                Ok(Some(Arm::Frame { player })) => {
                    if let Err(e) = self.keep_frame(player, payload) {
                        errors.push(EngineError::Payload(e));
                    }
                }
                Ok(None) => {}
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
        let Some(seat) = seat_of(&mut self.seats, conn) else {
            return Next::Gone;
        };
        if seat.target.is_none() && !seat.frame_sent {
            seat.target = seat.frame.clone();
            seat.frame_sent = true;
        }
        let needs = |id: &u32| {
            seat.shown.contains(id)
                || seat
                    .target
                    .as_ref()
                    .is_some_and(|t| t.assets.contains_key(id))
        };
        if let Some(&id) = seat.has.iter().find(|id| !needs(id)) {
            seat.has.remove(&id);
            return Next::Send(to_view::encode_forget(id).into());
        }
        if let Some(target) = seat.target.take() {
            if let Some((&id, asset)) = target.assets.iter().find(|(id, _)| !seat.has.contains(id))
            {
                seat.has.insert(id);
                let payload = asset.clone();
                seat.target = Some(target);
                return Next::Send(payload);
            }
            seat.shown = target.assets.keys().copied().collect();
            return Next::Send(target.frame);
        }
        match self.phase {
            Phase::Waiting | Phase::Ready(_) | Phase::Playing | Phase::Closing => Next::Idle,
            Phase::Over => Next::Gone,
        }
    }

    /// Move the messages for the engine to the end of `buf`. A host that
    /// cannot write all of them keeps the rest in `buf` for the next write.
    pub fn take_engine_output(&mut self, buf: &mut Vec<u8>) {
        buf.append(&mut self.to_engine);
    }

    /// Keep the asset `id`, and drop the ones that it takes the place of,
    /// or refuse it if the id is live or the image is too large. An asset
    /// that does not fit beside the ones on the screens is lost at once.
    fn keep_asset(
        &mut self,
        id: u32,
        size: Option<(u32, u32)>,
        bytes: usize,
        payload: Arc<[u8]>,
    ) -> Result<(), EngineError> {
        if self.cache.contains(id) {
            return Err(EngineError::LiveId(id));
        }
        let footprint =
            Footprint::new(size, bytes).map_err(|error| EngineError::Asset { id, error })?;
        match self.cache.asset(id, footprint) {
            Ok(dropped) => {
                self.assets.insert(id, payload);
                for gone in dropped {
                    self.assets.remove(&gone);
                    self.lose(gone);
                }
            }
            Err(_) => self.lose(id),
        }
        Ok(())
    }

    /// Keep the frame in `payload` for `player`, or for every player when
    /// `player` is `None`, with the assets that it draws. A frame for no
    /// seat reaches no screen, so it changes nothing.
    fn keep_frame(
        &mut self,
        player: Option<NonZeroU32>,
        payload: Arc<[u8]>,
    ) -> Result<(), wire::Error> {
        let seats = match player {
            None => self.seats.len(),
            Some(player) => usize::from(self.seats.contains_key(&player)),
        };
        if seats == 0 {
            return Ok(());
        }
        let ids = to_view::bitmap_ids(&payload)?;
        let assets: Assets = ids
            .iter()
            .filter_map(|id| Some((*id, self.assets.get(id)?.clone())))
            .collect();
        self.cache.frame(player, ids);
        let shot = Shot {
            frame: payload,
            assets: Arc::new(assets),
        };
        for (_, seat) in self
            .seats
            .iter_mut()
            .filter(|(p, _)| player.is_none_or(|player| **p == player))
        {
            seat.frame = Some(shot.clone());
            seat.frame_sent = false;
        }
        Ok(())
    }

    /// Tell the engine that the asset `id` is gone. Before the start, the
    /// lost waits for the start, which comes first.
    fn lose(&mut self, id: u32) {
        match self.phase {
            Phase::Playing => {
                to_engine::write_lost(&mut self.to_engine, id).expect(UNDER_THE_CAP);
            }
            Phase::Waiting | Phase::Ready(_) => self.lost_before_start.push(id),
            Phase::Closing | Phase::Over => {}
        }
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
    /// connect move it on, so no older connection matches.
    generation: u32,
    /// What the view holds down, as the engine saw it.
    held: Held,
    /// The newest frame for the player, kept for the next connect.
    frame: Option<Shot>,
    /// Whether the view got `frame`, or waits for it as `target`.
    frame_sent: bool,
    /// The next frame of the view, until the view has its assets and it.
    target: Option<Shot>,
    /// The ids of the assets that the view has.
    has: BTreeSet<u32>,
    /// The ids of the assets of the frame on the screen of the view.
    shown: BTreeSet<u32>,
}

/// A frame, with the assets that it draws, as they were at its arrival. The
/// frame keeps them for a view that has not got them, after the cache drops
/// them.
#[derive(Clone, Debug)]
struct Shot {
    frame: Arc<[u8]>,
    assets: Arc<Assets>,
}

impl Seat {
    /// A seat with no view yet. No connection has generation 0.
    fn new(nickname: String) -> Seat {
        Seat {
            nickname,
            generation: 0,
            held: Held::default(),
            frame: None,
            frame_sent: false,
            target: None,
            has: BTreeSet::new(),
            shown: BTreeSet::new(),
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
            InputEvent::Pad(_) | InputEvent::Resize { .. } | InputEvent::Tick => {}
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

/// Whether the room waits for the hello, waits for the start with the
/// players that the game takes, plays, told the engine to end, or the
/// engine ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Waiting,
    Ready(PlayerRange),
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
    use crate::scene::{Bitmap, Scene};
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
        /// form. The tickTaken of the engine goes back to the core.
        fn events(&mut self) -> Vec<String> {
            let mut buf = Vec::new();
            self.core.take_engine_output(&mut buf);
            self.engine.feed(&buf);
            let mut back = Vec::new();
            let events = std::iter::from_fn(|| self.engine.next_event(&mut back).unwrap())
                .map(|e| match e {
                    SessionEvent::Start(roster) => {
                        let members: Vec<_> = roster
                            .members()
                            .iter()
                            .map(|m| format!("{} {}", m.player, m.nickname))
                            .collect();
                        format!("start {}", members.join(", "))
                    }
                    SessionEvent::Tick => "tick".into(),
                    SessionEvent::Lost(id) => format!("lost {id}"),
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
                        InputEvent::Mouse(_) | InputEvent::Tick | InputEvent::Pad(_) => {
                            format!("{player} {event:?}")
                        }
                    },
                    SessionEvent::Error(e) => format!("error {e}"),
                    SessionEvent::End(None) => "end".into(),
                    SessionEvent::End(Some(e)) => format!("broken {e}"),
                })
                .collect();
            assert!(self.core.from_engine(&back).is_empty());
            events
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

    /// A frame of width `width` for `player`, or for every player, with
    /// its envelope.
    fn frame(player: u32, width: f32) -> Vec<u8> {
        drawing(player, width, &[])
    }

    /// A frame like [`frame`] that draws the bitmaps of `ids`.
    fn drawing(player: u32, width: f32, ids: &[u32]) -> Vec<u8> {
        let mut scene = Scene::new(width, 1.0);
        for &id in ids {
            scene.bitmap(Bitmap {
                id,
                ..Bitmap::default()
            });
        }
        let mut out = Vec::new();
        to_view::write_frame(&mut out, NonZeroU32::new(player), &scene).unwrap();
        out
    }

    /// An asset of a PNG of `side` by `side`, with its envelope.
    fn asset_of(id: u32, side: u32) -> Vec<u8> {
        let mut out = Vec::new();
        to_view::write_asset(&mut out, id, &crate::asset::png_head(side, side)).unwrap();
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
        match to_view::read(&mut &framed[..]).unwrap().unwrap() {
            to_view::Message::Asset { id, .. } => format!("asset {id}"),
            to_view::Message::Frame { player, scene } => match player {
                Some(p) => format!("frame {p} {}", scene.width()),
                None => format!("frame all {}", scene.width()),
            },
            to_view::Message::Hello(_) => "hello".into(),
            to_view::Message::Forget(id) => format!("forget {id}"),
            to_view::Message::TickTaken => "tick taken".into(),
        }
    }

    fn hello(min: u32, max: u32) -> Vec<u8> {
        let mut out = Vec::new();
        to_view::write_hello(&mut out, PlayerRange::new(min, max).unwrap()).unwrap();
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
        let takes = PlayerRange::new(2, 4).unwrap();
        assert_eq!(room.core.players(), Some(takes));
        room.core.tick();
        assert!(room.events().is_empty());
        assert_eq!(
            room.core.start(&["Ana"]),
            Err(StartError::Players { players: 1, takes })
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
        core.take_engine_output(&mut buf);
        assert_eq!(&buf[..4], b"rest");
        assert!(buf.len() > 4);
        let mut again = Vec::new();
        core.take_engine_output(&mut again);
        assert!(again.is_empty());
    }

    #[test]
    fn the_input_of_a_view_goes_with_its_player() {
        let (mut room, conns) = Room::playing(&["Ana", "Beto"]);
        room.core
            .from_view(conns[1], &to_server::encode_input(&key("b")))
            .unwrap();
        room.core.input(conns[0], &key("a"));
        room.core.input(conns[0], &InputEvent::Tick);
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
        let unknown = wire::with_unknown_view_value(&to_server::encode_input(&key("a")), |m| {
            wire::tag_of(m.get_event().unwrap())
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
    fn an_asset_that_does_not_fit_beside_the_screens_is_lost_at_once() {
        let (mut room, conns) = Room::playing(&["Ana"]);
        let mut stream = fill(1);
        stream.extend_from_slice(&drawing(1, 1.0, &[1, 2, 3, 4, 5, 6, 7, 8]));
        stream.extend_from_slice(&asset(9));
        stream.extend_from_slice(&drawing(1, 2.0, &[9]));
        assert!(room.core.from_engine(&stream).is_empty());
        assert_eq!(room.events(), ["lost 9"]);
        // The frame goes without the image.
        assert_eq!(sent(&mut room.core, conns[0]), ["frame 1 2", "idle"]);
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
        to_view::write_asset(&mut stream, 2, b"GIF89a").unwrap();
        stream.extend_from_slice(&asset(3));
        stream.extend_from_slice(&asset(3));
        let forget = to_view::encode_forget(3);
        stream.extend_from_slice(&framing::header(Side::Engine, forget.len() as u32));
        stream.extend_from_slice(&forget);
        assert!(matches!(
            room.core.from_engine(&stream)[..],
            [
                EngineError::Asset {
                    id: 1,
                    error: AssetError::TooManyPixels { .. }
                },
                EngineError::Asset {
                    id: 2,
                    error: AssetError::Unsupported
                },
                EngineError::LiveId(3),
                EngineError::Forget(3),
            ]
        ));
        assert!(!room.core.is_over());
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
