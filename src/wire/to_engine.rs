//! The messages from the server to the engine, in the `ServerMessage`
//! union.
//!
//! The server starts the session with the players, who stay the same until
//! the end, and passes on the input of each view with its player. A tick
//! of the server paces the engine for every player, a lost says that the
//! server dropped an asset, and the server ends the session with the end
//! of its stream.

use std::collections::HashSet;
use std::io::{self, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::server_message;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::framing::{Side, write_framed};
use super::protocol::decode_root;

/// One message of the server, one variant per arm of `ServerMessage`. The
/// arm `event` is `Input` here, so it does not clash with
/// [`crate::event::Event`]. `Start` and `Tick` are about the whole
/// session.
#[derive(Clone, Debug)]
pub enum Message {
    /// The input of `player`. The server sends every tick as
    /// [`Message::Tick`], so `event` is never [`InputEvent::Tick`].
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
    Start(Roster),
    /// Time for the engine to draw the next frames.
    Tick,
    /// The server dropped the asset of this id.
    Lost(u32),
}

/// A player of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The number of the player in the session.
    pub player: NonZeroU32,
    pub nickname: String,
}

/// The players at the start of the session, each player once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roster(Vec<Member>);

/// Two members of a [`Roster`] have the same player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicatePlayer(pub NonZeroU32);

impl Roster {
    /// The roster of `members`, or the first player that repeats.
    pub fn new(members: Vec<Member>) -> Result<Roster, DuplicatePlayer> {
        let mut seen = HashSet::with_capacity(members.len());
        match members.iter().find(|m| !seen.insert(m.player)) {
            Some(m) => Err(DuplicatePlayer(m.player)),
            None => Ok(Roster(members)),
        }
    }

    pub fn members(&self) -> &[Member] {
        &self.0
    }
}

impl std::fmt::Display for DuplicatePlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "player {} is in the roster twice", self.0)
    }
}

impl std::error::Error for DuplicatePlayer {}

impl From<DuplicatePlayer> for Error {
    fn from(e: DuplicatePlayer) -> Self {
        Error::DuplicatePlayer(e)
    }
}

/// Write the input `ev` of `player`. A tick is not written, and the error
/// is [`io::ErrorKind::InvalidInput`], since the server sends every tick
/// with [`write_tick`].
pub fn write_input(w: &mut impl Write, player: NonZeroU32, ev: &InputEvent) -> io::Result<()> {
    if matches!(ev, InputEvent::Tick) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a tick goes in a message of its own, not in the input of a player",
        ));
    }
    write_framed(w, Side::Server, &input_message(player.get(), ev))
}

/// Write a tick, for every player.
pub fn write_tick(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::Server, &tick_message())
}

/// Write that the server dropped the asset `id`.
pub fn write_lost(w: &mut impl Write, id: u32) -> io::Result<()> {
    write_framed(w, Side::Server, &lost_message(id))
}

/// Write the start of the session with `roster`.
pub fn write_start(w: &mut impl Write, roster: &Roster) -> io::Result<()> {
    write_framed(w, Side::Server, &start_message(roster.members()))
}

/// Decode `payload`. `None` for a message or an event of an arm from a
/// newer schema. An event or a member of player 0, an event that is a
/// tick, and a roster that repeats a player, are errors.
pub(crate) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<server_message::Owned, _>(payload, decode_message)
}

fn decode_message(msg: server_message::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        server_message::Event(e) => {
            let e = e?;
            let Some(event) = read_input_event(e.get_event()?)? else {
                return Ok(None);
            };
            if matches!(event, InputEvent::Tick) {
                return Err(Error::PlayerTick);
            }
            Ok(Some(Message::Input {
                player: nonzero_player(e.get_player())?,
                event,
            }))
        }
        server_message::Start(s) => {
            let members = s?
                .get_members()?
                .iter()
                .map(|m| {
                    Ok(Member {
                        player: nonzero_player(m.get_player())?,
                        nickname: m.get_nickname()?.to_str()?.to_owned(),
                    })
                })
                .collect::<Result<_, Error>>()?;
            Ok(Some(Message::Start(Roster::new(members)?)))
        }
        server_message::Tick(_) => Ok(Some(Message::Tick)),
        server_message::Lost(id) => Ok(Some(Message::Lost(id))),
    }
}

/// `player`, or [`Error::NoPlayer`] if it is 0.
fn nonzero_player(player: u32) -> Result<NonZeroU32, Error> {
    NonZeroU32::new(player).ok_or(Error::NoPlayer)
}

fn input_message(player: u32, ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut event = builder.init_root::<server_message::Builder>().init_event();
    event.set_player(player);
    write_input_event(event.init_event(), ev);
    builder
}

fn tick_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<server_message::Builder>().init_tick();
    builder
}

fn lost_message(id: u32) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<server_message::Builder>().set_lost(id);
    builder
}

fn start_message(members: &[Member]) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let start = builder.init_root::<server_message::Builder>().init_start();
    let len = u32::try_from(members.len()).expect("fewer than 2^32 players");
    let mut list = start.init_members(len);
    for (i, member) in (0..len).zip(members) {
        let mut m = list.reborrow().get(i);
        m.set_player(member.player.get());
        m.set_nickname(&*member.nickname);
    }
    builder
}

/// Encode the input `ev` of `player`, with no envelope. A test passes 0,
/// which [`write_input`] cannot.
#[cfg(test)]
pub(crate) fn encode_input(player: u32, ev: &InputEvent) -> Vec<u8> {
    super::finish(input_message(player, ev))
}
