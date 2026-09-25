//! The messages from the engine to the view, in the `EngineMessage` union.
//!
//! The engine says first how many players the game takes, in a hello for
//! the server. Then it uploads each bitmap as an asset, before the first
//! frame that draws it, and sends a frame per repaint, for one player or
//! for all of them. The engine ends the session with the end of its
//! stream. The server adds a forget for a view, when the view no longer
//! needs an asset.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::asset::png_size;
use crate::protocol_capnp::{engine_message, hello};
use crate::scene::Scene;

use super::Error;
use super::framing::{Side, write_framed};
use super::protocol::{ReadError, decode_root, read_next};
use super::scene::{read_bitmap_ids, read_scene, write_scene};

/// One message of the engine, one variant per arm of `EngineMessage`. An
/// asset goes to every player.
#[derive(Clone, Debug)]
pub enum Message {
    Asset {
        id: u32,
        blob: Vec<u8>,
        mime: Option<String>,
    },
    /// A frame for `player`, or for every player when `player` is `None`.
    Frame {
        player: Option<NonZeroU32>,
        scene: Scene,
    },
    Hello(PlayerRange),
    /// The view drops the asset of this id. Only a server sends it.
    Forget(u32),
}

/// The arm of a message of the engine, with the player of a frame. A
/// server routes a message by its arm and passes the payload on as it
/// came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// An asset, with what the limits of [`crate::asset`] count.
    Asset {
        id: u32,
        /// The size from the header of the PNG, or `None` if the blob is
        /// not a PNG.
        size: Option<(u32, u32)>,
        /// The length of the blob.
        bytes: usize,
    },
    /// A frame for `player`, or for every player when `player` is `None`.
    Frame {
        player: Option<NonZeroU32>,
    },
    Hello(PlayerRange),
    Forget(u32),
}

/// The fewest and the most players that a game takes, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerRange {
    min: NonZeroU32,
    max: NonZeroU32,
}

impl PlayerRange {
    /// The range from `min` to `max`, or `None` if `min` is 0 or above
    /// `max`.
    pub fn new(min: u32, max: u32) -> Option<PlayerRange> {
        let min = NonZeroU32::new(min)?;
        let max = NonZeroU32::new(max).filter(|&max| max >= min)?;
        Some(PlayerRange { min, max })
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

/// Read the next message of the engine. Returns `None` at the end of the
/// stream. A message of an arm from a newer schema is skipped, and the next
/// one comes out.
pub fn read(r: &mut impl Read) -> Result<Option<Message>, ReadError> {
    read_next(r, Side::Engine, decode)
}

/// Write a scene as a frame for `player`, or for every player when
/// `player` is `None`.
pub fn write_frame(
    w: &mut impl Write,
    player: Option<NonZeroU32>,
    scene: &Scene,
) -> io::Result<()> {
    write_framed(w, Side::Engine, &frame_message(player, scene))
}

/// Write the hello, the first message of the engine.
pub fn write_hello(w: &mut impl Write, players: PlayerRange) -> io::Result<()> {
    write_framed(w, Side::Engine, &hello_message(players))
}

/// Write a bitmap upload as an asset.
pub fn write_asset(w: &mut impl Write, id: u32, blob: &[u8], mime: Option<&str>) -> io::Result<()> {
    write_framed(w, Side::Engine, &asset_message(id, blob, mime))
}

/// The arm of `payload`, a message with no envelope, without a decode of
/// the scene of a frame or of the image of an asset. `None` for an arm from a newer schema. A hello
/// whose players are not a [`PlayerRange`] is an error.
pub fn arm(payload: &[u8]) -> Result<Option<Arm>, Error> {
    decode_root::<engine_message::Owned, _>(payload, |msg| {
        let Ok(which) = msg.which() else {
            return Ok(None);
        };
        Ok(Some(match which {
            engine_message::Asset(a) => {
                let a = a?;
                let blob = a.get_blob()?;
                Arm::Asset {
                    id: a.get_id(),
                    size: png_size(blob),
                    bytes: blob.len(),
                }
            }
            engine_message::Frame(f) => Arm::Frame {
                player: NonZeroU32::new(f?.get_player()),
            },
            engine_message::Hello(h) => Arm::Hello(read_hello(h?)?),
            engine_message::Forget(id) => Arm::Forget(id),
        }))
    })
}

/// The ids of the bitmaps that the frame in `payload`, a message with no
/// envelope, draws, with no decode of the rest of the scene. A message that
/// is not a frame draws none.
pub fn bitmap_ids(payload: &[u8]) -> Result<BTreeSet<u32>, Error> {
    decode_root::<engine_message::Owned, _>(payload, |msg| {
        let mut ids = BTreeSet::new();
        if let Ok(engine_message::Frame(f)) = msg.which() {
            read_bitmap_ids(f?.get_scene()?, &mut ids)?;
        }
        Ok(Some(ids))
    })
    .map(|ids| ids.unwrap_or_default())
}

/// Decode `payload`. `None` for a message of an arm from a newer schema.
pub(super) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<engine_message::Owned, _>(payload, decode_message)
}

fn decode_message(msg: engine_message::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        engine_message::Asset(a) => {
            let a = a?;
            let blob = a.get_blob()?.to_vec();
            let mime = match a.get_mime() {
                Ok(t) => {
                    let s = t.to_str()?.to_owned();
                    if s.is_empty() { None } else { Some(s) }
                }
                Err(_) => None,
            };
            Ok(Some(Message::Asset {
                id: a.get_id(),
                blob,
                mime,
            }))
        }
        engine_message::Frame(f) => {
            let f = f?;
            Ok(Some(Message::Frame {
                player: NonZeroU32::new(f.get_player()),
                scene: read_scene(f.get_scene()?)?,
            }))
        }
        engine_message::Hello(h) => Ok(Some(Message::Hello(read_hello(h?)?))),
        engine_message::Forget(id) => Ok(Some(Message::Forget(id))),
    }
}

fn read_hello(h: hello::Reader<'_>) -> Result<PlayerRange, Error> {
    let (min, max) = (h.get_min_players(), h.get_max_players());
    PlayerRange::new(min, max).ok_or(Error::PlayerRange { min, max })
}

fn frame_message(player: Option<NonZeroU32>, scene: &Scene) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut frame = builder.init_root::<engine_message::Builder>().init_frame();
    frame.set_player(player.map_or(0, NonZeroU32::get));
    write_scene(frame.init_scene(), scene);
    builder
}

fn hello_message(players: PlayerRange) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut hello = builder.init_root::<engine_message::Builder>().init_hello();
    hello.set_min_players(players.min.get());
    hello.set_max_players(players.max.get());
    builder
}

fn forget_message(id: u32) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<engine_message::Builder>()
        .set_forget(id);
    builder
}

fn asset_message(id: u32, blob: &[u8], mime: Option<&str>) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut asset = builder.init_root::<engine_message::Builder>().init_asset();
    asset.set_id(id);
    asset.set_blob(blob);
    if let Some(m) = mime {
        asset.set_mime(m);
    }
    builder
}

/// Encode that the asset `id` is gone, with no envelope, as a server sends
/// it to a view.
pub fn encode_forget(id: u32) -> Vec<u8> {
    super::finish(forget_message(id))
}

/// Encode a scene as a frame for every player, with no envelope.
#[cfg(test)]
pub(crate) fn encode_frame(scene: &Scene) -> Vec<u8> {
    encode_frame_to(None, scene)
}

/// Encode a scene as a frame for `player`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_frame_to(player: Option<NonZeroU32>, scene: &Scene) -> Vec<u8> {
    super::finish(frame_message(player, scene))
}

/// Encode a bitmap upload as an asset, with no envelope.
#[cfg(test)]
pub(crate) fn encode_asset(id: u32, blob: &[u8], mime: Option<&str>) -> Vec<u8> {
    super::finish(asset_message(id, blob, mime))
}
