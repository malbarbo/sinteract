//! The messages of the engine, which go to the server.
//!
//! The engine says first how many players the game takes, in a message
//! whose root is a `Hello`. Every message after it is an `EngineToServer`.
//! The engine uploads each image as an asset, before the first frame that
//! draws it, and sends a frame per repaint, for one player or for all of
//! them. It tells the server as it takes each tick. The engine
//! ends the session with the end of its stream.
//!
//! [`crate::session::Session::write_frame`] writes the assets and the
//! frame. The server copies them into messages of
//! [`super::server_to_view`], which a view reads.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::sync::Arc;

use capnp::Word;
use capnp::message::{Builder as MessageBuilder, HeapAllocator};
use capnp::serialize;

use crate::asset::{Footprint, ImageError};
use crate::protocol_capnp::{engine_to_server, hello};
use crate::scene::{Image, Scene};
use crate::scene_capnp::scene as wire_scene;
use crate::session::PlayerRange;

use super::Error;
use super::framing::{Side, write_framed};
use super::protocol::decode_root;
use super::scene::{read_bitmap_ids, scene_message};
use super::server_to_view;

/// The arm of a message of the engine, with the player of a frame. A
/// server routes a message by its arm, and sends the views the message in
/// `to_view` of an asset or a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arm {
    /// An asset, with what the limits of [`crate::asset`] count, or why
    /// its blob cannot be an asset.
    Asset {
        id: u32,
        footprint: Result<Footprint, ImageError>,
        to_view: Arc<[u8]>,
    },
    /// A frame for `player`, or for every player when `player` is `None`,
    /// with the ids of its bitmaps. They may include a bitmap that a view
    /// does not draw, such as one in a hidden layer.
    Frame {
        player: Option<NonZeroU32>,
        ids: BTreeSet<u32>,
        to_view: Arc<[u8]>,
    },
    TickTaken,
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

/// Write the hello, the first message of the engine, which
/// [`crate::session::Session::start`] writes.
pub(crate) fn write_hello(w: &mut impl Write, players: PlayerRange) -> io::Result<()> {
    write_framed(
        w,
        Side::Engine,
        &hello_message(players.min().get(), players.max().get()),
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
/// it does not decode the image. It copies an asset and the scene of a
/// frame into a message of [`super::server_to_view`]. `None` for an arm
/// from a newer schema.
pub fn arm(payload: &[u8]) -> Result<Option<Arm>, Error> {
    decode_root::<engine_to_server::Owned, _>(payload, |msg| {
        let Ok(which) = msg.which() else {
            return Ok(None);
        };
        Ok(Some(match which {
            engine_to_server::Asset(a) => {
                let a = a?;
                let (id, blob) = (a.get_id(), a.get_blob()?);
                Arm::Asset {
                    id,
                    footprint: Footprint::of(blob),
                    to_view: server_to_view::encode_asset(id, blob),
                }
            }
            engine_to_server::Frame(f) => {
                let f = f?;
                let scene = f.get_scene()?;
                let mut ids = BTreeSet::new();
                decode_root::<wire_scene::Owned, _>(scene, |s| read_bitmap_ids(s, &mut ids))?;
                Arm::Frame {
                    player: NonZeroU32::new(f.get_player()),
                    ids,
                    to_view: server_to_view::encode_frame(scene),
                }
            }
            engine_to_server::TickTaken(()) => Arm::TickTaken,
        }))
    })
}

/// The players of the hello in `payload`, a message with no envelope. A
/// hello whose players are not a [`PlayerRange`] is an error.
pub fn read_hello(payload: &[u8]) -> Result<PlayerRange, Error> {
    decode_root::<hello::Owned, _>(payload, |h| {
        let (min, max) = (h.get_min_players(), h.get_max_players());
        PlayerRange::new(min, max).ok_or(Error::PlayerRange { min, max })
    })
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
    let mut hello = builder.init_root::<hello::Builder>();
    hello.set_min_players(min);
    hello.set_max_players(max);
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
