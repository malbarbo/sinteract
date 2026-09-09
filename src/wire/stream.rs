//! Decode a frame straight onto a renderer, without an `Element` tree.

use capnp::message::ReaderOptions;
use capnp::serialize;

use crate::frame_capnp::{element, message};
use crate::renderer::sealed::Paint as PaintSink;
use crate::scene::Path;

use super::Error;
use super::scene::{read_bitmap, read_clip_path, read_path_into, read_text_node};

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
pub(crate) fn stream_frame<P: PaintSink, R: std::io::Read>(
    paint: &mut P,
    reader: R,
) -> Result<(), Error> {
    let msg = serialize::read_message(reader, ReaderOptions::new())?;
    let m: message::Reader = msg.get_root()?;
    match m.which()? {
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
fn stream_elements<P: PaintSink>(
    paint: &mut P,
    list: capnp::struct_list::Reader<'_, element::Owned>,
    scratch: &mut Path,
) -> Result<(), Error> {
    use element::Which;
    for node in list.iter() {
        match node.which()? {
            Which::Path(p) => {
                read_path_into(p?, scratch)?;
                paint.draw_path(scratch);
            }
            Which::Clipped(c) => {
                let c = c?;
                let clip = read_clip_path(c.get_clip()?)?;
                let children = c.get_elements()?;
                // with_clip returns the value of the closure, so the Result
                // of the nested walk comes straight out.
                paint.with_clip(&clip, |p2| stream_elements(p2, children, &mut *scratch))?;
            }
            Which::Text(t) => {
                let t = read_text_node(t?)?;
                paint.draw_text(&t);
            }
            Which::Bitmap(b) => {
                let b = read_bitmap(b?);
                paint.draw_bitmap(&b);
            }
        }
    }
    Ok(())
}
