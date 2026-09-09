use tiny_skia::PremultipliedColorU8;

/// The straight RGB of a premultiplied pixel. Alpha 0 gives black.
pub(crate) fn unpremultiply(p: PremultipliedColorU8) -> (u8, u8, u8) {
    let c = p.demultiply();
    (c.red(), c.green(), c.blue())
}
