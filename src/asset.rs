//! The images of a room. [`Assets`] is the table of the engine, which gives
//! an image its id and sends it before the first frame that draws it.
//! [`Cache`] keeps the live assets of a room under the limits, in the
//! server.
//!
//! A limit counts the pixels, since each view decodes an asset to four
//! bytes a pixel, and a small file can hold a large image. It also counts
//! the bytes, which the server keeps and sends to each view.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use crate::scene::{Bitmap, Element, RotatedRect, Scene};

/// The most pixels of one image, 2048 by 2048.
pub const MAX_IMAGE_PIXELS: u64 = 2048 * 2048;

/// The most pixels of an image that [`Assets::image`] shrinks to
/// [`MAX_IMAGE_PIXELS`], sixteen times as many, such as a photo of 8000 by
/// 6000. A larger one would take the engine more than 256 MiB to decode.
pub const MAX_SHRINK_PIXELS: u64 = 16 * MAX_IMAGE_PIXELS;

/// The most pixels of the live assets of a room, eight of the largest
/// images, which a view decodes to 128 MiB.
pub const MAX_LIVE_PIXELS: u64 = 8 * MAX_IMAGE_PIXELS;

/// The most bytes of the live assets of a room. It is under the cap of the
/// framing, so any asset under it fits in a message.
pub const MAX_LIVE_BYTES: u64 = 48 << 20;

/// The images of an engine. [`Assets::image`] gives an image its id, as a
/// front end turns its image into a [`Bitmap`], and [`Assets::frame`] says
/// which images go out before a frame. The same image keeps its id until
/// the server loses it, so a program can make the same image each frame
/// and send it once.
///
/// The front end calls `image` for the bitmaps of one frame, then `frame`
/// for that frame, and [`Assets::lost`] for each lost of the server. An
/// image that did not go out is gone after the next two calls of `frame`.
#[derive(Debug, Default)]
pub struct Assets {
    ids: HashMap<Arc<[u8]>, u32>,
    images: BTreeMap<u32, Entry>,
    /// How many times `frame` ran.
    frames: u64,
    next_id: u32,
}

impl Assets {
    pub fn new() -> Self {
        Self::default()
    }

    /// The image in `blob`, with its id, or an error if it is not a PNG, a
    /// JPEG, a GIF or a WebP. An image with more than [`MAX_IMAGE_PIXELS`]
    /// shrinks to fit, with the feature `render`, up to
    /// [`MAX_SHRINK_PIXELS`], and is an error past that or without the
    /// feature. The size of the asset is the size on the screen, after the
    /// EXIF orientation of a JPEG.
    pub fn image(&mut self, blob: &[u8]) -> Result<Asset, AssetError> {
        if let Some(&id) = self.ids.get(blob) {
            let entry = self.images.get_mut(&id).expect("an id names an image");
            entry.seen = self.frames;
            return Ok(entry.asset);
        }
        let source = Arc::<[u8]>::from(blob);
        let head = head(blob).ok_or(AssetError::Unsupported)?;
        let (width, height) = head.size;
        let (png, size) = if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
            let shrunk = shrink(blob, width, height)?;
            let size = image_size(&shrunk).expect("the shrunk image is a PNG");
            (Arc::from(shrunk), size)
        } else {
            (source.clone(), (width, height))
        };
        let footprint = Footprint::new(Some(size), png.len())?;
        // The student sees the image the way up that its EXIF gives.
        let (width, height) = if head.turned() {
            (height, width)
        } else {
            (width, height)
        };
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .expect("an engine makes fewer than 2^32 images");
        let asset = Asset { id, width, height };
        self.ids.insert(source.clone(), id);
        self.images.insert(
            id,
            Entry {
                asset,
                source,
                png,
                footprint,
                sent: false,
                seen: self.frames,
            },
        );
        Ok(asset)
    }

    /// The id and the PNG of each image that `scene` draws and that has not
    /// gone out, which go out before the frame. A bitmap whose id did not
    /// come from [`Assets::image`] sends nothing. Returns
    /// [`AssetError::Full`] and changes nothing if the images of `scene`
    /// go over the limits of a room, since the server would lose one of
    /// them each frame.
    pub fn frame(&mut self, scene: &Scene) -> Result<Vec<Upload>, AssetError> {
        let ids = bitmap_ids(scene);
        let mut load = Load::default();
        for entry in ids.iter().filter_map(|id| self.images.get(id)) {
            load = load.with(entry.footprint)?;
        }
        let mut send = Vec::new();
        for id in ids {
            if let Some(entry) = self.images.get_mut(&id).filter(|e| !e.sent) {
                entry.sent = true;
                send.push((id, entry.png.clone()));
            }
        }
        let frames = self.frames;
        // An image that did not go out, from before the last frame.
        let stale: Vec<u32> = self
            .images
            .iter()
            .filter(|(_, entry)| !entry.sent && entry.seen < frames)
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            self.remove(id);
        }
        self.frames += 1;
        Ok(send)
    }

    /// Say that the server lost the image `id`. The image gets a new id the
    /// next time, and goes out again. An unknown id does nothing.
    pub fn lost(&mut self, id: u32) {
        self.remove(id);
    }

    fn remove(&mut self, id: u32) {
        if let Some(entry) = self.images.remove(&id) {
            self.ids.remove(&entry.source);
        }
    }
}

/// The id and the PNG of an image that goes out before a frame.
pub type Upload = (u32, Arc<[u8]>);

/// An image of [`Assets`], with its id and the size of the image from the
/// program. A shrunk image keeps its size, since a [`Bitmap`] places the
/// image with no regard to its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

impl Asset {
    /// The bitmap of the image, drawn into `rect`.
    pub fn fit(self, rect: RotatedRect) -> Bitmap {
        Bitmap::fit(self.id, rect)
    }
}

/// The ids of the bitmaps that `scene` draws, in its clips too.
pub fn bitmap_ids(scene: &Scene) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    add_bitmap_ids(scene.elements(), &mut ids);
    ids
}

/// The live assets of a room, under the limits. [`Cache::asset`] keeps a
/// new asset, and drops the ones that the frames used longest ago when it
/// does not fit. [`Cache::frame`] says which assets a frame draws. An asset
/// that the last frame draws stays, as does one that came after the last
/// frame, since the next frame draws it. The cache does not tell the
/// players apart, since the views of a room mostly draw the same images.
/// Each asset keeps a `T`, such as its message.
#[derive(Debug)]
pub struct Cache<T> {
    live: BTreeMap<u32, Live<T>>,
    /// The ids that the last frame draws.
    shown: BTreeSet<u32>,
    load: Load,
    /// How many frames came.
    frames: u64,
}

impl<T> Cache<T> {
    pub fn new() -> Self {
        Self {
            live: BTreeMap::new(),
            shown: BTreeSet::new(),
            load: Load::default(),
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

    /// Keep the asset `id`, which is not live, with `value`, and return the
    /// ids of the assets that it drops to fit, the ones that the frames
    /// used longest ago first. Returns [`AssetError::Full`] and changes
    /// nothing if the asset does not fit even without every asset that may
    /// go.
    pub fn asset(
        &mut self,
        id: u32,
        footprint: Footprint,
        value: T,
    ) -> Result<Vec<u32>, AssetError> {
        assert!(
            !self.contains(id),
            "Cache::asset takes an id that is not live"
        );
        let mut may_go: Vec<(u64, u32)> = self
            .live
            .iter()
            .filter(|(id, live)| !self.shown.contains(id) && live.used < self.frames)
            .map(|(&id, live)| (live.used, id))
            .collect();
        may_go.sort_unstable();
        let mut load = self.load;
        let mut dropped = Vec::new();
        let mut may_go = may_go.into_iter();
        let load = loop {
            match load.with(footprint) {
                Ok(load) => break load,
                Err(e) => {
                    let Some((_, gone)) = may_go.next() else {
                        return Err(e);
                    };
                    let footprint = self
                        .live
                        .get(&gone)
                        .expect("may_go holds live ids")
                        .footprint;
                    load = load.without(footprint);
                    dropped.push(gone);
                }
            }
        };
        for gone in &dropped {
            self.live.remove(gone);
        }
        self.load = load;
        self.live.insert(
            id,
            Live {
                footprint,
                used: self.frames,
                value,
            },
        );
        Ok(dropped)
    }

    /// Say that a frame draws the assets of `ids`. An id that is not live
    /// changes nothing.
    pub fn frame(&mut self, ids: BTreeSet<u32>) {
        for id in &ids {
            if let Some(live) = self.live.get_mut(id) {
                live.used = self.frames;
            }
        }
        self.shown = ids;
        self.frames += 1;
    }
}

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// What an asset counts toward the limits of a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footprint {
    pixels: u64,
    bytes: u64,
}

impl Footprint {
    /// The footprint of `blob`, or an error if it is not a PNG, a JPEG, a
    /// GIF or a WebP, or has more than [`MAX_IMAGE_PIXELS`].
    pub fn of(blob: &[u8]) -> Result<Footprint, AssetError> {
        Footprint::new(image_size(blob), blob.len())
    }

    /// The footprint of a blob of `bytes` bytes whose header gives `size`,
    /// as [`image_size`] reads it.
    pub fn new(size: Option<(u32, u32)>, bytes: usize) -> Result<Footprint, AssetError> {
        let (width, height) = size.ok_or(AssetError::Unsupported)?;
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_IMAGE_PIXELS {
            return Err(AssetError::TooManyPixels { width, height });
        }
        Ok(Footprint {
            pixels,
            bytes: bytes as u64,
        })
    }
}

/// The live assets of a room, as the limits count them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Load {
    pixels: u64,
    bytes: u64,
}

impl Load {
    /// The load with `asset` on top, or an error if that goes over
    /// [`MAX_LIVE_PIXELS`] or [`MAX_LIVE_BYTES`].
    fn with(self, asset: Footprint) -> Result<Load, AssetError> {
        let load = Load {
            pixels: self.pixels + asset.pixels,
            bytes: self.bytes + asset.bytes,
        };
        if load.pixels > MAX_LIVE_PIXELS || load.bytes > MAX_LIVE_BYTES {
            return Err(AssetError::Full {
                pixels: load.pixels,
                bytes: load.bytes,
            });
        }
        Ok(load)
    }

    /// The load without `asset`, which [`Load::with`] added before.
    fn without(self, asset: Footprint) -> Load {
        Load {
            pixels: self.pixels - asset.pixels,
            bytes: self.bytes - asset.bytes,
        }
    }
}

/// Why an asset breaks the limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetError {
    /// The blob does not start as a PNG, a JPEG, a GIF or a WebP does,
    /// which are the formats that a view decodes.
    Unsupported,
    /// The image has more than [`MAX_IMAGE_PIXELS`].
    TooManyPixels { width: u32, height: u32 },
    /// The live assets of the room would hold these many pixels and bytes,
    /// over [`MAX_LIVE_PIXELS`] or [`MAX_LIVE_BYTES`].
    Full { pixels: u64, bytes: u64 },
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssetError::Unsupported => {
                f.write_str("the image is not a PNG, a JPEG, a GIF or a WebP")
            }
            AssetError::TooManyPixels { width, height } => write!(
                f,
                "the image of {width}x{height} has more than {MAX_IMAGE_PIXELS} pixels"
            ),
            AssetError::Full { pixels, bytes } => write!(
                f,
                "the images of the room would take {pixels} pixels and {bytes} bytes, \
                 over {MAX_LIVE_PIXELS} pixels or {MAX_LIVE_BYTES} bytes"
            ),
        }
    }
}

impl std::error::Error for AssetError {}

/// The width and the height from the header of the image in `blob`, or
/// `None` if `blob` is not a PNG, a JPEG, a GIF or a WebP, or gives a
/// width or a height of 0. It is the size that a decoder allocates, before
/// the EXIF orientation of a JPEG turns it. It reads the header and decodes
/// nothing.
pub fn image_size(blob: &[u8]) -> Option<(u32, u32)> {
    head(blob).map(|head| head.size)
}

/// The format of an image, from its first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Png,
    Jpeg,
    Gif,
    WebP,
}

/// What the header of an image says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Head {
    format: Format,
    /// The width and the height, both above 0.
    size: (u32, u32),
    /// The EXIF orientation of a JPEG, from 1 to 8, and 1 for any other
    /// format.
    orientation: u8,
}

impl Head {
    /// Returns `true` if the orientation swaps the width and the height,
    /// `false` otherwise.
    fn turned(self) -> bool {
        self.orientation >= 5
    }
}

/// The header of the image in `blob`, or `None` if `blob` is not a PNG, a
/// JPEG, a GIF or a WebP, or gives a width or a height of 0.
fn head(blob: &[u8]) -> Option<Head> {
    let (format, size, orientation) = if blob.starts_with(PNG_SIGNATURE) {
        (Format::Png, png_size(blob)?, 1)
    } else if blob.starts_with(b"\xff\xd8") {
        let (size, orientation) = jpeg_head(blob)?;
        (Format::Jpeg, size, orientation)
    } else if blob.starts_with(b"GIF87a") || blob.starts_with(b"GIF89a") {
        (Format::Gif, gif_size(blob)?, 1)
    } else if blob.get(..4) == Some(b"RIFF") && blob.get(8..12) == Some(b"WEBP") {
        (Format::WebP, webp_size(blob)?, 1)
    } else {
        return None;
    };
    (size.0 > 0 && size.1 > 0).then_some(Head {
        format,
        size,
        orientation,
    })
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

/// The size from the frame header of a JPEG, and its EXIF orientation. The
/// walk goes over every segment up to the scan, as a decoder does. A
/// second frame header is an error, and the last EXIF segment wins.
fn jpeg_head(blob: &[u8]) -> Option<((u32, u32), u8)> {
    let mut size = None;
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
    Some((size?, orientation))
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

    let head = head(blob).ok_or(AssetError::Unsupported)?;
    let (width, height) = head.size;
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(AssetError::TooManyPixels { width, height }.into());
    }
    let format = match head.format {
        Format::Png => ImageFormat::Png,
        Format::Jpeg => ImageFormat::Jpeg,
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
    if let Some(orientation) = image::metadata::Orientation::from_exif(head.orientation) {
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

/// An image of [`Assets`].
#[derive(Debug)]
struct Entry {
    asset: Asset,
    /// The image from the front end, which finds the image again.
    source: Arc<[u8]>,
    /// The PNG that goes out, which is `source` unless it shrank.
    png: Arc<[u8]>,
    footprint: Footprint,
    /// Whether the image went out, and the server has not lost it since.
    sent: bool,
    /// The count of frames at the last call of [`Assets::image`] for it.
    seen: u64,
}

/// The image in `blob`, of `width` by `height`, shrunk to at most
/// [`MAX_IMAGE_PIXELS`] with its ratio, as a PNG, or an error if it has
/// more than [`MAX_SHRINK_PIXELS`] or does not decode.
#[cfg(feature = "render")]
fn shrink(blob: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AssetError> {
    if u64::from(width) * u64::from(height) > MAX_SHRINK_PIXELS {
        return Err(AssetError::TooManyPixels { width, height });
    }
    shrink_to(blob, MAX_IMAGE_PIXELS)
}

/// Without the renderer, an image over the limit cannot shrink.
#[cfg(not(feature = "render"))]
fn shrink(_blob: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AssetError> {
    Err(AssetError::TooManyPixels { width, height })
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
fn shrink_to(blob: &[u8], pixels: u64) -> Result<Vec<u8>, AssetError> {
    let mut image = decode(blob, MAX_SHRINK_PIXELS).map_err(|_| AssetError::Unsupported)?;
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

fn add_bitmap_ids(elements: &[Element], ids: &mut BTreeSet<u32>) {
    for element in elements {
        match element {
            Element::Bitmap(b) => {
                ids.insert(b.id);
            }
            Element::Clipped { elements, .. } => add_bitmap_ids(elements, ids),
            Element::Path(_) | Element::Text(_) => {}
        }
    }
}

/// The first 24 bytes of a PNG of `width` by `height`, all that
/// [`image_size`] reads.
#[cfg(test)]
pub(crate) fn png_head(width: u32, height: u32) -> Vec<u8> {
    let mut head = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    head.extend_from_slice(&width.to_be_bytes());
    head.extend_from_slice(&height.to_be_bytes());
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{ClipPath, FillRule};

    /// A scene that draws the bitmaps of `ids`, the last one inside a clip.
    fn drawing(ids: &[u32]) -> Scene {
        let mut scene = Scene::new(10.0, 10.0);
        let Some((last, rest)) = ids.split_last() else {
            return scene;
        };
        for &id in rest {
            scene.bitmap(Bitmap {
                id,
                ..Bitmap::default()
            });
        }
        let clip = ClipPath::builder(FillRule::NonZero, 0.0, 0.0)
            .line_to(5.0, 0.0)
            .line_to(5.0, 5.0)
            .build();
        scene.clip(clip).bitmap(Bitmap {
            id: *last,
            ..Bitmap::default()
        });
        scene
    }

    fn sent(send: &[Upload]) -> Vec<u32> {
        send.iter().map(|(id, _)| *id).collect()
    }

    fn id(assets: &mut Assets, png: &[u8]) -> u32 {
        assets.image(png).unwrap().id
    }

    #[test]
    fn the_same_image_gets_the_same_id_and_goes_out_once() {
        let mut assets = Assets::new();
        let a = id(&mut assets, &png_head(4, 4));
        assert_eq!(id(&mut assets, &png_head(4, 4)), a);
        let b = id(&mut assets, &png_head(5, 5));
        assert_ne!(a, b);
        let send = assets.frame(&drawing(&[a, b])).unwrap();
        assert_eq!(sent(&send), [a, b]);
        assert_eq!(&*send[0].1, &png_head(4, 4)[..]);
        // An image that no frame draws for a while keeps its id.
        assert!(assets.frame(&drawing(&[])).unwrap().is_empty());
        assert!(assets.frame(&drawing(&[])).unwrap().is_empty());
        assert_eq!(id(&mut assets, &png_head(4, 4)), a);
        assert!(assets.frame(&drawing(&[a, b])).unwrap().is_empty());
    }

    #[test]
    fn a_lost_image_gets_a_new_id_and_goes_out_again() {
        let mut assets = Assets::new();
        let a = id(&mut assets, &png_head(4, 4));
        assets.frame(&drawing(&[a])).unwrap();
        assets.lost(a);
        assets.lost(99);
        let again = id(&mut assets, &png_head(4, 4));
        assert_ne!(again, a);
        assert_eq!(sent(&assets.frame(&drawing(&[a, again])).unwrap()), [again]);
    }

    #[test]
    fn an_image_that_did_not_go_out_is_gone_after_two_frames() {
        let mut assets = Assets::new();
        let a = id(&mut assets, &png_head(4, 4));
        assets.frame(&drawing(&[])).unwrap();
        assert_eq!(id(&mut assets, &png_head(4, 4)), a);
        assets.frame(&drawing(&[])).unwrap();
        assets.frame(&drawing(&[])).unwrap();
        assert_ne!(id(&mut assets, &png_head(4, 4)), a);
    }

    #[test]
    fn an_image_that_is_not_a_png_or_is_too_large_gets_no_id() {
        let mut assets = Assets::new();
        assert_eq!(assets.image(b"GIF89a"), Err(AssetError::Unsupported));
        assert_eq!(
            assets.image(&png_head(8193, 8193)),
            Err(AssetError::TooManyPixels {
                width: 8193,
                height: 8193
            })
        );
    }

    #[cfg(not(feature = "render"))]
    #[test]
    fn an_image_over_the_limit_is_an_error_without_the_renderer() {
        let mut assets = Assets::new();
        assert!(matches!(
            assets.image(&png_head(2049, 2048)),
            Err(AssetError::TooManyPixels { .. })
        ));
    }

    #[cfg(feature = "render")]
    #[test]
    fn an_image_over_the_limit_goes_to_the_renderer_to_shrink() {
        // The header of a large image, with no pixels to decode.
        let mut assets = Assets::new();
        assert_eq!(
            assets.image(&png_head(2049, 2048)),
            Err(AssetError::Unsupported)
        );
    }

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

    #[test]
    fn a_frame_whose_images_go_over_the_limits_is_an_error_and_changes_nothing() {
        let mut assets = Assets::new();
        let ids: Vec<u32> = (0..9)
            .map(|k| id(&mut assets, &png_head(2048, 2048 - k)))
            .collect();
        assert!(matches!(
            assets.frame(&drawing(&ids)),
            Err(AssetError::Full { .. })
        ));
        assert_eq!(sent(&assets.frame(&drawing(&ids[..8])).unwrap()), ids[..8]);
    }

    #[test]
    fn a_bitmap_of_an_unknown_id_sends_nothing() {
        let mut assets = Assets::new();
        assert!(assets.frame(&drawing(&[42])).unwrap().is_empty());
    }

    /// The footprint of the largest image, eight of which fill a room.
    fn largest() -> Footprint {
        Footprint::new(Some((2048, 2048)), 1).unwrap()
    }

    fn ids(ids: &[u32]) -> BTreeSet<u32> {
        ids.iter().copied().collect()
    }

    /// A cache with the eight largest assets 0 to 7, and a frame that
    /// draws none of them.
    fn full() -> Cache<()> {
        let mut cache = Cache::new();
        for id in 0..8 {
            assert_eq!(cache.asset(id, largest(), ()), Ok(vec![]));
        }
        cache.frame(ids(&[]));
        cache
    }

    #[test]
    fn a_full_cache_drops_the_asset_that_the_frames_used_longest_ago() {
        let mut cache = full();
        cache.frame(ids(&[0, 3]));
        cache.frame(ids(&[5]));
        cache.frame(ids(&[]));
        assert_eq!(cache.asset(8, largest(), ()), Ok(vec![1]));
        assert_eq!(cache.asset(9, largest(), ()), Ok(vec![2]));
        cache.frame(ids(&[8, 9]));
        assert_eq!(cache.asset(10, largest(), ()), Ok(vec![4]));
        assert_eq!(cache.asset(11, largest(), ()), Ok(vec![6]));
        assert_eq!(cache.asset(12, largest(), ()), Ok(vec![7]));
        assert_eq!(cache.asset(13, largest(), ()), Ok(vec![0]));
        assert!(!cache.contains(0));
        assert!(cache.contains(3));
    }

    #[test]
    fn an_asset_of_the_last_frame_or_for_the_next_frame_stays() {
        let mut cache = full();
        cache.frame(ids(&[0, 1]));
        cache.frame(ids(&[2, 3, 4, 5, 6]));
        assert_eq!(cache.asset(8, largest(), ()), Ok(vec![7]));
        assert_eq!(cache.asset(9, largest(), ()), Ok(vec![0]));
        assert_eq!(cache.asset(10, largest(), ()), Ok(vec![1]));
        // Every other asset is in the last frame or came after it.
        assert!(matches!(
            cache.asset(11, largest(), ()),
            Err(AssetError::Full { .. })
        ));
        assert!(!cache.contains(11));
        cache.frame(ids(&[8]));
        assert_eq!(cache.asset(11, largest(), ()), Ok(vec![2]));
    }

    #[test]
    fn an_asset_that_fits_drops_nothing_and_one_too_large_changes_nothing() {
        let mut cache = Cache::new();
        let small = Footprint::new(Some((10, 10)), 100).unwrap();
        assert_eq!(cache.asset(1, small, ()), Ok(vec![]));
        cache.frame(ids(&[]));
        let heavy = Footprint::new(Some((1, 1)), MAX_LIVE_BYTES as usize).unwrap();
        assert!(cache.asset(2, heavy, ()).is_ok_and(|gone| gone == [1]));
        cache.frame(ids(&[2]));
        assert!(cache.asset(3, small, ()).is_err());
        assert!(cache.contains(2) && !cache.contains(3));
    }

    /// Send the images of `scene` from `assets` to `cache`, as a view does,
    /// and tell `assets` of each one that `cache` drops. Returns the ids
    /// that went out and the ids that were lost.
    fn round_trip(
        assets: &mut Assets,
        cache: &mut Cache<()>,
        scene: &Scene,
    ) -> (Vec<u32>, Vec<u32>) {
        let mut sent = Vec::new();
        let mut lost = Vec::new();
        for (id, blob) in assets.frame(scene).unwrap() {
            sent.push(id);
            let footprint = Footprint::of(&blob).unwrap();
            for gone in cache.asset(id, footprint, ()).unwrap() {
                assets.lost(gone);
                lost.push(gone);
            }
        }
        cache.frame(bitmap_ids(scene));
        (sent, lost)
    }

    #[test]
    fn an_engine_that_fills_the_room_loses_its_oldest_image_and_sends_it_again() {
        let mut assets = Assets::new();
        let mut cache = Cache::new();
        // Ten images of nearly the largest size, one a frame, where eight
        // fit.
        let image = |k: u32| png_head(2048, 2048 - k);
        let mut first = Vec::new();
        for k in 0..10 {
            let a = id(&mut assets, &image(k));
            let (sent, lost) = round_trip(&mut assets, &mut cache, &drawing(&[a]));
            assert_eq!(sent, [a]);
            let expected = if k < 8 {
                vec![]
            } else {
                vec![first[k as usize - 8]]
            };
            assert_eq!(lost, expected, "frame {k}");
            first.push(a);
        }
        // The first image is lost, so it gets a new id and goes out again.
        let again = id(&mut assets, &image(0));
        assert_ne!(again, first[0]);
        let (sent, lost) = round_trip(&mut assets, &mut cache, &drawing(&[again]));
        assert_eq!((sent, lost), (vec![again], vec![first[2]]));
        assert!(cache.contains(again) && !cache.contains(first[0]));
        // An image that is still live keeps its id and does not go out.
        assert_eq!(id(&mut assets, &image(9)), first[9]);
        let (sent, _) = round_trip(&mut assets, &mut cache, &drawing(&[first[9]]));
        assert!(sent.is_empty());
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
    fn the_size_comes_from_the_frame_header_of_a_jpeg() {
        let jpeg = jpeg_head_of(&[app(0xe0, b"JFIF\0"), sof(640, 480)]);
        assert_eq!(
            head(&jpeg).map(|h| (h.format, h.size)),
            Some((Format::Jpeg, (640, 480)))
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
        assert_eq!((head.size, head.orientation), ((640, 480), 6));
        assert!(head.turned());
        let little = jpeg_head_of(&[exif(false, 8), sof(640, 480)]);
        assert_eq!(super::head(&little).unwrap().orientation, 8);
        let last = jpeg_head_of(&[exif(true, 6), sof(640, 480), exif(true, 3)]);
        assert_eq!(super::head(&last).unwrap().orientation, 3);
        let wrong = jpeg_head_of(&[exif(true, 9), sof(640, 480)]);
        assert_eq!(super::head(&wrong).unwrap().orientation, 1);
    }

    #[test]
    fn a_turned_jpeg_has_the_size_on_the_screen_and_the_footprint_of_the_file() {
        let jpeg = jpeg_head_of(&[exif(true, 6), sof(640, 480)]);
        let asset = Assets::new().image(&jpeg).unwrap();
        assert_eq!((asset.width, asset.height), (480, 640));
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
            Err(AssetError::TooManyPixels {
                width: 2049,
                height: 2048
            })
        );
        // A product of two large sides does not wrap.
        assert!(Footprint::of(&png_head(u32::MAX, u32::MAX)).is_err());
        assert_eq!(Footprint::of(b"GIF89a"), Err(AssetError::Unsupported));
    }

    #[test]
    fn a_load_refuses_an_asset_over_the_pixels_or_the_bytes_of_a_room() {
        let largest = Footprint::of(&png_head(2048, 2048)).unwrap();
        let mut load = Load::default();
        for _ in 0..8 {
            load = load.with(largest).unwrap();
        }
        let one = Footprint::new(Some((1, 1)), 1).unwrap();
        assert!(matches!(load.with(one), Err(AssetError::Full { .. })));
        assert_eq!(
            load.without(largest).with(one).unwrap().without(one),
            load.without(largest)
        );
        let heavy = Footprint::new(Some((1, 1)), MAX_LIVE_BYTES as usize).unwrap();
        let load = Load::default().with(heavy).unwrap();
        assert_eq!(
            load.with(one),
            Err(AssetError::Full {
                pixels: 2,
                bytes: MAX_LIVE_BYTES + 1
            })
        );
    }
}
