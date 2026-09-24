//! The messages from the server to the engine, in the `ServerMessage`
//! union.
//!
//! The server starts the session with the players, passes on the input of
//! each view with its player, and says when a player joins or leaves.
//! The server ends the session with a close.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::server_message;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::framing::{Side, write_framed};
use super::protocol::{ReadError, decode_root, read_next};

/// One message of the server, one variant per arm of `ServerMessage`. The
/// arm `event` is `Input` here, so it does not clash with
/// [`crate::event::Event`]. `Close` and `Start` are about the whole
/// session.
#[derive(Clone, Debug)]
pub enum Message {
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
    Close,
    Start(Roster),
    Join {
        player: NonZeroU32,
        nickname: String,
    },
    Leave {
        player: NonZeroU32,
    },
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

/// Read the next message of the server. Returns `None` at the end of the
/// stream. A message or an event of an arm from a newer schema is skipped,
/// and the next one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<Message>, ReadError> {
    read_next(r, Side::Server, decode)
}

/// Write the input `ev` of `player`.
pub fn write_input(w: &mut impl Write, player: NonZeroU32, ev: &InputEvent) -> io::Result<()> {
    write_framed(w, Side::Server, &input_message(player.get(), ev))
}

/// Write the close of the session.
pub fn write_close(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::Server, &close_message())
}

/// Write the start of the session with `roster`.
pub fn write_start(w: &mut impl Write, roster: &Roster) -> io::Result<()> {
    write_framed(w, Side::Server, &start_message(roster.members()))
}

/// Write that `player` joined the session as `nickname`.
pub fn write_join(w: &mut impl Write, player: NonZeroU32, nickname: &str) -> io::Result<()> {
    write_framed(w, Side::Server, &join_message(player.get(), nickname))
}

/// Write that `player` left the session.
pub fn write_leave(w: &mut impl Write, player: NonZeroU32) -> io::Result<()> {
    write_framed(w, Side::Server, &leave_message(player.get()))
}

/// Decode `payload`. `None` for a message or an event of an arm from a
/// newer schema. An event, a join, a leave or a member of player 0, and a
/// roster that repeats a player, are errors.
pub(super) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
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
            Ok(Some(Message::Input {
                player: nonzero_player(e.get_player())?,
                event,
            }))
        }
        server_message::Close(_) => Ok(Some(Message::Close)),
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
        server_message::Join(j) => {
            let j = j?;
            Ok(Some(Message::Join {
                player: nonzero_player(j.get_player())?,
                nickname: j.get_nickname()?.to_str()?.to_owned(),
            }))
        }
        server_message::Leave(l) => Ok(Some(Message::Leave {
            player: nonzero_player(l?.get_player())?,
        })),
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

fn close_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<server_message::Builder>().init_close();
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

fn join_message(player: u32, nickname: &str) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut join = builder.init_root::<server_message::Builder>().init_join();
    join.set_player(player);
    join.set_nickname(nickname);
    builder
}

fn leave_message(player: u32) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<server_message::Builder>()
        .init_leave()
        .set_player(player);
    builder
}

/// Encode the input `ev` of `player`, with no envelope. A test passes 0,
/// which [`write_input`] cannot.
#[cfg(test)]
pub(crate) fn encode_input(player: u32, ev: &InputEvent) -> Vec<u8> {
    super::finish(input_message(player, ev))
}

/// Encode that `player` joined as `nickname`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_join(player: u32, nickname: &str) -> Vec<u8> {
    super::finish(join_message(player, nickname))
}

/// Encode that `player` left, with no envelope.
#[cfg(test)]
pub(crate) fn encode_leave(player: u32) -> Vec<u8> {
    super::finish(leave_message(player))
}

/// Encode a close, with no envelope.
#[cfg(test)]
pub(crate) fn encode_close() -> Vec<u8> {
    super::finish(close_message())
}
