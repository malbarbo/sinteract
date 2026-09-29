//! The messages of the server, in the `ServerToView` union, which go to a
//! view.
//!
//! The server copies each asset and the scene of each frame of the engine
//! into a message of its own, once for every view. A view gets an asset
//! before the first frame that draws it, and a forget when no frame of the
//! view draws it anymore. A WebSocket frames each message itself, so a
//! message for a view has no envelope. The server ends the session with the
//! close of the WebSocket.

use std::collections::HashMap;
use std::sync::Arc;

use capnp::message::Builder as MessageBuilder;

use crate::protocol_capnp::server_to_view;
use crate::scene::{Image, Scene};
use crate::scene_capnp::scene as wire_scene;

use super::Error;
use super::protocol::decode_root;
use super::scene::read_scene;

/// The reader of the messages of the server for a view, which returns the
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
    /// the images and return `None`, as does a message of an arm from a
    /// newer schema. An asset that is not an image that [`Image::new`]
    /// takes keeps no image, so a bitmap of its id is skipped, as is a
    /// bitmap of an id with no asset.
    pub fn read(&mut self, payload: &[u8]) -> Result<Option<Scene>, Error> {
        decode_root::<server_to_view::Owned, _>(payload, |msg| {
            let Ok(which) = msg.which() else {
                return Ok(None);
            };
            match which {
                server_to_view::Asset(a) => {
                    let a = a?;
                    match Image::new(a.get_blob()?.to_vec()) {
                        Ok(image) => self.images.insert(a.get_id(), image),
                        Err(_) => self.images.remove(&a.get_id()),
                    };
                    Ok(None)
                }
                server_to_view::Frame(scene) => {
                    let images = |id| self.images.get(&id).cloned();
                    decode_root::<wire_scene::Owned, _>(scene?, |s| {
                        Ok(Some(read_scene(s, &images)?))
                    })
                }
                server_to_view::Forget(id) => {
                    self.images.remove(&id);
                    Ok(None)
                }
            }
        })
    }
}

/// Encode the blob of an image as the asset `id`, with no envelope.
pub fn encode_asset(id: u32, blob: &[u8]) -> Arc<[u8]> {
    let mut builder = MessageBuilder::new_default();
    let mut asset = builder.init_root::<server_to_view::Builder>().init_asset();
    asset.set_id(id);
    asset.set_blob(blob);
    super::to_shared_bytes(&builder)
}

/// Encode `scene`, the bytes of a message whose root is a `Scene`, as a
/// frame, with no envelope.
pub fn encode_frame(scene: &[u8]) -> Arc<[u8]> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<server_to_view::Builder>()
        .set_frame(scene);
    super::to_shared_bytes(&builder)
}

/// Encode that the view drops the asset `id`, with no envelope.
pub fn encode_forget(id: u32) -> Arc<[u8]> {
    let mut builder = MessageBuilder::new_default();
    builder
        .init_root::<server_to_view::Builder>()
        .set_forget(id);
    super::to_shared_bytes(&builder)
}
