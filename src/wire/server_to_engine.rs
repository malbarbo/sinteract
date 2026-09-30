//! The messages of the server, which go to the engine.
//!
//! The server starts the session with the players and the rate of the
//! ticks, which stay the same until the end, in a message whose root is a
//! `Start`. Every message after it is
//! a `ServerToEngine`. The server passes on the input of each view with its player. A tick
//! of the server paces the engine for every player, a lost says that the
//! server dropped an asset, and the server ends the session with the end
//! of its stream.

use std::io::{self, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::event::InputEvent;
use crate::protocol_capnp::{server_to_engine, start};
use crate::session::MAX_PLAYERS;

use super::Error;
use super::event::{read_input_event, write_input_event};
use super::framing::{Side, write_framed};
use super::protocol::decode_root;

/// One message of the server, one variant per arm of `ServerToEngine`. The
/// arm `event` is `Input` here, so it does not clash with
/// [`crate::event::Event`]. `Tick` is about the whole session.
#[derive(Clone, Debug)]
pub enum Message {
    /// The input of `player`.
    Input {
        player: NonZeroU32,
        event: InputEvent,
    },
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
/// `nicknames`, numbered from 1 in their order, and ticks at `tick_rate`
/// thousandths of a hertz. More than [`MAX_PLAYERS`] players is
/// [`io::ErrorKind::InvalidInput`].
pub fn write_start<S: AsRef<str>>(
    w: &mut impl Write,
    nicknames: &[S],
    tick_rate: NonZeroU32,
) -> io::Result<()> {
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
    write_framed(
        w,
        Side::Server,
        &start_message(len, nicknames, tick_rate.get()),
    )
}

/// Decode `payload`. `None` for a message or an event of an arm from a
/// newer schema. An event of player 0 is an error.
pub(crate) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<server_to_engine::Owned, _>(payload, decode_message)
}

fn decode_message(msg: server_to_engine::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        server_to_engine::Input(e) => {
            let e = e?;
            let Some(event) = read_input_event(e.get_event()?)? else {
                return Ok(None);
            };
            Ok(Some(Message::Input {
                player: nonzero_player(e.get_player())?,
                event,
            }))
        }
        server_to_engine::Tick(_) => Ok(Some(Message::Tick)),
        server_to_engine::Lost(id) => Ok(Some(Message::Lost(id))),
    }
}

/// The nicknames of the players of the start in `payload`, a message with
/// no envelope, the first of player 1, and the rate of its ticks. A rate
/// of 0 is [`Error::NoTickRate`].
pub(crate) fn read_start(payload: &[u8]) -> Result<(Vec<String>, NonZeroU32), Error> {
    decode_root::<start::Owned, _>(payload, |s| {
        let nicknames = s
            .get_members()?
            .iter()
            .map(|m| Ok(m.get_nickname()?.to_str()?.to_owned()))
            .collect::<Result<_, Error>>()?;
        let tick_rate = NonZeroU32::new(s.get_tick_rate()).ok_or(Error::NoTickRate)?;
        Ok((nicknames, tick_rate))
    })
}

/// `player`, or [`Error::NoPlayer`] if it is 0.
fn nonzero_player(player: u32) -> Result<NonZeroU32, Error> {
    NonZeroU32::new(player).ok_or(Error::NoPlayer)
}

pub(super) fn input_message(player: u32, ev: &InputEvent) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut input = builder
        .init_root::<server_to_engine::Builder>()
        .init_input();
    input.set_player(player);
    write_input_event(input.init_event(), ev);
    builder
}

fn tick_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder.init_root::<server_to_engine::Builder>().init_tick();
    builder
}

fn lost_message(id: u32) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<server_to_engine::Builder>()
        .set_lost(id);
    builder
}

/// A start of the `len` players of `nicknames`, with ticks at
/// `tick_rate`.
pub(super) fn start_message<S: AsRef<str>>(
    len: u32,
    nicknames: &[S],
    tick_rate: u32,
) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut start = builder.init_root::<start::Builder>();
    start.set_tick_rate(tick_rate);
    let mut list = start.init_members(len);
    for (i, nickname) in (0..len).zip(nicknames) {
        list.reborrow().get(i).set_nickname(nickname.as_ref());
    }
    builder
}
