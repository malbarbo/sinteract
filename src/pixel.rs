//! Shared pixel-format helpers for the raster backends.

use tiny_skia::PremultipliedColorU8;

/// Recover straight-alpha RGB from a tiny-skia premultiplied pixel.
///
/// tiny-skia stores `c_premult = c_straight * a / 255`; this inverts it.
/// Fully-opaque pixels pass through untouched and `a == 0` yields black
/// (no color survives) — callers that need a background substitute it
/// themselves (e.g. by compositing the result over `bg`).
pub(crate) fn unpremultiply(p: PremultipliedColorU8) -> (u8, u8, u8) {
    let a = p.alpha();
    if a == 255 {
        return (p.red(), p.green(), p.blue());
    }
    if a == 0 {
        return (0, 0, 0);
    }
    let af = f32::from(a) / 255.0;
    let straight = |c: u8| (f32::from(c) / af).round().clamp(0.0, 255.0) as u8;
    (straight(p.red()), straight(p.green()), straight(p.blue()))
}
