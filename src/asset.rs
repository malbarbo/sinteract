//! The images of a room. [`Assets`] is the table of the engine, which gives
//! an image its id and sends it before the first frame that draws it.
//! [`Cache`] keeps the live assets of a room under the limits, in the
//! server, and in a local host for its display.
//!
//! A limit counts the pixels, since each view decodes an asset to four
//! bytes a pixel, and a small PNG can hold a large image. It also counts
//! the bytes, which the server keeps and sends to each view.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::num::NonZeroU32;
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

    /// The image in `png`, with its id, or an error if it is not a PNG. An
    /// image with more than [`MAX_IMAGE_PIXELS`] shrinks to fit, with the
    /// feature `render`, up to [`MAX_SHRINK_PIXELS`], and is an error past
    /// that or without the feature.
    pub fn image(&mut self, png: &[u8]) -> Result<Asset, AssetError> {
        if let Some(&id) = self.ids.get(png) {
            let entry = self.images.get_mut(&id).expect("an id names an image");
            entry.seen = self.frames;
            return Ok(entry.asset);
        }
        let source = Arc::<[u8]>::from(png);
        let (width, height) = png_size(png).ok_or(AssetError::NotPng)?;
        let (png, (width, height)) = if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
            let shrunk = shrink(png, width, height)?;
            let size = png_size(&shrunk).expect("the shrunk image is a PNG");
            (Arc::from(shrunk), size)
        } else {
            (source.clone(), (width, height))
        };
        Footprint::new(Some((width, height)), png.len())?;
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
                sent: false,
                seen: self.frames,
            },
        );
        Ok(asset)
    }

    /// The id and the PNG of each image that `scene` draws and that has not
    /// gone out, which go out before the frame. A bitmap whose id did not
    /// come from [`Assets::image`] draws nothing, so it sends nothing.
    pub fn frame(&mut self, scene: &Scene) -> Vec<(u32, Arc<[u8]>)> {
        let mut send = Vec::new();
        for id in bitmap_ids(scene) {
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
        send
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

/// An image of [`Assets`], with its id and the size of the PNG that goes
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

impl Asset {
    /// The bitmap of the image, drawn into `rect`.
    pub fn fit(self, rect: RotatedRect) -> Bitmap {
        Bitmap::fit(self.id, self.width, self.height, rect)
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
/// that the last frame of some player draws stays, as does one that came
/// after the last frame, since the next frame draws it.
#[derive(Debug, Default)]
pub struct Cache {
    live: BTreeMap<u32, Live>,
    /// The ids that the last frame of each player draws, with 0 for the
    /// last frame for every player.
    shown: BTreeMap<u32, BTreeSet<u32>>,
    load: Load,
    /// How many frames came.
    frames: u64,
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the asset `id` is live, `false` otherwise.
    pub fn contains(&self, id: u32) -> bool {
        self.live.contains_key(&id)
    }

    /// Keep the asset `id`, which is not live, and return the ids of the
    /// assets that it drops to fit, the ones that the frames used longest
    /// ago first. Returns [`AssetError::Full`] and changes nothing if the
    /// asset does not fit even without every asset that may go.
    pub fn asset(&mut self, id: u32, footprint: Footprint) -> Result<Vec<u32>, AssetError> {
        assert!(
            !self.contains(id),
            "Cache::asset takes an id that is not live"
        );
        let shown: BTreeSet<u32> = self.shown.values().flatten().copied().collect();
        let mut may_go: Vec<(u64, u32)> = self
            .live
            .iter()
            .filter(|(id, live)| !shown.contains(id) && live.used < self.frames)
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
            },
        );
        Ok(dropped)
    }

    /// Say that a frame for `player`, or for every player when `player` is
    /// `None`, draws the assets of `ids`. An id that is not live changes
    /// nothing.
    pub fn frame(&mut self, player: Option<NonZeroU32>, ids: BTreeSet<u32>) {
        for id in &ids {
            if let Some(live) = self.live.get_mut(id) {
                live.used = self.frames;
            }
        }
        match player {
            None => {
                self.shown.clear();
                self.shown.insert(0, ids);
            }
            Some(player) => {
                self.shown.insert(player.get(), ids);
            }
        }
        self.frames += 1;
    }
}

/// What an asset counts toward the limits of a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Footprint {
    pixels: u64,
    bytes: u64,
}

impl Footprint {
    /// The footprint of `blob`, or an error if it is not a PNG or has more
    /// than [`MAX_IMAGE_PIXELS`].
    pub fn of(blob: &[u8]) -> Result<Footprint, AssetError> {
        Footprint::new(png_size(blob), blob.len())
    }

    /// The footprint of a blob of `bytes` bytes whose PNG header gives
    /// `size`, as [`png_size`] reads it.
    pub fn new(size: Option<(u32, u32)>, bytes: usize) -> Result<Footprint, AssetError> {
        let (width, height) = size.ok_or(AssetError::NotPng)?;
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
pub struct Load {
    pixels: u64,
    bytes: u64,
}

impl Load {
    /// The load with `asset` on top, or an error if that goes over
    /// [`MAX_LIVE_PIXELS`] or [`MAX_LIVE_BYTES`].
    pub fn with(self, asset: Footprint) -> Result<Load, AssetError> {
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
    pub fn without(self, asset: Footprint) -> Load {
        Load {
            pixels: self.pixels - asset.pixels,
            bytes: self.bytes - asset.bytes,
        }
    }
}

/// Why an asset breaks the limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetError {
    /// The blob does not start as a PNG does. A view decodes only PNG.
    NotPng,
    /// The image has more than [`MAX_IMAGE_PIXELS`].
    TooManyPixels { width: u32, height: u32 },
    /// The live assets of the room would hold these many pixels and bytes,
    /// over [`MAX_LIVE_PIXELS`] or [`MAX_LIVE_BYTES`].
    Full { pixels: u64, bytes: u64 },
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssetError::NotPng => f.write_str("the image is not a PNG"),
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

/// The width and the height from the header of the PNG in `blob`, or
/// `None` if `blob` does not start with the signature and the header of a
/// PNG, or gives a width or a height of 0. It reads 24 bytes and decodes
/// nothing.
pub fn png_size(blob: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    // The header is the first chunk, 13 bytes long.
    const HEADER: &[u8] = b"\0\0\0\x0dIHDR";
    let rest = blob.strip_prefix(SIGNATURE)?.strip_prefix(HEADER)?;
    let (width, rest) = rest.split_first_chunk::<4>()?;
    let (height, _) = rest.split_first_chunk::<4>()?;
    let (width, height) = (u32::from_be_bytes(*width), u32::from_be_bytes(*height));
    (width > 0 && height > 0).then_some((width, height))
}

/// A live asset of a [`Cache`].
#[derive(Debug)]
struct Live {
    footprint: Footprint,
    /// The count of frames when a frame last drew the asset, or when it
    /// came. One that came after the last frame has the count of frames.
    used: u64,
}

/// An image of [`Assets`].
#[derive(Debug)]
struct Entry {
    asset: Asset,
    /// The PNG from the front end, which finds the image again.
    source: Arc<[u8]>,
    /// The PNG that goes out, which is `source` unless it shrank.
    png: Arc<[u8]>,
    /// Whether the image went out, and the server has not lost it since.
    sent: bool,
    /// The count of frames at the last call of [`Assets::image`] for it.
    seen: u64,
}

/// `png`, of `width` by `height`, shrunk to at most [`MAX_IMAGE_PIXELS`]
/// with its ratio, or an error if it has more than [`MAX_SHRINK_PIXELS`]
/// or does not decode.
#[cfg(feature = "render")]
fn shrink(png: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AssetError> {
    if u64::from(width) * u64::from(height) > MAX_SHRINK_PIXELS {
        return Err(AssetError::TooManyPixels { width, height });
    }
    shrink_to(png, shrunk_size(width, height, MAX_IMAGE_PIXELS))
}

/// Without the renderer, an image over the limit cannot shrink.
#[cfg(not(feature = "render"))]
fn shrink(_png: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AssetError> {
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

/// `png` drawn at `size`, which is smaller, as a PNG. Each halving
/// averages four pixels, which a single scale of a large ratio would skip,
/// and a last scale reaches the size.
#[cfg(feature = "render")]
fn shrink_to(png: &[u8], (width, height): (u32, u32)) -> Result<Vec<u8>, AssetError> {
    let mut image = tiny_skia::Pixmap::decode_png(png).map_err(|_| AssetError::NotPng)?;
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
/// [`png_size`] reads.
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

    fn sent(send: &[(u32, Arc<[u8]>)]) -> Vec<u32> {
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
        let send = assets.frame(&drawing(&[a, b]));
        assert_eq!(sent(&send), [a, b]);
        assert_eq!(&*send[0].1, &png_head(4, 4)[..]);
        // An image that no frame draws for a while keeps its id.
        assert!(assets.frame(&drawing(&[])).is_empty());
        assert!(assets.frame(&drawing(&[])).is_empty());
        assert_eq!(id(&mut assets, &png_head(4, 4)), a);
        assert!(assets.frame(&drawing(&[a, b])).is_empty());
    }

    #[test]
    fn a_lost_image_gets_a_new_id_and_goes_out_again() {
        let mut assets = Assets::new();
        let a = id(&mut assets, &png_head(4, 4));
        assets.frame(&drawing(&[a]));
        assets.lost(a);
        assets.lost(99);
        let again = id(&mut assets, &png_head(4, 4));
        assert_ne!(again, a);
        assert_eq!(sent(&assets.frame(&drawing(&[a, again]))), [again]);
    }

    #[test]
    fn an_image_that_did_not_go_out_is_gone_after_two_frames() {
        let mut assets = Assets::new();
        let a = id(&mut assets, &png_head(4, 4));
        assets.frame(&drawing(&[]));
        assert_eq!(id(&mut assets, &png_head(4, 4)), a);
        assets.frame(&drawing(&[]));
        assets.frame(&drawing(&[]));
        assert_ne!(id(&mut assets, &png_head(4, 4)), a);
    }

    #[test]
    fn an_image_that_is_not_a_png_or_is_too_large_gets_no_id() {
        let mut assets = Assets::new();
        assert_eq!(assets.image(b"GIF89a"), Err(AssetError::NotPng));
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
        assert_eq!(assets.image(&png_head(2049, 2048)), Err(AssetError::NotPng));
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
        let png = shrink_to(&stripes.encode_png().unwrap(), (6, 3)).unwrap();
        assert_eq!(png_size(&png), Some((6, 3)));
        let small = tiny_skia::Pixmap::decode_png(&png).unwrap();
        let pixel = small.pixel(3, 1).unwrap();
        assert!((100..=155).contains(&pixel.red()), "{pixel:?}");
        assert!((100..=155).contains(&pixel.blue()), "{pixel:?}");
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
    fn a_bitmap_of_an_unknown_id_sends_nothing() {
        let mut assets = Assets::new();
        assert!(assets.frame(&drawing(&[42])).is_empty());
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
    fn full() -> Cache {
        let mut cache = Cache::new();
        for id in 0..8 {
            assert_eq!(cache.asset(id, largest()), Ok(vec![]));
        }
        cache.frame(None, ids(&[]));
        cache
    }

    #[test]
    fn a_full_cache_drops_the_asset_that_the_frames_used_longest_ago() {
        let mut cache = full();
        cache.frame(None, ids(&[0, 3]));
        cache.frame(None, ids(&[5]));
        cache.frame(None, ids(&[]));
        assert_eq!(cache.asset(8, largest()), Ok(vec![1]));
        assert_eq!(cache.asset(9, largest()), Ok(vec![2]));
        cache.frame(None, ids(&[8, 9]));
        assert_eq!(cache.asset(10, largest()), Ok(vec![4]));
        assert_eq!(cache.asset(11, largest()), Ok(vec![6]));
        assert_eq!(cache.asset(12, largest()), Ok(vec![7]));
        assert_eq!(cache.asset(13, largest()), Ok(vec![0]));
        assert!(!cache.contains(0));
        assert!(cache.contains(3));
    }

    #[test]
    fn an_asset_on_a_screen_or_for_the_next_frame_stays() {
        let mut cache = full();
        cache.frame(None, ids(&[0, 1]));
        cache.frame(NonZeroU32::new(2), ids(&[2, 3]));
        cache.frame(NonZeroU32::new(1), ids(&[4, 5]));
        // The frame for every player may still be on the screen of a player
        // with no frame of its own.
        assert_eq!(cache.asset(8, largest()), Ok(vec![6]));
        assert_eq!(cache.asset(9, largest()), Ok(vec![7]));
        // Every other asset is on a screen or came after the last frame.
        assert!(matches!(
            cache.asset(10, largest()),
            Err(AssetError::Full { .. })
        ));
        assert!(!cache.contains(10));
        cache.frame(None, ids(&[8]));
        assert_eq!(cache.asset(10, largest()), Ok(vec![0]));
    }

    #[test]
    fn an_asset_that_fits_drops_nothing_and_one_too_large_changes_nothing() {
        let mut cache = Cache::new();
        let small = Footprint::new(Some((10, 10)), 100).unwrap();
        assert_eq!(cache.asset(1, small), Ok(vec![]));
        cache.frame(None, ids(&[]));
        let heavy = Footprint::new(Some((1, 1)), MAX_LIVE_BYTES as usize).unwrap();
        assert!(cache.asset(2, heavy).is_ok_and(|gone| gone == [1]));
        cache.frame(None, ids(&[2]));
        assert!(cache.asset(3, small).is_err());
        assert!(cache.contains(2) && !cache.contains(3));
    }

    #[test]
    fn the_size_comes_from_the_header_of_a_png() {
        assert_eq!(png_size(&png_head(640, 480)), Some((640, 480)));
        assert_eq!(png_size(&png_head(640, 480)[..23]), None);
        assert_eq!(png_size(&png_head(0, 480)), None);
        assert_eq!(png_size(b"\xff\xd8\xff\xe0 a JPEG and more bytes"), None);
        let mut other_chunk = png_head(640, 480);
        other_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_size(&other_chunk), None);
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
        assert_eq!(Footprint::of(b"GIF89a"), Err(AssetError::NotPng));
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
