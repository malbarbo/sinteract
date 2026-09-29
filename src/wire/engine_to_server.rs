//! The messages of the engine, in the `EngineToServer` union, which go to
//! the server and through it to the views.
//!
//! The engine says first how many players the game takes, in a hello for
//! the server. Then it uploads each image as an asset, before the first
//! frame that draws it, and sends a frame per repaint, for one player or
//! for all of them. It tells the server as it takes each tick. The engine
//! ends the session with the end of its stream. The server adds a forget
//! for a view, when the view no longer needs an asset.
//!
//! [`crate::session::Session::write_frame`] writes the assets and the
//! frame, and a view reads them back with a [`FrameReader`].

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::num::NonZeroU32;

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator};
use capnp::serialize;

use crate::asset::{Footprint, ImageError};
use crate::protocol_capnp::{engine_to_server, hello};
use crate::scene::{Image, Scene};
use crate::scene_capnp::scene as wire_scene;

use super::Error;
use super::framing::{Side, write_framed};
use super::protocol::decode_root;
use super::scene::{read_bitmap_ids, read_scene, scene_message};

/// The reader of the frames of the engine for a view, which returns the
/// scene of each frame. It keeps the images of the assets that the frames
/// draw, so a view reads its whole connection with one.
#[derive(Debug, Default)]
pub struct FrameReader {
    images: HashMap<u32, Image>,
}

impl FrameReader {
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
        decode_root::<engine_to_server::Owned, _>(payload, |msg| {
            let Ok(which) = msg.which() else {
                return Ok(None);
            };
            match which {
                engine_to_server::Asset(a) => {
                    let a = a?;
                    match Image::new(a.get_blob()?.to_vec()) {
                        Ok(image) => self.images.insert(a.get_id(), image),
                        Err(_) => self.images.remove(&a.get_id()),
                    };
                    Ok(None)
                }
                engine_to_server::Frame(f) => {
                    let images = |id| self.images.get(&id).cloned();
                    decode_root::<wire_scene::Owned, _>(f?.get_scene()?, |s| {
                        Ok(Some(read_scene(s, &images)?))
                    })
                }
                engine_to_server::Forget(id) => {
                    self.images.remove(&id);
                    Ok(None)
                }
                engine_to_server::Hello(_) | engine_to_server::TickTaken(()) => Ok(None),
            }
        })
    }
}

/// The arm of a message of the engine, with the player of a frame. A
/// server routes a message by its arm and passes the payload on as it
/// came.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arm {
    /// An asset, with what the limits of [`crate::asset`] count, or why
    /// its blob cannot be an asset.
    Asset {
        id: u32,
        footprint: Result<Footprint, ImageError>,
    },
    /// A frame for `player`, or for every player when `player` is `None`,
    /// with the ids of its bitmaps. They may include a bitmap that a view
    /// does not draw, such as one in a hidden layer.
    Frame {
        player: Option<NonZeroU32>,
        ids: BTreeSet<u32>,
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
    write_framed(
        w,
        Side::Engine,
        &hello_message(players.min.get(), players.max.get()),
    )
}

/// Write that the engine took a tick.
pub fn write_tick_taken(w: &mut impl Write) -> io::Result<()> {
    write_framed(w, Side::Engine, &tick_taken_message())
}

/// Write the blob of an image as the asset `id`.
pub fn write_asset(w: &mut impl Write, id: u32, blob: &[u8]) -> io::Result<()> {
    write_framed(w, Side::Engine, &asset_message(id, blob))
}

/// The arm of `payload`, a message with no envelope. Of a frame it reads
/// the ids of the bitmaps and not the rest of the scene, and of an asset
/// it does not decode the image. `None` for an arm
/// from a newer schema. A hello whose players are not a [`PlayerRange`]
/// is an error.
pub fn arm(payload: &[u8]) -> Result<Option<Arm>, Error> {
    decode_root::<engine_to_server::Owned, _>(payload, |msg| {
        let Ok(which) = msg.which() else {
            return Ok(None);
        };
        Ok(Some(match which {
            engine_to_server::Asset(a) => {
                let a = a?;
                Arm::Asset {
                    id: a.get_id(),
                    footprint: Footprint::of(a.get_blob()?),
                }
            }
            engine_to_server::Frame(f) => {
                let f = f?;
                let mut ids = BTreeSet::new();
                decode_root::<wire_scene::Owned, _>(f.get_scene()?, |s| {
                    read_bitmap_ids(s, &mut ids)
                })?;
                Arm::Frame {
                    player: NonZeroU32::new(f.get_player()),
                    ids,
                }
            }
            engine_to_server::Hello(h) => Arm::Hello(read_hello(h?)?),
            engine_to_server::Forget(id) => Arm::Forget(id),
            engine_to_server::TickTaken(()) => Arm::TickTaken,
        }))
    })
}

pub(super) fn read_hello(h: hello::Reader<'_>) -> Result<PlayerRange, Error> {
    let (min, max) = (h.get_min_players(), h.get_max_players());
    PlayerRange::new(min, max).ok_or(Error::PlayerRange { min, max })
}

pub(super) fn frame_message(
    player: Option<NonZeroU32>,
    scene: &Scene,
    ids: &dyn Fn(&Image) -> u32,
) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut frame = builder
        .init_root::<engine_to_server::Builder>()
        .init_frame();
    frame.set_player(player.map_or(0, NonZeroU32::get));
    // Straight into the data of the frame, with no buffer in between.
    let scene = scene_message(scene, ids);
    let len = serialize::compute_serialized_size_in_words(&scene) * size_of::<Word>();
    let data: &mut [u8] = frame.init_scene(len as u32);
    serialize::write_message(data, &scene).expect("the data of the frame fits the scene");
    builder
}

/// A hello for `min` to `max` players, which a test may set out of range.
pub(super) fn hello_message(min: u32, max: u32) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut hello = builder
        .init_root::<engine_to_server::Builder>()
        .init_hello();
    hello.set_min_players(min);
    hello.set_max_players(max);
    builder
}

fn forget_message(id: u32) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<engine_to_server::Builder>()
        .set_forget(id);
    builder
}

fn tick_taken_message() -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<engine_to_server::Builder>()
        .set_tick_taken(());
    builder
}

pub(super) fn asset_message(id: u32, blob: &[u8]) -> MessageBuilder<HeapAllocator> {
    let mut builder = MessageBuilder::new_default();
    let mut asset = builder
        .init_root::<engine_to_server::Builder>()
        .init_asset();
    asset.set_id(id);
    asset.set_blob(blob);
    builder
}

/// Encode that the asset `id` is gone, with no envelope, as a server sends
/// it to a view.
pub fn encode_forget(id: u32) -> Vec<u8> {
    super::to_bytes(forget_message(id))
}
