//! Shared pixel-format helpers for the raster backends.

use tiny_skia::PremultipliedColorU8;

/// Recover straight-alpha RGB from a tiny-skia premultiplied pixel
/// (`c_premult = c_straight * a / 255`, inverted). `a == 0` yields black;
/// callers wanting a background composite the result over it.
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
