//! Shared pixel-format helpers for the raster backends.

use tiny_skia::PremultipliedColorU8;

/// Recover straight-alpha RGB from a tiny-skia premultiplied pixel. `a == 0`
/// yields black; callers wanting a background composite the result over it.
pub(crate) fn unpremultiply(p: PremultipliedColorU8) -> (u8, u8, u8) {
    let c = p.demultiply();
    (c.red(), c.green(), c.blue())
}
