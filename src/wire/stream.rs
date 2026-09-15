//! Decode a frame straight onto a renderer, without an `Element` tree.

use capnp::message::ReaderOptions;
use capnp::serialize;

use crate::protocol_capnp::message;
use crate::renderer::sealed::Canvas;
use crate::scene::Path;
use crate::scene_capnp::element;

use crate::renderer::AllocError;

use super::Error as PayloadError;
use super::scene::{read_bitmap, read_clip_path, read_path_into, read_text_node};
use super::skip_unknown;

/// Decoding a frame and painting it fail in three ways, and only the first
/// leaves the session usable.
#[derive(Debug)]
pub enum Error {
    /// The frame itself is malformed.
    Payload(PayloadError),
    /// The message decoded, but it is not a `Frame`, or it is an arm from a
    /// newer schema. Use [`decode`](super::decode) for the other arms.
    WrongMessageKind,
    /// The renderer could not size its surface for the frame.
    Surface(AllocError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Payload(e) => e.fmt(f),
            Error::WrongMessageKind => {
                write!(f, "expected Message::Frame, got a different union arm")
            }
            Error::Surface(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<PayloadError> for Error {
    fn from(e: PayloadError) -> Self {
        Error::Payload(e)
    }
}

impl From<capnp::Error> for Error {
    fn from(e: capnp::Error) -> Self {
        Error::Payload(e.into())
    }
}

impl From<AllocError> for Error {
    fn from(e: AllocError) -> Self {
        Error::Surface(e)
    }
}

/// Decode one `Message::Frame` from `reader` and paint it onto `paint`. The
/// reader is walked lazily, so the element list never becomes a
/// `Vec<Element>`, and a `Clipped` subtree recurses through
/// [`Paint::with_clip`]. Every path decodes into one scratch [`Path`] that
/// the whole frame reuses, so decoding allocates about as much as the
/// longest path.
///
/// The surface is sized once, after the dimensions are known and before any
/// element is painted. Any other message returns [`Error::WrongMessageKind`].
/// Use [`decode`] for those.
pub(crate) fn stream_frame<P: Canvas, R: std::io::Read>(
    paint: &mut P,
    reader: R,
) -> Result<(), Error> {
    let msg = serialize::read_message(reader, ReaderOptions::new())?;
    let m: message::Reader = msg.get_root()?;
    let Ok(which) = m.which() else {
        return Err(Error::WrongMessageKind);
    };
    match which {
        message::Frame(f) => {
            let frame = f?;
            paint.ensure_size(frame.get_width(), frame.get_height())?;
            if frame.has_elements() {
                let mut scratch = Path::default();
                stream_elements(paint, frame.get_elements()?, &mut scratch)?;
            }
            Ok(())
        }
        message::Asset(_) | message::Event(_) | message::SessionClose(()) => {
            Err(Error::WrongMessageKind)
        }
    }
}

/// `scratch` is the one [`Path`] of the frame. A clip gets its own, because
/// it is still live while its children decode.
fn stream_elements<P: Canvas>(
    paint: &mut P,
    list: capnp::struct_list::Reader<'_, element::Owned>,
    scratch: &mut Path,
) -> Result<(), Error> {
    use element::Which;
    for node in list.iter() {
        // An element of an arm from a newer schema is skipped, as `decode`
        // skips it, and so is one that holds a value from a newer schema.
        let Ok(which) = node.which() else {
            continue;
        };
        match which {
            Which::Path(p) => {
                if skip_unknown(read_path_into(p?, scratch))?.is_some() {
                    paint.draw_path(scratch);
                }
            }
            Which::Clipped(c) => {
                let c = c?;
                let Some(clip) = skip_unknown(read_clip_path(c.get_clip()?))? else {
                    continue;
                };
                let children = c.get_elements()?;
                // with_clip returns the value of the closure, so the Result
                // of the nested walk comes straight out.
                paint.with_clip(&clip, |p2| stream_elements(p2, children, &mut *scratch))?;
            }
            Which::Text(t) => {
                if let Some(t) = skip_unknown(read_text_node(t?))? {
                    paint.draw_text(&t);
                }
            }
            Which::Bitmap(b) => {
                let b = read_bitmap(b?);
                paint.draw_bitmap(&b);
            }
        }
    }
    Ok(())
}
