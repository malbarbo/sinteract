//! The limits on the assets of a room. The server keeps the live assets of
//! a room under them, and a local host keeps its display under them the
//! same way.
//!
//! A limit counts the pixels, since each view decodes an asset to four
//! bytes a pixel, and a small PNG can hold a large image. It also counts
//! the bytes, which the server keeps and sends to each view.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU32;

/// The most pixels of one image, 2048 by 2048.
pub const MAX_IMAGE_PIXELS: u64 = 2048 * 2048;

/// The most pixels of the live assets of a room, eight of the largest
/// images, which a view decodes to 128 MiB.
pub const MAX_LIVE_PIXELS: u64 = 8 * MAX_IMAGE_PIXELS;

/// The most bytes of the live assets of a room. It is under the cap of the
/// framing, so any asset under it fits in a message.
pub const MAX_LIVE_BYTES: u64 = 48 << 20;

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
