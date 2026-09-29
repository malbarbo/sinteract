//! The messages of the server, in the `ServerMessage` union, which go to
//! the engine.
//!
//! The server starts the session with the players, who stay the same until
//! the end, and passes on the input of each view with its player. A tick
//! of the server paces the engine for every player, a lost says that the
//! server dropped an asset, and the server ends the session with the end
//! of its stream.

use std::io::{self, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::server_message;

use super::Error;
use super::engine_message::MAX_PLAYERS;
use super::event::{read_input_event, write_input_event};
use super::framing::{Side, write_framed};
use super::protocol::decode_root;

/// One message of the server, one variant per arm of `ServerMessage`. The
/// arm `event` is `Input` here, so it does not clash with
/// [`crate::event::Event`]. `Start` and `Tick` are about the whole
/// session.
#[derive(Clone, Debug)]
pub enum Message {
    /// The input of `player`.
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
    /// The nicknames of the players, the first of player 1.
    Start(Vec<String>),
    /// Time for the engine to draw the next frames.
    Tick,
    /// The server dropped the asset of this id.
    Lost(u32),
}

/// Write the input `ev` of `player`.
pub fn write_input(w: &mut impl Write, player: NonZeroU32, ev: &InputEvent) -> io::Result<()> {
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

/// Write the start of the session, with a player for each of
/// `nicknames`, numbered from 1 in their order. More than
/// [`MAX_PLAYERS`] players is [`io::ErrorKind::InvalidInput`].
pub fn write_start<S: AsRef<str>>(w: &mut impl Write, nicknames: &[S]) -> io::Result<()> {
    let Some(len) = u32::try_from(nicknames.len())
        .ok()
        .filter(|&len| len <= MAX_PLAYERS)
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "a start of {} players has more than {MAX_PLAYERS}",
                nicknames.len()
            ),
        ));
    };
    write_framed(w, Side::Server, &start_message(len, nicknames))
}

/// Decode `payload`. `None` for a message or an event of an arm from a
/// newer schema. An event of player 0 is an error.
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
            Ok(Some(Message::Input {
                player: nonzero_player(e.get_player())?,
                event,
            }))
        }
        server_message::Start(s) => {
            let nicknames = s?
                .get_members()?
                .iter()
                .map(|m| Ok(m.get_nickname()?.to_str()?.to_owned()))
                .collect::<Result<_, Error>>()?;
            Ok(Some(Message::Start(nicknames)))
        }
        server_message::Tick(_) => Ok(Some(Message::Tick)),
        server_message::Lost(id) => Ok(Some(Message::Lost(id))),
    }
}

/// `player`, or [`Error::NoPlayer`] if it is 0.
fn nonzero_player(player: u32) -> Result<NonZeroU32, Error> {
    NonZeroU32::new(player).ok_or(Error::NoPlayer)
}

pub(super) fn input_message(player: u32, ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
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

/// A start of the `len` players of `nicknames`.
fn start_message<S: AsRef<str>>(len: u32, nicknames: &[S]) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let start = builder.init_root::<server_message::Builder>().init_start();
    let mut list = start.init_members(len);
    for (i, nickname) in (0..len).zip(nicknames) {
        list.reborrow().get(i).set_nickname(nickname.as_ref());
    }
    builder
}
