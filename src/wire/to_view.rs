//! The messages from the engine to the view, in the `EngineMessage` union.
//!
//! The engine says first how many players the game takes, in a hello for
//! the server. Then it uploads each image as an asset, before the first
//! frame that draws it, and sends a frame per repaint, for one player or
//! for all of them. It tells the server as it takes each tick. The engine
//! ends the session with the end of its stream. The server adds a forget
//! for a view, when the view no longer needs an asset.
//!
//! [`crate::session::Session::write_frame`] writes the assets and the
//! frame, and a view reads them back with a [`Reader`].

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read, Write};
use std::num::NonZeroU32;

use capnp::message::{Builder as MessageBuilder, HeapAllocator};

use crate::asset::image_size;
use crate::protocol_capnp::{engine_message, hello};
use crate::scene::{Image, Scene};

use super::Error;
use super::framing::{Side, write_framed};
use super::protocol::{ReadError, decode_root, read_next};
use super::scene::{read_bitmap_ids, read_scene, write_scene};

/// The images of the assets that a view keeps, which the frames draw.
#[derive(Debug, Default)]
pub struct Reader {
    images: HashMap<u32, Image>,
}

impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read `payload`, a message with no envelope, as
    /// [`crate::server::Next::Send`] carries it. A frame comes out as its
    /// scene, with the image of each bitmap. An asset and a forget change
    /// the images and return `None`, as does every other message. An asset
    /// that is not an image that [`Image::new`] takes keeps no image, so a
    /// bitmap of its id is skipped, as is a bitmap of an id with no asset.
    pub fn read(&mut self, payload: &[u8]) -> Result<Option<Scene>, Error> {
        decode_root::<engine_message::Owned, _>(payload, |msg| {
            let Ok(which) = msg.which() else {
                return Ok(None);
            };
            match which {
                engine_message::Asset(a) => {
                    let a = a?;
                    match Image::new(a.get_blob()?.to_vec()) {
                        Ok(image) => self.images.insert(a.get_id(), image),
                        Err(_) => self.images.remove(&a.get_id()),
                    };
                    Ok(None)
                }
                engine_message::Frame(f) => {
                    let images = |id| self.images.get(&id).cloned();
                    Ok(Some(read_scene(f?.get_scene()?, &images)?))
                }
                engine_message::Forget(id) => {
                    self.images.remove(&id);
                    Ok(None)
                }
                engine_message::Hello(_) | engine_message::TickTaken(()) => Ok(None),
            }
        })
    }

    /// Read the messages of the engine from `r`, as [`Reader::read`] does,
    /// up to the next frame. Returns `None` at the end of the stream.
    pub fn read_frame(&mut self, r: &mut impl Read) -> Result<Option<Scene>, ReadError> {
        read_next(r, Side::Engine, |payload| self.read(payload))
    }
}

/// One message of the engine, one variant per arm of `EngineMessage`. A
/// bitmap of the id `n` draws [`crate::asset::png_image`] of `n` by 1, as
/// [`encode_frame`] writes it, and one of the id 0 is skipped.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) enum Message {
    Asset {
        id: u32,
        blob: Vec<u8>,
    },
    /// A frame for `player`, or for every player when `player` is `None`.
    Frame {
        player: Option<NonZeroU32>,
        scene: Scene,
    },
    Hello(PlayerRange),
    /// The view drops the asset of this id. Only a server sends it.
    Forget(u32),
    /// The engine took a tick. It goes to the server alone.
    TickTaken,
}

/// The arm of a message of the engine, with the player of a frame. A
/// server routes a message by its arm and passes the payload on as it
/// came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// An asset, with what the limits of [`crate::asset`] count.
    Asset {
        id: u32,
        /// The size from the header of the image, or `None` if the blob is
        /// not an image that a view decodes.
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
    TickTaken,
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
    pub fn new(min: u32, max: u32) -> Option<PlayerRange> {
        let min = NonZeroU32::new(min)?;
        let max = NonZeroU32::new(max).filter(|&max| max >= min && max.get() <= MAX_PLAYERS)?;
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

/// Read the next message of the engine, as [`decode`] does. Returns
/// `None` at the end of the stream.
#[cfg(test)]
pub(crate) fn read(r: &mut impl Read) -> Result<Option<Message>, ReadError> {
    read_next(r, Side::Engine, decode)
}

/// Write a scene as a frame for `player`, or for every player when
/// `player` is `None`, with the id that `ids` gives the image of each
/// bitmap. The asset of each id goes out before, with [`write_asset`].
pub fn write_frame(
    w: &mut impl Write,
    player: Option<NonZeroU32>,
    scene: &Scene,
    ids: &dyn Fn(&Image) -> u32,
) -> io::Result<()> {
    write_framed(w, Side::Engine, &frame_message(player, scene, ids))
}

/// Write the hello, the first message of the engine.
pub fn write_hello(w: &mut impl Write, players: PlayerRange) -> io::Result<()> {
    write_framed(w, Side::Engine, &hello_message(players))
}

/// Write that the engine took a tick.
pub fn write_tick_taken(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::Engine, &tick_taken_message())
}

/// Write the file of an image as the asset `id`.
pub fn write_asset(w: &mut impl Write, id: u32, blob: &[u8]) -> io::Result<()> {
    write_framed(w, Side::Engine, &asset_message(id, blob))
}

/// The arm of `payload`, a message with no envelope, without a decode of
/// the scene of a frame or of the image of an asset. `None` for an arm
/// from a newer schema. A hello whose players are not a [`PlayerRange`]
/// is an error.
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
                    size: image_size(blob),
                    bytes: blob.len(),
                }
            }
            engine_message::Frame(f) => Arm::Frame {
                player: NonZeroU32::new(f?.get_player()),
            },
            engine_message::Hello(h) => Arm::Hello(read_hello(h?)?),
            engine_message::Forget(id) => Arm::Forget(id),
            engine_message::TickTaken(()) => Arm::TickTaken,
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

/// Decode `payload`, a message with no envelope. `None` for a message of an
/// arm from a newer schema.
#[cfg(test)]
pub(crate) fn decode(payload: &[u8]) -> Result<Option<Message>, Error> {
    decode_root::<engine_message::Owned, _>(payload, decode_message)
}

#[cfg(test)]
fn decode_message(msg: engine_message::Reader<'_>) -> Result<Option<Message>, Error> {
    let Ok(which) = msg.which() else {
        return Ok(None);
    };
    match which {
        engine_message::Asset(a) => {
            let a = a?;
            Ok(Some(Message::Asset {
                id: a.get_id(),
                blob: a.get_blob()?.to_vec(),
            }))
        }
        engine_message::Frame(f) => {
            let f = f?;
            Ok(Some(Message::Frame {
                player: NonZeroU32::new(f.get_player()),
                scene: read_scene(f.get_scene()?, &|id| {
                    Image::new(crate::asset::png_head(id, 1)).ok()
                })?,
            }))
        }
        engine_message::Hello(h) => Ok(Some(Message::Hello(read_hello(h?)?))),
        engine_message::Forget(id) => Ok(Some(Message::Forget(id))),
        engine_message::TickTaken(()) => Ok(Some(Message::TickTaken)),
    }
}

fn read_hello(h: hello::Reader<'_>) -> Result<PlayerRange, Error> {
    let (min, max) = (h.get_min_players(), h.get_max_players());
    PlayerRange::new(min, max).ok_or(Error::PlayerRange { min, max })
}

fn frame_message(
    player: Option<NonZeroU32>,
    scene: &Scene,
    ids: &dyn Fn(&Image) -> u32,
) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut frame = builder.init_root::<engine_message::Builder>().init_frame();
    frame.set_player(player.map_or(0, NonZeroU32::get));
    write_scene(frame.init_scene(), scene, ids);
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

fn tick_taken_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<engine_message::Builder>()
        .set_tick_taken(());
    builder
}

fn asset_message(id: u32, blob: &[u8]) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut asset = builder.init_root::<engine_message::Builder>().init_asset();
    asset.set_id(id);
    asset.set_blob(blob);
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

/// Encode a scene as a frame for `player`, with no envelope. The id of an
/// image is its width, as [`decode`] reads it.
#[cfg(test)]
pub(crate) fn encode_frame_to(player: Option<NonZeroU32>, scene: &Scene) -> Vec<u8> {
    super::finish(frame_message(player, scene, &Image::width))
}

/// Encode the file of an image as the asset `id`, with no envelope.
#[cfg(test)]
pub(crate) fn encode_asset(id: u32, blob: &[u8]) -> Vec<u8> {
    super::finish(asset_message(id, blob))
}

/// Encode a hello of `min` to `max` players, with no envelope. A test
/// passes a range that [`write_hello`] cannot.
#[cfg(test)]
pub(crate) fn encode_hello(min: u32, max: u32) -> Vec<u8> {
    let mut builder = MessageBuilder::new_default();
    let mut hello = builder.init_root::<engine_message::Builder>().init_hello();
    hello.set_min_players(min);
    hello.set_max_players(max);
    super::finish(builder)
}
