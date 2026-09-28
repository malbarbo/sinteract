//! The limits of the images of a room. [`fit_image`] shrinks an image to
//! the limit of one image, and the server keeps the live assets of a room
//! under the limits.
//!
//! A limit counts the pixels, since each view decodes an asset to four
//! bytes a pixel, and a small file can hold a large image. It also counts
//! the bytes, which the server keeps and sends to each view.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::scene::Image;

/// The most pixels of one image, 2048 by 2048.
pub const MAX_IMAGE_PIXELS: u64 = 2048 * 2048;

/// The most pixels of an image that [`fit_image`] shrinks to
/// [`MAX_IMAGE_PIXELS`], sixteen times as many, such as a photo of 8000 by
/// 6000. A larger one would take the engine more than 256 MiB to decode.
pub const MAX_SHRINK_PIXELS: u64 = 16 * MAX_IMAGE_PIXELS;

/// The most pixels of the live assets of a room, eight of the largest
/// images, which a view decodes to 128 MiB.
pub const MAX_LIVE_PIXELS: u64 = 8 * MAX_IMAGE_PIXELS;

/// The most bytes of the live assets of a room. It is under the cap of the
/// framing, so any asset under it fits in a message.
pub const MAX_LIVE_BYTES: u64 = 48 << 20;

/// The live assets of a room, under the limits at each frame.
/// [`Cache::asset`] keeps a new asset, and [`Cache::frame`] says which
/// assets a frame draws and drops the ones that the frames used longest
/// ago, down to the limits. An asset that the frame draws goes last, so an
/// engine that fits the images of each frame in the limits never loses one
/// that it draws. Between two frames the cache holds at most twice the
/// limits, since the new assets alone stay under them. The cache
/// does not tell the players apart, since the views of a room mostly draw
/// the same images. Each asset keeps a `T`, such as its message.
#[derive(Debug)]
pub(crate) struct Cache<T> {
    live: BTreeMap<u32, Live<T>>,
    /// The load of the assets up to the last frame, under the limits.
    load: Footprint,
    /// The load of the assets that came after the last frame.
    new: Footprint,
    /// How many frames came.
    frames: u64,
}

impl<T> Cache<T> {
    pub fn new() -> Self {
        Self {
            live: BTreeMap::new(),
            load: Footprint::NONE,
            new: Footprint::NONE,
            frames: 0,
        }
    }

    /// Returns `true` if the asset `id` is live, `false` otherwise.
    pub fn contains(&self, id: u32) -> bool {
        self.live.contains_key(&id)
    }

    /// The value of the asset `id`, or `None` if it is not live.
    pub fn get(&self, id: u32) -> Option<&T> {
        self.live.get(&id).map(|live| &live.value)
    }

    /// Keep the asset `id`, which is not live, with `value` until the next
    /// frame. Returns an error if the asset does not fit beside the others
    /// that came after the last frame, since no frame can draw them all. An
    /// error changes nothing.
    pub fn asset(&mut self, id: u32, footprint: Footprint, value: T) -> Result<(), RoomFull> {
        assert!(!self.contains(id), "the asset {id} is live");
        self.new = self.new.with(footprint)?;
        self.live.insert(
            id,
            Live {
                footprint,
                used: self.frames,
                value,
            },
        );
        Ok(())
    }

    /// Say that a frame draws the assets of `ids`, and return the ids of
    /// the assets that it drops to fit the limits, the ones that the
    /// frames used longest ago first. An id that is not live changes
    /// nothing.
    pub fn frame(&mut self, ids: &BTreeSet<u32>) -> Vec<u32> {
        for id in ids {
            if let Some(live) = self.live.get_mut(id) {
                live.used = self.frames;
            }
        }
        self.frames += 1;
        let mut load = self.load.plus(self.new);
        self.new = Footprint::NONE;
        let mut dropped = Vec::new();
        if load.fits() {
            self.load = load;
            return dropped;
        }
        let mut may_go: Vec<(bool, u64, u32)> = self
            .live
            .iter()
            .map(|(&id, live)| (ids.contains(&id), live.used, id))
            .collect();
        may_go.sort_unstable();
        for (_, _, id) in may_go {
            if load.fits() {
                break;
            }
            let live = self.live.remove(&id).expect("may_go holds live ids");
            load = load.without(live.footprint);
            dropped.push(id);
        }
        self.load = load;
        dropped
    }
}

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// What an asset, or the assets of a room together, count toward the
/// limits of a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footprint {
    pixels: u64,
    bytes: u64,
}

impl Footprint {
    /// The footprint of `blob`, or an error if it is not a PNG, a JPEG, a
    /// GIF or a WebP, or has more than [`MAX_IMAGE_PIXELS`].
    pub fn of(blob: &[u8]) -> Result<Footprint, ImageError> {
        let size = head(blob).ok_or(ImageError::Unsupported)?.size;
        check_pixels(size, MAX_IMAGE_PIXELS)?;
        Ok(Footprint {
            pixels: pixels(size),
            bytes: blob.len() as u64,
        })
    }

    /// No asset.
    const NONE: Footprint = Footprint {
        pixels: 0,
        bytes: 0,
    };

    /// The footprint with `asset` on top, or an error if that goes over
    /// [`MAX_LIVE_PIXELS`] or [`MAX_LIVE_BYTES`].
    fn with(self, asset: Footprint) -> Result<Footprint, RoomFull> {
        let load = self.plus(asset);
        if !load.fits() {
            return Err(RoomFull {
                pixels: load.pixels,
                bytes: load.bytes,
            });
        }
        Ok(load)
    }

    fn plus(self, other: Footprint) -> Footprint {
        Footprint {
            pixels: self.pixels + other.pixels,
            bytes: self.bytes + other.bytes,
        }
    }

    /// Returns `true` if the footprint is under [`MAX_LIVE_PIXELS`] and
    /// [`MAX_LIVE_BYTES`], `false` otherwise.
    fn fits(self) -> bool {
        self.pixels <= MAX_LIVE_PIXELS && self.bytes <= MAX_LIVE_BYTES
    }

    /// The footprint without `asset`, which the footprint holds.
    fn without(self, asset: Footprint) -> Footprint {
        Footprint {
            pixels: self.pixels - asset.pixels,
            bytes: self.bytes - asset.bytes,
        }
    }
}

/// Returns an error if `images` go over the limits of a room together,
/// since the server would lose one of them each frame.
pub(crate) fn fit_room<'a>(images: impl IntoIterator<Item = &'a Image>) -> Result<(), RoomFull> {
    let mut load = Footprint::NONE;
    for image in images {
        load = load.with(Footprint {
            pixels: image.pixels(),
            bytes: image.file().len() as u64,
        })?;
    }
    Ok(())
}

/// Why an image cannot be an asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// The blob does not start as a PNG, a JPEG, a GIF or a WebP does,
    /// which are the formats that a view decodes.
    Unsupported,
    /// The image has more than [`MAX_IMAGE_PIXELS`].
    TooManyPixels { width: u32, height: u32 },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Unsupported => {
                f.write_str("the image is not a PNG, a JPEG, a GIF or a WebP")
            }
            ImageError::TooManyPixels { width, height } => write!(
                f,
                "the image of {width}x{height} has more than {MAX_IMAGE_PIXELS} pixels"
            ),
        }
    }
}

impl std::error::Error for ImageError {}

/// The images of a room would hold these many pixels and bytes together,
/// over [`MAX_LIVE_PIXELS`] or [`MAX_LIVE_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomFull {
    pub pixels: u64,
    pub bytes: u64,
}

impl fmt::Display for RoomFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the images of the room would take {} pixels and {} bytes, \
             over {MAX_LIVE_PIXELS} pixels or {MAX_LIVE_BYTES} bytes",
            self.pixels, self.bytes
        )
    }
}

impl std::error::Error for RoomFull {}

/// `file`, or a PNG of it shrunk to [`MAX_IMAGE_PIXELS`] with its ratio if
/// it is larger, for [`Image::new`](crate::scene::Image::new). A front end
/// calls it once, as it loads the image. An image shrinks, with the feature
/// `render`, up to [`MAX_SHRINK_PIXELS`], and is an error past that or
/// without the feature. It is an error too if `file` is not a PNG, a JPEG,
/// a GIF or a WebP.
pub fn fit_image(file: Vec<u8>) -> Result<Vec<u8>, ImageError> {
    let size = head(&file).ok_or(ImageError::Unsupported)?.size;
    if pixels(size) > MAX_IMAGE_PIXELS {
        shrink(&file, size)
    } else {
        Ok(file)
    }
}

/// The size on the screen of the image in `file`, after the EXIF
/// orientation of a JPEG, or an error if it is not a PNG, a JPEG, a GIF or
/// a WebP, or has more than [`MAX_IMAGE_PIXELS`].
pub(crate) fn screen_size(file: &[u8]) -> Result<(u32, u32), ImageError> {
    let head = head(file).ok_or(ImageError::Unsupported)?;
    check_pixels(head.size, MAX_IMAGE_PIXELS)?;
    let (width, height) = head.size;
    Ok(if head.turned() {
        (height, width)
    } else {
        (width, height)
    })
}

/// How a document that holds the files of its images, such as an SVG or a
/// PDF, takes an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Embed {
    /// A PNG, which a document holds as it is.
    Png,
    /// A JPEG that its EXIF does not turn, which a document holds as it is.
    Jpeg { color: JpegColor, size: (u32, u32) },
    /// Any other image, of the media type `mime`. A document holds it
    /// decoded, since a GIF may move, a WebP does not show everywhere, and a
    /// document may not turn a JPEG.
    Decode { mime: &'static str },
}

/// How a document takes the image in `blob`, or an error if it is not a
/// PNG, a JPEG, a GIF or a WebP, or has more than [`MAX_IMAGE_PIXELS`].
pub(crate) fn embed(blob: &[u8]) -> Result<Embed, ImageError> {
    let head = head(blob).ok_or(ImageError::Unsupported)?;
    check_pixels(head.size, MAX_IMAGE_PIXELS)?;
    Ok(match head.format {
        Format::Png => Embed::Png,
        Format::Jpeg {
            orientation: 1,
            color: Some(color),
        } => Embed::Jpeg {
            color,
            size: head.size,
        },
        Format::Jpeg { .. } => Embed::Decode { mime: "image/jpeg" },
        Format::Gif => Embed::Decode { mime: "image/gif" },
        Format::WebP => Embed::Decode { mime: "image/webp" },
    })
}

/// The format of an image, from its first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Png,
    /// `orientation` is the EXIF orientation, from 1 to 8. `color` is the
    /// color of a JPEG of 8 bits a sample in gray or in RGB, and `None` for
    /// any other JPEG.
    Jpeg {
        orientation: u8,
        color: Option<JpegColor>,
    },
    Gif,
    WebP,
}

/// What the header of an image says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Head {
    format: Format,
    /// The width and the height, both above 0.
    size: (u32, u32),
}

/// The color space of a JPEG that a document can hold as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JpegColor {
    Gray,
    Rgb,
}

impl Head {
    /// Returns `true` if the orientation swaps the width and the height,
    /// `false` otherwise.
    fn turned(self) -> bool {
        self.orientation() >= 5
    }

    /// The EXIF orientation, from 1 to 8. Only a JPEG has one other than 1.
    fn orientation(self) -> u8 {
        match self.format {
            Format::Jpeg { orientation, .. } => orientation,
            Format::Png | Format::Gif | Format::WebP => 1,
        }
    }
}

/// Returns an error if an image of `size` has more than `max` pixels.
fn check_pixels(size: (u32, u32), max: u64) -> Result<(), ImageError> {
    if pixels(size) > max {
        let (width, height) = size;
        return Err(ImageError::TooManyPixels { width, height });
    }
    Ok(())
}

/// The pixels of an image of `size`, which do not wrap.
fn pixels((width, height): (u32, u32)) -> u64 {
    u64::from(width) * u64::from(height)
}

/// The header of the image in `blob`, or `None` if `blob` is not a PNG, a
/// JPEG, a GIF or a WebP, or gives a width or a height of 0.
fn head(blob: &[u8]) -> Option<Head> {
    let (format, size) = if blob.starts_with(PNG_SIGNATURE) {
        (Format::Png, png_size(blob)?)
    } else if blob.starts_with(b"\xff\xd8") {
        let (size, orientation, color) = jpeg_head(blob)?;
        (Format::Jpeg { orientation, color }, size)
    } else if blob.starts_with(b"GIF87a") || blob.starts_with(b"GIF89a") {
        (Format::Gif, gif_size(blob)?)
    } else if blob.get(..4) == Some(b"RIFF") && blob.get(8..12) == Some(b"WEBP") {
        (Format::WebP, webp_size(blob)?)
    } else {
        return None;
    };
    (size.0 > 0 && size.1 > 0).then_some(Head { format, size })
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The size from the IHDR of a PNG, the first chunk.
fn png_size(blob: &[u8]) -> Option<(u32, u32)> {
    const HEADER: &[u8] = b"\0\0\0\x0dIHDR";
    let rest = blob.strip_prefix(PNG_SIGNATURE)?.strip_prefix(HEADER)?;
    let width = u32::from_be_bytes(array(rest, 0)?);
    let height = u32::from_be_bytes(array(rest, 4)?);
    Some((width, height))
}

/// The size from the frame header of a JPEG, its EXIF orientation, and its
/// color if it is a Huffman DCT frame of 8 bits a sample in gray or RGB.
/// The walk goes over every segment up to the scan, as a decoder does. A
/// second frame header is an error, and the last EXIF segment wins.
fn jpeg_head(blob: &[u8]) -> Option<((u32, u32), u8, Option<JpegColor>)> {
    let mut size = None;
    let mut color = None;
    let mut orientation = 1;
    let mut at = 2;
    loop {
        if *blob.get(at)? != 0xff {
            return None;
        }
        // A marker may follow any number of fill bytes.
        while *blob.get(at)? == 0xff {
            at += 1;
        }
        let marker = *blob.get(at)?;
        at += 1;
        match marker {
            // TEM and RST0 to RST7 have no length.
            0x01 | 0xd0..=0xd7 => continue,
            // The scan starts, or the image ends.
            0xda | 0xd9 => break,
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes(array(blob, at)?));
        let segment = blob.get(at.checked_add(2)?..at.checked_add(len)?)?;
        match marker {
            // SOF0 to SOF15. C4, C8 and CC are DHT, JPG and DAC.
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                if size.is_some() {
                    return None;
                }
                let height = u16::from_be_bytes(array(segment, 1)?);
                let width = u16::from_be_bytes(array(segment, 3)?);
                size = Some((u32::from(width), u32::from(height)));
                // Only SOF0 to SOF2, the Huffman DCT frames, open in every
                // PDF viewer and browser.
                let dct = matches!(marker, 0xc0..=0xc2);
                color = match (dct, segment.first(), segment.get(5)) {
                    (true, Some(8), Some(1)) => Some(JpegColor::Gray),
                    (true, Some(8), Some(3)) => Some(JpegColor::Rgb),
                    _ => None,
                };
            }
            0xe1 => {
                if let Some(tiff) = segment.strip_prefix(b"Exif\0\0") {
                    orientation = exif_orientation(tiff).unwrap_or(1);
                }
            }
            _ => {}
        }
        at += len;
    }
    Some((size?, orientation, color))
}

/// The orientation tag of the first IFD of the TIFF in `tiff`, or `None`
/// if there is none. As a decoder does, the tag counts only with the type
/// SHORT, a count of 1 and a value from 1 to 8.
fn exif_orientation(tiff: &[u8]) -> Option<u8> {
    const ORIENTATION: u16 = 0x0112;
    const SHORT: u16 = 3;
    let big = match tiff.get(..4)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        _ => return None,
    };
    let u16_at = |at: usize| {
        let b = array(tiff, at)?;
        Some(if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |at: usize| {
        let b = array(tiff, at)?;
        Some(if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let entries = u16_at(ifd)?;
    for i in 0..usize::from(entries) {
        // Each entry is 12 bytes, after the count of 2 bytes.
        let entry = ifd.checked_add(2 + 12 * i)?;
        if u16_at(entry)? == ORIENTATION {
            let (kind, count, value) = (u16_at(entry + 2)?, u32_at(entry + 4)?, u16_at(entry + 8)?);
            return (kind == SHORT && count == 1 && (1..=8).contains(&value))
                .then(|| u8::try_from(value).expect("the value is at most 8"));
        }
    }
    None
}

/// The size of a GIF, the larger of its logical screen and of the extent
/// of its first image on it. A decoder draws the first image into a buffer
/// of the screen, but an image past the screen takes its own buffer, and a
/// browser may not clip it.
fn gif_size(blob: &[u8]) -> Option<(u32, u32)> {
    let screen_width = u16::from_le_bytes(array(blob, 6)?);
    let screen_height = u16::from_le_bytes(array(blob, 8)?);
    let flags = *blob.get(10)?;
    let mut at = 13;
    if flags & 0x80 != 0 {
        // The global color table, of 2^(n+1) colors of 3 bytes.
        at += 3 << ((flags & 7) + 1);
    }
    loop {
        match *blob.get(at)? {
            // An extension: the introducer, the label, then sub-blocks up
            // to one of length 0.
            0x21 => {
                at += 2;
                loop {
                    let len = usize::from(*blob.get(at)?);
                    at += 1 + len;
                    if len == 0 {
                        break;
                    }
                }
            }
            // The descriptor of the first image.
            0x2c => {
                let [left, top, width, height] =
                    [1, 3, 5, 7].map(|offset| array(blob, at + offset).map(u16::from_le_bytes));
                let right = u32::from(left?) + u32::from(width?);
                let bottom = u32::from(top?) + u32::from(height?);
                return Some((
                    right.max(u32::from(screen_width)),
                    bottom.max(u32::from(screen_height)),
                ));
            }
            // The trailer, or damage, before any image.
            _ => return None,
        }
    }
}

/// The size of a WebP, from its first chunk. A lossy image starts with
/// VP8, a lossless one with VP8L, and one with more, such as alpha or an
/// animation, with VP8X and the size of its canvas.
fn webp_size(blob: &[u8]) -> Option<(u32, u32)> {
    let payload = blob.get(20..)?;
    match blob.get(12..16)? {
        b"VP8 " => {
            // The frame tag, then the start code.
            if payload.get(3..6)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            // The top two bits of each are the scale, which a decoder
            // ignores.
            let width = u16::from_le_bytes(array(payload, 6)?) & 0x3fff;
            let height = u16::from_le_bytes(array(payload, 8)?) & 0x3fff;
            Some((u32::from(width), u32::from(height)))
        }
        b"VP8L" => {
            if *payload.first()? != 0x2f {
                return None;
            }
            // 14 bits of the width less 1, then 14 of the height less 1.
            let bits = u32::from_le_bytes(array(payload, 1)?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => {
            // The flags take 4 bytes, then 24 bits of each side less 1.
            let side = |at: usize| {
                let [a, b, c] = array(payload, at)?;
                Some(u32::from_le_bytes([a, b, c, 0]) + 1)
            };
            Some((side(4)?, side(7)?))
        }
        _ => None,
    }
}

/// The pixels of the image in `blob`, premultiplied, the way up that the
/// EXIF of a JPEG gives, or an error if it does not decode or has more
/// than `max_pixels`. The decoder allocates no more than the size in the
/// header, which [`Footprint`] counts, and its own buffers stay under four
/// bytes a pixel of `max_pixels`.
#[cfg(feature = "render")]
pub(crate) fn decode(
    blob: &[u8],
    max_pixels: u64,
) -> Result<tiny_skia::Pixmap, Box<dyn std::error::Error + Send + Sync>> {
    use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

    let head = head(blob).ok_or(ImageError::Unsupported)?;
    check_pixels(head.size, max_pixels)?;
    let (width, height) = head.size;
    let format = match head.format {
        Format::Png => ImageFormat::Png,
        Format::Jpeg { .. } => ImageFormat::Jpeg,
        Format::Gif => ImageFormat::Gif,
        Format::WebP => ImageFormat::WebP,
    };
    let mut reader = ImageReader::with_format(std::io::Cursor::new(blob), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_pixels * 4);
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    // The limits bound the buffers of the decoder but not the image, whose
    // size comes from the decoder. A decoder that reads the header in its
    // own way must not allocate past what the header gave.
    let (w, h) = decoder.dimensions();
    if w > width || h > height {
        return Err(
            format!("the image of {w}x{h} is larger than its header of {width}x{height}").into(),
        );
    }
    let mut image = DynamicImage::from_decoder(decoder)?;
    // A PNG or a WebP may hold an EXIF too, but the size of the asset, and
    // a browser, turn only a JPEG.
    if let Some(orientation) = image::metadata::Orientation::from_exif(head.orientation()) {
        image.apply_orientation(orientation);
    }
    let size = tiny_skia::IntSize::from_wh(image.width(), image.height())
        .ok_or("the image has no pixels")?;
    let mut pixels = image.into_rgba8().into_raw();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let [r, g, b, a] = *pixel;
        let p = tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
        *pixel = [p.red(), p.green(), p.blue(), a];
    }
    Ok(tiny_skia::Pixmap::from_vec(pixels, size).expect("the pixels fill the size"))
}

/// The `N` bytes of `blob` from `at`, or `None` past its end.
fn array<const N: usize>(blob: &[u8], at: usize) -> Option<[u8; N]> {
    blob.get(at..)?.first_chunk().copied()
}

/// A live asset of a [`Cache`].
#[derive(Debug)]
struct Live<T> {
    footprint: Footprint,
    /// The count of frames when a frame last drew the asset, or when it
    /// came. One that came after the last frame has the count of frames.
    used: u64,
    value: T,
}

/// The image in `blob`, of `width` by `height`, shrunk to at most
/// [`MAX_IMAGE_PIXELS`] with its ratio, as a PNG, or an error if it has
/// more than [`MAX_SHRINK_PIXELS`] or does not decode.
#[cfg(feature = "render")]
fn shrink(blob: &[u8], size: (u32, u32)) -> Result<Vec<u8>, ImageError> {
    check_pixels(size, MAX_SHRINK_PIXELS)?;
    shrink_to(blob, MAX_IMAGE_PIXELS)
}

/// Without the renderer, an image over the limit cannot shrink.
#[cfg(not(feature = "render"))]
fn shrink(_blob: &[u8], (width, height): (u32, u32)) -> Result<Vec<u8>, ImageError> {
    Err(ImageError::TooManyPixels { width, height })
}

/// The largest size with the ratio of `width` by `height` and at most
/// `pixels`, at least 1 by 1.
#[cfg(feature = "render")]
fn shrunk_size(width: u32, height: u32, pixels: u64) -> (u32, u32) {
    let scale = (pixels as f64 / (f64::from(width) * f64::from(height))).sqrt();
    let to_height = ((f64::from(height) * scale) as u32).max(1);
    let to_width = ((f64::from(width) * scale) as u32)
        .max(1)
        .min((pixels / u64::from(to_height)) as u32);
    (to_width, to_height)
}

/// The image in `blob` drawn at the largest size with its ratio and at
/// most `pixels`, as a PNG. The size is the one after the EXIF orientation
/// of a JPEG, and the PNG is the way up. Each halving averages four
/// pixels, which a single scale of a large ratio would skip, and a last
/// scale reaches the size.
#[cfg(feature = "render")]
fn shrink_to(blob: &[u8], pixels: u64) -> Result<Vec<u8>, ImageError> {
    let mut image = decode(blob, MAX_SHRINK_PIXELS).map_err(|_| ImageError::Unsupported)?;
    let (width, height) = shrunk_size(image.width(), image.height(), pixels);
    while image.width() / 2 >= width && image.height() / 2 >= height {
        image = scaled(&image, image.width() / 2, image.height() / 2);
    }
    let image = scaled(&image, width, height);
    Ok(image.encode_png().expect("a pixmap encodes"))
}

/// `image` drawn at `width` by `height`.
#[cfg(feature = "render")]
fn scaled(image: &tiny_skia::Pixmap, width: u32, height: u32) -> tiny_skia::Pixmap {
    let mut out = tiny_skia::Pixmap::new(width, height).expect("a shrunk image has a size");
    let transform = tiny_skia::Transform::from_scale(
        width as f32 / image.width() as f32,
        height as f32 / image.height() as f32,
    );
    let paint = tiny_skia::PixmapPaint {
        quality: tiny_skia::FilterQuality::Bilinear,
        ..tiny_skia::PixmapPaint::default()
    };
    out.draw_pixmap(0, 0, image.as_ref(), &paint, transform, None);
    out
}

/// The first 24 bytes of a PNG of `width` by `height`, all that [`head`]
/// reads.
#[cfg(test)]
pub(crate) fn png_head(width: u32, height: u32) -> Vec<u8> {
    let mut head = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    head.extend_from_slice(&width.to_be_bytes());
    head.extend_from_slice(&height.to_be_bytes());
    head
}

/// An image of [`png_head`], which has a size and no pixels to decode.
#[cfg(test)]
pub(crate) fn png_image(width: u32, height: u32) -> crate::scene::Image {
    Image::new(png_head(width, height)).expect("a PNG head is an image")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "render")]
    #[test]
    fn a_shrunk_image_averages_its_pixels() {
        // Stripes of red and blue, one pixel wide, average to purple.
        let mut stripes = tiny_skia::Pixmap::new(64, 32).unwrap();
        for (i, pixel) in stripes.pixels_mut().iter_mut().enumerate() {
            *pixel = if i % 2 == 0 {
                tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply()
            } else {
                tiny_skia::ColorU8::from_rgba(0, 0, 255, 255).premultiply()
            };
        }
        let png = shrink_to(&stripes.encode_png().unwrap(), 18).unwrap();
        assert_eq!(image_size(&png), Some((6, 3)));
        let small = tiny_skia::Pixmap::decode_png(&png).unwrap();
        let pixel = small.pixel(3, 1).unwrap();
        assert!((100..=155).contains(&pixel.red()), "{pixel:?}");
        assert!((100..=155).contains(&pixel.blue()), "{pixel:?}");
    }

    #[cfg(feature = "render")]
    #[test]
    fn a_turned_jpeg_shrinks_to_a_png_the_way_up() {
        let jpeg = red_blue(image::ImageFormat::Jpeg);
        let mut turned = jpeg[..2].to_vec();
        turned.extend_from_slice(&exif(true, 6));
        turned.extend_from_slice(&jpeg[2..]);
        let png = shrink_to(&turned, 32).unwrap();
        assert_eq!(
            head(&png).map(|h| (h.format, h.size)),
            Some((Format::Png, (4, 8)))
        );
        let small = tiny_skia::Pixmap::decode_png(&png).unwrap();
        assert!(small.pixel(2, 1).unwrap().red() > 200);
    }

    #[cfg(feature = "render")]
    #[test]
    fn the_shrunk_size_keeps_the_ratio_under_the_limit() {
        let max = MAX_IMAGE_PIXELS;
        assert_eq!(shrunk_size(4096, 4096, max), (2048, 2048));
        assert_eq!(shrunk_size(3000, 2000, max), (2508, 1672));
        assert_eq!(shrunk_size(40_000_000, 1, max), (4_194_304, 1));
        for (w, h) in [(3000, 2000), (8000, 6000), (5000, 900), (40_000_000, 1)] {
            let (to_w, to_h) = shrunk_size(w, h, max);
            assert!(u64::from(to_w) * u64::from(to_h) <= MAX_IMAGE_PIXELS);
        }
    }

    /// The footprint of the largest image, eight of which fill a room.
    fn largest() -> Footprint {
        Footprint {
            pixels: MAX_IMAGE_PIXELS,
            bytes: 1,
        }
    }

    fn ids(ids: &[u32]) -> BTreeSet<u32> {
        ids.iter().copied().collect()
    }

    /// A cache with the eight largest assets 0 to 7, and a frame that
    /// draws none of them.
    fn full() -> Cache<()> {
        let mut cache = Cache::new();
        for id in 0..8 {
            assert_eq!(cache.asset(id, largest(), ()), Ok(()));
        }
        assert_eq!(cache.frame(&ids(&[])), []);
        cache
    }

    #[test]
    fn a_frame_drops_the_assets_that_the_frames_used_longest_ago() {
        let mut cache = full();
        cache.frame(&ids(&[0, 3]));
        cache.frame(&ids(&[5]));
        cache.frame(&ids(&[]));
        assert_eq!(cache.asset(8, largest(), ()), Ok(()));
        assert_eq!(cache.asset(9, largest(), ()), Ok(()));
        assert_eq!(cache.frame(&ids(&[8, 9])), [1, 2]);
        for id in 10..14 {
            assert_eq!(cache.asset(id, largest(), ()), Ok(()));
        }
        assert_eq!(cache.frame(&ids(&[])), [4, 6, 7, 0]);
        assert!(!cache.contains(0));
        assert!(cache.contains(3));
    }

    #[test]
    fn a_frame_keeps_an_old_asset_that_it_draws_beside_a_new_one() {
        let mut cache = full();
        cache.frame(&ids(&[1, 2, 3, 4, 5, 6, 7]));
        assert_eq!(cache.asset(8, largest(), ()), Ok(()));
        assert_eq!(cache.frame(&ids(&[0, 8])), [1]);
        assert!(cache.contains(0) && cache.contains(8));
    }

    #[test]
    fn a_frame_that_draws_over_the_limits_drops_its_own_assets() {
        let mut cache = full();
        assert_eq!(cache.asset(8, largest(), ()), Ok(()));
        assert_eq!(cache.frame(&ids(&[0, 1, 2, 3, 4, 5, 6, 7, 8])), [0]);
    }

    #[test]
    fn an_asset_that_does_not_fit_beside_the_new_ones_changes_nothing() {
        let mut cache = full();
        let small = Footprint {
            pixels: 100,
            bytes: 100,
        };
        let heavy = Footprint {
            pixels: 1,
            bytes: MAX_LIVE_BYTES,
        };
        assert_eq!(cache.asset(8, heavy, ()), Ok(()));
        assert!(matches!(cache.asset(9, small, ()), Err(RoomFull { .. })));
        assert!(!cache.contains(9));
        assert_eq!(cache.frame(&ids(&[8])), [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(cache.asset(9, small, ()), Ok(()));
        assert_eq!(cache.frame(&ids(&[9])), [8]);
    }

    #[test]
    fn the_size_comes_from_the_header_of_a_png() {
        assert_eq!(image_size(&png_head(640, 480)), Some((640, 480)));
        assert_eq!(image_size(&png_head(640, 480)[..23]), None);
        assert_eq!(image_size(&png_head(0, 480)), None);
        let mut other_chunk = png_head(640, 480);
        other_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(image_size(&other_chunk), None);
    }

    #[test]
    fn a_document_holds_a_png_and_an_upright_jpeg_as_they_are() {
        assert_eq!(embed(&png_head(640, 480)), Ok(Embed::Png));
        let gray = jpeg_head_of(&[sof(640, 480)]);
        assert_eq!(
            embed(&gray),
            Ok(Embed::Jpeg {
                color: JpegColor::Gray,
                size: (640, 480)
            })
        );
        let decode = Ok(Embed::Decode { mime: "image/jpeg" });
        let turned = jpeg_head_of(&[exif(true, 6), sof(640, 480)]);
        assert_eq!(embed(&turned), decode);
        // A frame header of 4 components, such as CMYK.
        let mut cmyk = sof(640, 480);
        cmyk[9] = 4;
        assert_eq!(embed(&jpeg_head_of(&[cmyk])), decode);
        // A lossless frame header, which few viewers read.
        let mut lossless = sof(640, 480);
        lossless[1] = 0xc3;
        assert_eq!(embed(&jpeg_head_of(&[lossless])), decode);
        assert_eq!(
            embed(&png_head(4096, 4096)),
            Err(ImageError::TooManyPixels {
                width: 4096,
                height: 4096
            })
        );
        assert_eq!(embed(b"not an image"), Err(ImageError::Unsupported));
    }

    #[test]
    fn the_size_comes_from_the_frame_header_of_a_jpeg() {
        let jpeg = jpeg_head_of(&[app(0xe0, b"JFIF\0"), sof(640, 480)]);
        assert_eq!(
            head(&jpeg).map(|h| (h.format, h.size)),
            Some((
                Format::Jpeg {
                    orientation: 1,
                    color: Some(JpegColor::Gray)
                },
                (640, 480)
            ))
        );
        // Fill bytes before a marker, and a segment that has no length.
        let mut filled = jpeg[..2].to_vec();
        filled.extend_from_slice(b"\xff\xff\xd0");
        filled.extend_from_slice(&jpeg[2..]);
        assert_eq!(image_size(&filled), Some((640, 480)));
        assert_eq!(image_size(&jpeg[..jpeg.len() - 3]), None);
        assert_eq!(image_size(&jpeg_head_of(&[sof(640, 480), sof(8, 8)])), None);
        assert_eq!(image_size(&jpeg_head_of(&[sof(640, 0)])), None);
        // A DHT has a marker of the range of the frame headers.
        assert_eq!(image_size(&jpeg_head_of(&[app(0xc4, &[0; 8])])), None);
    }

    #[test]
    fn the_orientation_of_a_jpeg_comes_from_its_last_exif() {
        let turned = jpeg_head_of(&[exif(true, 6), sof(640, 480)]);
        let head = head(&turned).unwrap();
        assert_eq!((head.size, head.orientation()), ((640, 480), 6));
        assert!(head.turned());
        let little = jpeg_head_of(&[exif(false, 8), sof(640, 480)]);
        assert_eq!(super::head(&little).unwrap().orientation(), 8);
        let last = jpeg_head_of(&[exif(true, 6), sof(640, 480), exif(true, 3)]);
        assert_eq!(super::head(&last).unwrap().orientation(), 3);
        let wrong = jpeg_head_of(&[exif(true, 9), sof(640, 480)]);
        assert_eq!(super::head(&wrong).unwrap().orientation(), 1);
    }

    #[test]
    fn an_image_over_the_limit_or_of_no_format_is_an_error() {
        assert!(matches!(
            Image::new(png_head(2049, 2048)),
            Err(ImageError::TooManyPixels { .. })
        ));
        assert!(matches!(
            Image::new(b"GIF89a".to_vec()),
            Err(ImageError::Unsupported)
        ));
    }

    #[test]
    fn fit_image_keeps_an_image_under_the_limit() {
        assert_eq!(fit_image(png_head(2, 3)), Ok(png_head(2, 3)));
        assert_eq!(fit_image(b"GIF".to_vec()), Err(ImageError::Unsupported));
        // The header of a large image, with no pixels to shrink.
        let large = fit_image(png_head(2049, 2048));
        if cfg!(feature = "render") {
            assert_eq!(large, Err(ImageError::Unsupported));
        } else {
            assert!(matches!(large, Err(ImageError::TooManyPixels { .. })));
        }
    }

    #[test]
    fn a_turned_jpeg_has_the_size_on_the_screen_and_the_footprint_of_the_file() {
        let jpeg = jpeg_head_of(&[exif(true, 6), sof(640, 480)]);
        let image = Image::new(jpeg.clone()).unwrap();
        assert_eq!((image.width(), image.height()), (480, 640));
        assert_eq!(Footprint::of(&jpeg).map(|f| f.pixels), Ok(640 * 480));
    }

    #[test]
    fn the_size_of_a_gif_covers_its_screen_and_its_first_image() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[64, 0, 32, 0, 0x80, 0, 0]);
        // A global color table of 2 colors.
        gif.extend_from_slice(&[0; 6]);
        // A graphic control extension.
        gif.extend_from_slice(&[0x21, 0xf9, 4, 0, 0, 0, 0, 0]);
        let mut inside = gif.clone();
        inside.extend_from_slice(&[0x2c, 0, 0, 0, 0, 8, 0, 8, 0, 0]);
        assert_eq!(
            head(&inside).map(|h| (h.format, h.size)),
            Some((Format::Gif, (64, 32)))
        );
        let mut past = gif.clone();
        past.extend_from_slice(&[0x2c, 60, 0, 0, 0, 100, 0, 8, 0, 0]);
        assert_eq!(image_size(&past), Some((160, 32)));
        gif.push(0x3b);
        assert_eq!(image_size(&gif), None);
    }

    #[test]
    fn the_size_of_a_webp_comes_from_its_first_chunk() {
        let lossy = webp(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 0x80, 0xc2, 0xe0, 0x01],
        );
        assert_eq!(
            head(&lossy).map(|h| (h.format, h.size)),
            Some((Format::WebP, (640, 480)))
        );
        let bits: u32 = 639 | (479 << 14);
        let mut lossless = vec![0x2f];
        lossless.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(image_size(&webp(b"VP8L", &lossless)), Some((640, 480)));
        let extended = webp(b"VP8X", &[0x10, 0, 0, 0, 0x7f, 0x02, 0, 0xdf, 0x01, 0]);
        assert_eq!(image_size(&extended), Some((640, 480)));
        assert_eq!(image_size(&webp(b"VP8 ", &[0, 0, 0, 0, 0, 0])), None);
        assert_eq!(image_size(&webp(b"ALPH", &[0; 10])), None);
    }

    #[cfg(feature = "render")]
    #[test]
    fn each_format_decodes_to_its_pixels() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::WebP,
        ] {
            let pixmap = decode(&red_blue(format), MAX_IMAGE_PIXELS).unwrap();
            assert_eq!((pixmap.width(), pixmap.height()), (16, 8), "{format:?}");
            let red = pixmap.pixel(2, 4).unwrap();
            let blue = pixmap.pixel(13, 4).unwrap();
            // A JPEG loses a little.
            assert!(red.red() > 230 && red.blue() < 25, "{format:?} {red:?}");
            assert!(blue.blue() > 230 && blue.red() < 25, "{format:?} {blue:?}");
        }
    }

    #[cfg(feature = "render")]
    #[test]
    fn a_jpeg_decodes_the_way_up_of_its_exif() {
        let jpeg = red_blue(image::ImageFormat::Jpeg);
        let mut turned = jpeg[..2].to_vec();
        turned.extend_from_slice(&exif(true, 6));
        turned.extend_from_slice(&jpeg[2..]);
        let pixmap = decode(&turned, MAX_IMAGE_PIXELS).unwrap();
        assert_eq!((pixmap.width(), pixmap.height()), (8, 16));
        // A quarter turn clockwise puts the red left half on top.
        assert!(pixmap.pixel(4, 2).unwrap().red() > 230);
        assert!(pixmap.pixel(4, 13).unwrap().blue() > 230);
    }

    #[cfg(feature = "render")]
    #[test]
    fn an_image_that_breaks_the_header_or_the_limit_does_not_decode() {
        let png = red_blue(image::ImageFormat::Png);
        assert!(decode(&png, 16 * 8).is_ok());
        assert!(decode(&png, 16 * 8 - 1).is_err());
        assert!(decode(&png[..png.len() - 8], MAX_IMAGE_PIXELS).is_err());
        assert!(decode(b"GIF89a", MAX_IMAGE_PIXELS).is_err());
    }

    /// An image of 16 by 8 in `format`, red on the left half and blue on
    /// the right.
    #[cfg(feature = "render")]
    fn red_blue(format: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_fn(16, 8, |x, _| {
            image::Rgb(if x < 8 { [255, 0, 0] } else { [0, 0, 255] })
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut out, format)
            .unwrap();
        out.into_inner()
    }

    /// A JPEG of the start marker and `segments`, up to the scan.
    fn jpeg_head_of(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut jpeg = b"\xff\xd8".to_vec();
        for segment in segments {
            jpeg.extend_from_slice(segment);
        }
        jpeg.extend_from_slice(b"\xff\xda\0\x02");
        jpeg
    }

    /// A segment of `marker` that holds `data`.
    fn app(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut segment = vec![0xff, marker];
        let len = u16::try_from(data.len() + 2).unwrap();
        segment.extend_from_slice(&len.to_be_bytes());
        segment.extend_from_slice(data);
        segment
    }

    /// A baseline frame header of `width` by `height`, with one component.
    fn sof(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![8];
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&[1, 1, 0x11, 0]);
        app(0xc0, &data)
    }

    /// An EXIF segment whose first IFD has a tag of the software and the
    /// orientation, in big or little endian.
    fn exif(big: bool, orientation: u16) -> Vec<u8> {
        let u16b = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut data = b"Exif\0\0".to_vec();
        data.extend_from_slice(if big { b"MM\0*" } else { b"II*\0" });
        data.extend_from_slice(&u32b(8));
        data.extend_from_slice(&u16b(2));
        // The software, an ASCII string of 4 bytes that fits in the entry.
        data.extend_from_slice(&u16b(0x0131));
        data.extend_from_slice(&u16b(2));
        data.extend_from_slice(&u32b(4));
        data.extend_from_slice(b"abc\0");
        data.extend_from_slice(&u16b(0x0112));
        data.extend_from_slice(&u16b(3));
        data.extend_from_slice(&u32b(1));
        data.extend_from_slice(&u16b(orientation));
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&u32b(0));
        app(0xe1, &data)
    }

    /// A WebP whose first chunk is `fourcc` with `payload`.
    fn webp(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&u32::try_from(payload.len() + 12).unwrap().to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(fourcc);
        webp.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        webp.extend_from_slice(payload);
        webp
    }

    #[test]
    fn an_image_over_the_limit_or_not_a_png_has_no_footprint() {
        assert_eq!(
            Footprint::of(&png_head(2048, 2048)),
            Ok(Footprint {
                pixels: MAX_IMAGE_PIXELS,
                bytes: 24
            })
        );
        assert_eq!(
            Footprint::of(&png_head(2049, 2048)),
            Err(ImageError::TooManyPixels {
                width: 2049,
                height: 2048
            })
        );
        // A product of two large sides does not wrap.
        assert!(Footprint::of(&png_head(u32::MAX, u32::MAX)).is_err());
        assert_eq!(Footprint::of(b"GIF89a"), Err(ImageError::Unsupported));
    }

    #[test]
    fn a_load_refuses_an_asset_over_the_pixels_or_the_bytes_of_a_room() {
        let largest = Footprint::of(&png_head(2048, 2048)).unwrap();
        let mut load = Footprint::NONE;
        for _ in 0..8 {
            load = load.with(largest).unwrap();
        }
        let one = Footprint {
            pixels: 1,
            bytes: 1,
        };
        assert!(matches!(load.with(one), Err(RoomFull { .. })));
        assert_eq!(
            load.without(largest).with(one).unwrap().without(one),
            load.without(largest)
        );
        let heavy = Footprint {
            pixels: 1,
            bytes: MAX_LIVE_BYTES,
        };
        let load = Footprint::NONE.with(heavy).unwrap();
        assert_eq!(
            load.with(one),
            Err(RoomFull {
                pixels: 2,
                bytes: MAX_LIVE_BYTES + 1
            })
        );
    }

    /// The width and the height from the header of the image in `blob`,
    /// before the EXIF orientation of a JPEG turns them.
    fn image_size(blob: &[u8]) -> Option<(u32, u32)> {
        head(blob).map(|head| head.size)
    }
}
