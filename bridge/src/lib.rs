//! Real desktop adapter. IronRDP owns every RDP wire operation.
pub mod audio;
pub mod broker;
pub mod clipboard;
mod gpu_capture;
pub(crate) mod keyboard;
pub mod login;
mod login_compose;
pub mod microphone;
pub mod network;
pub mod performance;
pub mod video;
pub mod wayland;

use ironrdp_server::{BitmapUpdate, DesktopSize, PixelFormat, ServerError, ServerErrorKind};
use std::{
    collections::VecDeque,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU16, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

pub const LISTEN_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 33991);
pub const DISPOSABLE_LISTEN_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 33991);
pub const MAX_DIMENSION: u32 = 4096;

/// Broker sessions accept remote clients; disposable test sessions stay local.
pub fn listen_addr(mode: &str, configured: Option<&str>) -> anyhow::Result<SocketAddr> {
    let default = match mode {
        "broker" => LISTEN_ADDR,
        "disposable" => DISPOSABLE_LISTEN_ADDR,
        _ => anyhow::bail!("NIRI_RDP_MODE must be broker or disposable"),
    };
    let addr = match configured {
        Some(value) => value.parse::<SocketAddr>().map_err(|_| {
            anyhow::anyhow!("NIRI_RDP_LISTEN must be a numeric IP address and port, such as 0.0.0.0:33991 or [::]:33991")
        })?,
        None => default,
    };
    anyhow::ensure!(addr.port() != 0, "NIRI_RDP_LISTEN port must not be zero");
    anyhow::ensure!(
        !addr.ip().is_multicast(),
        "NIRI_RDP_LISTEN must not be a multicast address"
    );
    anyhow::ensure!(
        mode != "disposable" || addr.ip().is_loopback(),
        "disposable mode requires a loopback NIRI_RDP_LISTEN address"
    );
    Ok(addr)
}

/// `NIRI_RDP_MICROPHONE`: `auto` (default) offers MS-RDPEAI audio input, so
/// mstsc's "Record from this computer" works; `off` offers no AUDIO_INPUT
/// channel and drops the broker's microphone descriptor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicrophoneMode {
    #[default]
    Auto,
    Off,
}
impl MicrophoneMode {
    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var("NIRI_RDP_MICROPHONE") {
            Ok(value) => Self::parse(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse(None),
            Err(_) => anyhow::bail!("NIRI_RDP_MICROPHONE must contain valid Unicode"),
        }
    }
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value {
            None | Some("auto") => Ok(Self::Auto),
            Some("off") => Ok(Self::Off),
            Some(_) => anyhow::bail!("NIRI_RDP_MICROPHONE must be auto or off"),
        }
    }
}

pub fn server_error(message: impl Into<String>) -> ServerError {
    ServerError::new("niri-rdp bridge", ServerErrorKind::Reason(message.into()))
}

/// RDP set-1 scancodes become Linux evdev keycodes; E0 is not an extra key.
pub fn evdev_key(code: u8, extended: bool) -> Option<u32> {
    if !extended {
        return match code {
            1..=83 | 86..=88 => Some(u32::from(code)),
            0x70 => Some(93),  // Hiragana/Katakana
            0x73 => Some(89),  // JIS Ro
            0x79 => Some(92),  // Henkan
            0x7b => Some(94),  // Muhenkan
            0x7d => Some(124), // JIS Yen
            _ => None,
        };
    }
    Some(match code {
        0x1c => 96,
        0x1d => 97,
        0x35 => 98,
        0x37 => 99,
        0x38 => 100,
        0x47 => 102,
        0x48 => 103,
        0x49 => 104,
        0x4b => 105,
        0x4d => 106,
        0x4f => 107,
        0x50 => 108,
        0x51 => 109,
        0x52 => 110,
        0x53 => 111,
        0x5b => 125,
        0x5c => 126,
        0x5d => 127,
        _ => return None,
    })
}

/// Validate compositor-provided memory layout before allocating or reading.
pub fn buffer_length(width: u32, height: u32, stride: u32) -> anyhow::Result<usize> {
    anyhow::ensure!(
        (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height),
        "capture dimensions outside 1..4096"
    );
    anyhow::ensure!(
        stride >= width * 4 && stride <= MAX_DIMENSION * 4,
        "invalid capture stride"
    );
    (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("capture overflow"))
}

/// Native BGR SHM already has the wire/encoder pixel order. Its unused alpha
/// byte can remain untouched when the bitmap declares BgrX32.
pub(crate) fn capture_bitmap(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
    target: DesktopSize,
    abgr: bool,
) -> anyhow::Result<BitmapUpdate> {
    if native_layout(width, height, stride, inverted, target, abgr) {
        return native_bitmap(bytes.to_vec().into(), width, height, stride, target);
    }
    bitmap(bytes, width, height, stride, inverted, target, abgr)
}

/// Like `capture_bitmap`, but a native layout shares `data` without copying.
/// The caller guarantees nobody writes `data` while any clone of it lives.
pub(crate) fn shared_capture_bitmap(
    data: bytes::Bytes,
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
    target: DesktopSize,
    abgr: bool,
) -> anyhow::Result<BitmapUpdate> {
    if native_layout(width, height, stride, inverted, target, abgr) {
        return native_bitmap(data, width, height, stride, target);
    }
    bitmap(&data, width, height, stride, inverted, target, abgr)
}

fn native_layout(
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
    target: DesktopSize,
    abgr: bool,
) -> bool {
    !inverted
        && !abgr
        && width == u32::from(target.width)
        && height == u32::from(target.height)
        && stride == width * 4
}

fn native_bitmap(
    data: bytes::Bytes,
    width: u32,
    height: u32,
    stride: u32,
    target: DesktopSize,
) -> anyhow::Result<BitmapUpdate> {
    anyhow::ensure!(
        data.len() == buffer_length(width, height, stride)?,
        "SHM length mismatch"
    );
    Ok(BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(target.width).unwrap(),
        height: NonZeroU16::new(target.height).unwrap(),
        format: PixelFormat::BgrX32,
        data,
        stride: NonZeroUsize::new(stride as usize).unwrap(),
    })
}

/// Strip row padding, normalize upside-down SHM and scale the desktop contract.
pub fn bitmap(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
    target: DesktopSize,
    abgr: bool,
) -> anyhow::Result<BitmapUpdate> {
    anyhow::ensure!(
        bytes.len() == buffer_length(width, height, stride)?,
        "SHM length mismatch"
    );
    anyhow::ensure!(
        (1..=MAX_DIMENSION as u16).contains(&target.width)
            && (1..=MAX_DIMENSION as u16).contains(&target.height),
        "RDP dimensions outside 1..4096"
    );
    let out_stride = usize::from(target.width) * 4;
    // Every output byte is supplied below; avoid zero-filling a second full
    // desktop allocation before copying the compositor's pixels into it.
    let mut data = Vec::with_capacity(out_stride * usize::from(target.height));
    for y in 0..u32::from(target.height) {
        let sy = y * height / u32::from(target.height);
        let sy = if inverted { height - 1 - sy } else { sy };
        if u32::from(target.width) == width {
            let src = (sy * stride) as usize;
            let dst = data.len();
            data.extend_from_slice(&bytes[src..src + out_stride]);
            let row = &mut data[dst..];
            for pixel in row.as_chunks_mut::<4>().0 {
                if abgr {
                    pixel.swap(0, 2);
                }
                pixel[3] = 255;
            }
            continue;
        }
        for x in 0..u32::from(target.width) {
            let sx = x * width / u32::from(target.width);
            let src = (sy * stride + sx * 4) as usize;
            let dst = data.len();
            data.extend_from_slice(&bytes[src..src + 4]);
            if abgr {
                data.swap(dst, dst + 2);
            }
            data[dst + 3] = 255;
        }
    }
    Ok(BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(target.width).unwrap(),
        height: NonZeroU16::new(target.height).unwrap(),
        format: PixelFormat::BgrA32,
        data: data.into(),
        stride: NonZeroUsize::new(out_stride).unwrap(),
    })
}

/// Desktop comparisons and damage use 64x64 pixel tiles.
pub(crate) const DAMAGE_TILE: usize = 64;

/// A set of damage tiles over one desktop size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TileSet {
    columns: usize,
    rows: usize,
    words: Vec<u64>,
}
impl TileSet {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        let columns = width.div_ceil(DAMAGE_TILE);
        let rows = height.div_ceil(DAMAGE_TILE);
        Self {
            columns,
            rows,
            words: vec![0; (columns * rows).div_ceil(64)],
        }
    }
    pub(crate) fn for_image(image: &BitmapUpdate) -> Self {
        Self::new(
            usize::from(image.width.get()),
            usize::from(image.height.get()),
        )
    }
    pub(crate) fn insert(&mut self, column: usize, row: usize) {
        debug_assert!(column < self.columns && row < self.rows);
        let index = row * self.columns + column;
        self.words[index / 64] |= 1 << (index % 64);
    }
    pub(crate) fn contains(&self, column: usize, row: usize) -> bool {
        let index = row * self.columns + column;
        column < self.columns
            && row < self.rows
            && self.words[index / 64] & (1 << (index % 64)) != 0
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }
    pub(crate) fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }
    /// Mark every tile a pixel rectangle touches, clipped to the desktop.
    pub(crate) fn insert_rect(&mut self, x: u32, y: u32, width: u32, height: u32) {
        let (left, top) = (x as usize, y as usize);
        let (first_column, first_row) = (left / DAMAGE_TILE, top / DAMAGE_TILE);
        if width == 0 || height == 0 || first_column >= self.columns || first_row >= self.rows {
            return;
        }
        let right = ((left + width as usize - 1) / DAMAGE_TILE).min(self.columns - 1);
        let bottom = ((top + height as usize - 1) / DAMAGE_TILE).min(self.rows - 1);
        for row in first_row..=bottom {
            for column in first_column..=right {
                self.insert(column, row);
            }
        }
    }
    fn same_grid(&self, other: &Self) -> bool {
        self.columns == other.columns && self.rows == other.rows
    }
    pub(crate) fn union_with(&mut self, other: &Self) {
        debug_assert!(self.same_grid(other));
        for (word, other) in self.words.iter_mut().zip(&other.words) {
            *word |= other;
        }
    }
    pub(crate) fn is_subset(&self, other: &Self) -> bool {
        self.same_grid(other)
            && self
                .words
                .iter()
                .zip(&other.words)
                .all(|(word, other)| word & !other == 0)
    }
    fn tiles(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        (0..self.rows)
            .flat_map(move |row| (0..self.columns).map(move |column| (column, row)))
            .filter(|&(column, row)| self.contains(column, row))
    }
}

/// Both desktops share one memory layout, so tiles compare byte for byte.
pub(crate) fn comparable(previous: &BitmapUpdate, image: &BitmapUpdate) -> bool {
    previous.width == image.width
        && previous.height == image.height
        && previous.format == image.format
        && previous.stride == image.stride
        && previous.data.len() == image.data.len()
}

/// The same immutable pixels: a heartbeat or refinement resends one snapshot.
fn same_buffer(previous: &BitmapUpdate, image: &BitmapUpdate) -> bool {
    previous.data.as_ptr() == image.data.as_ptr() && previous.data.len() == image.data.len()
}

/// A hint is usable only for the desktop size it was made for.
fn usable_hint<'a>(hint: Option<&'a TileSet>, image: &BitmapUpdate) -> Option<&'a TileSet> {
    hint.filter(|hint| hint.same_grid(&TileSet::for_image(image)))
}

fn tile_changed(previous: &BitmapUpdate, image: &BitmapUpdate, column: usize, row: usize) -> bool {
    let width = usize::from(image.width.get());
    let height = usize::from(image.height.get());
    let bpp = usize::from(image.format.bytes_per_pixel());
    let (x, y) = (column * DAMAGE_TILE, row * DAMAGE_TILE);
    let (w, h) = ((width - x).min(DAMAGE_TILE), (height - y).min(DAMAGE_TILE));
    (y..y + h).any(|line| {
        let start = line * image.stride.get() + x * bpp;
        previous.data[start..start + w * bpp] != image.data[start..start + w * bpp]
    })
}

/// Exactly the tiles whose pixels differ. Tiles outside `hint` are known to
/// be unchanged and are never read. Both desktops must be comparable.
pub(crate) fn changed_tiles(
    previous: &BitmapUpdate,
    image: &BitmapUpdate,
    hint: Option<&TileSet>,
) -> TileSet {
    debug_assert!(comparable(previous, image));
    let mut changed = TileSet::for_image(image);
    if same_buffer(previous, image) {
        return changed;
    }
    match usable_hint(hint, image) {
        Some(hint) => {
            for (column, row) in hint.tiles() {
                if tile_changed(previous, image, column, row) {
                    changed.insert(column, row);
                }
            }
        }
        None => {
            for row in 0..changed.rows {
                for column in 0..changed.columns {
                    if tile_changed(previous, image, column, row) {
                        changed.insert(column, row);
                    }
                }
            }
        }
    }
    changed
}

/// Whether two desktops have identical pixels. A shared buffer needs no
/// comparison; a hint limits the comparison to its tiles.
pub(crate) fn same_pixels(
    previous: &BitmapUpdate,
    image: &BitmapUpdate,
    hint: Option<&TileSet>,
) -> bool {
    if !comparable(previous, image) {
        return false;
    }
    if same_buffer(previous, image) {
        return true;
    }
    match usable_hint(hint, image) {
        Some(hint) => !hint
            .tiles()
            .any(|(column, row)| tile_changed(previous, image, column, row)),
        None => previous.data == image.data,
    }
}

/// Who owns a delivered snapshot's pixels. A pooled capture slot advances its
/// generation whenever the compositor may write it again.
#[derive(Clone)]
pub(crate) struct SnapshotOwner {
    pub(crate) generation: Arc<AtomicU64>,
    pub(crate) value: u64,
}
impl SnapshotOwner {
    fn current(&self) -> bool {
        self.generation.load(Ordering::Acquire) == self.value
    }
}

struct CaptureRecord {
    pointer: usize,
    length: usize,
    sequence: u64,
    captured_at: Instant,
    changes: Option<Arc<TileSet>>,
    owner: Option<SnapshotOwner>,
}
impl CaptureRecord {
    fn matches(&self, image: &BitmapUpdate) -> bool {
        self.pointer == image.data.as_ptr() as usize && self.length == image.data.len()
    }
}

/// Recently delivered desktops, newest last, with each one's changed tiles
/// relative to its predecessor. A consumer that still holds a delivered image
/// finds its record by pixel address. Addresses are reused, so change history
/// is trusted only for a pooled slot that has not been written since: a live
/// image in that slot can only be the record's own snapshot.
#[derive(Default)]
pub(crate) struct CaptureLog {
    records: VecDeque<CaptureRecord>,
    sequence: u64,
}
const CAPTURE_LOG_LENGTH: usize = 16;
impl CaptureLog {
    /// Record a newly delivered desktop. `changes` holds the tiles that may
    /// differ from the previously delivered desktop, or None when unknown.
    pub(crate) fn push(
        &mut self,
        image: &BitmapUpdate,
        captured_at: Instant,
        changes: Option<TileSet>,
        owner: Option<SnapshotOwner>,
    ) -> u64 {
        self.sequence += 1;
        if self.records.len() == CAPTURE_LOG_LENGTH {
            self.records.pop_front();
        }
        self.records.push_back(CaptureRecord {
            pointer: image.data.as_ptr() as usize,
            length: image.data.len(),
            sequence: self.sequence,
            captured_at,
            changes: changes.map(Arc::new),
            owner,
        });
        self.sequence
    }
    pub(crate) fn captured_at(&self, image: &BitmapUpdate) -> Option<Instant> {
        self.records
            .iter()
            .rev()
            .find(|record| record.matches(image))
            .map(|record| record.captured_at)
    }
    fn live(&self, image: &BitmapUpdate) -> Option<u64> {
        let record = self
            .records
            .iter()
            .rev()
            .find(|record| record.matches(image))?;
        record
            .owner
            .as_ref()
            .is_some_and(SnapshotOwner::current)
            .then_some(record.sequence)
    }
    /// Tiles that may differ between two delivered desktops both still held by
    /// the caller. None requires a full comparison.
    pub(crate) fn changes_between(
        &self,
        previous: &BitmapUpdate,
        image: &BitmapUpdate,
    ) -> Option<TileSet> {
        if !comparable(previous, image) {
            return None;
        }
        let base = self.live(previous)?;
        let current = self.live(image)?;
        if base > current {
            return None;
        }
        // Records are consecutive, so every step after a retained base exists.
        let mut union = TileSet::for_image(image);
        for record in self
            .records
            .iter()
            .filter(|record| (base + 1..=current).contains(&record.sequence))
        {
            let changes = record.changes.as_ref()?;
            if !changes.same_grid(&union) {
                return None;
            }
            union.union_with(changes);
        }
        Some(union)
    }
}

/// Compare complete retained desktops, so skipped capture notifications cannot
/// lose damage. At most 32 rectangular tiles are returned; broad damage uses a
/// full-frame update. Subregions share immutable Bytes without pixel copies.
pub(crate) fn bitmap_damage(
    previous: Option<&BitmapUpdate>,
    image: &BitmapUpdate,
) -> Vec<BitmapUpdate> {
    bitmap_damage_hinted(previous, image, None)
}

/// `bitmap_damage` that reads only the tiles in `hint`; every other tile is
/// known to be unchanged. Without a hint every tile is compared.
pub(crate) fn bitmap_damage_hinted(
    previous: Option<&BitmapUpdate>,
    image: &BitmapUpdate,
    hint: Option<&TileSet>,
) -> Vec<BitmapUpdate> {
    let Some(previous) = previous.filter(|old| comparable(old, image)) else {
        return vec![image.clone()];
    };
    if same_buffer(previous, image) {
        return Vec::new();
    }
    let hint = usable_hint(hint, image);
    match hint {
        Some(hint) if hint.is_empty() => return Vec::new(),
        None if previous.data == image.data => return Vec::new(),
        _ => {}
    }
    let width = usize::from(image.width.get());
    let height = usize::from(image.height.get());
    let mut regions: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut area = 0usize;
    for y in (0..height).step_by(DAMAGE_TILE) {
        let h = (height - y).min(DAMAGE_TILE);
        let mut run: Option<(usize, usize)> = None;
        for x in (0..width)
            .step_by(DAMAGE_TILE)
            .chain(std::iter::once(width))
        {
            let w = (width - x).min(DAMAGE_TILE);
            let (column, row) = (x / DAMAGE_TILE, y / DAMAGE_TILE);
            let changed = x != width
                && hint.is_none_or(|hint| hint.contains(column, row))
                && tile_changed(previous, image, column, row);
            if changed {
                area += w * h;
                if area * 2 >= width * height {
                    return vec![image.clone()];
                }
                run = Some(run.map_or((x, w), |(start, length)| (start, length + w)));
            } else if let Some((start, length)) = run.take() {
                if let Some(rect) = regions
                    .iter_mut()
                    .rev()
                    .find(|rect| rect.0 == start && rect.2 == length && rect.1 + rect.3 == y)
                {
                    rect.3 += h;
                } else {
                    if regions.len() == 32 {
                        return vec![image.clone()];
                    }
                    regions.push((start, y, length, h));
                }
            }
        }
    }
    regions
        .into_iter()
        .map(|(x, y, width, height)| {
            image
                .sub(
                    x as u16,
                    y as u16,
                    NonZeroU16::new(width as u16).unwrap(),
                    NonZeroU16::new(height as u16).unwrap(),
                )
                .unwrap()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_bitmap_damage_accumulates_skipped_and_overlapping_changes() {
        let target = DesktopSize {
            width: 256,
            height: 256,
        };
        let mut pixels = vec![0u8; 256 * 256 * 4];
        let initial = capture_bitmap(&pixels, 256, 256, 1024, false, target, false).unwrap();
        assert!(bitmap_damage(Some(&initial), &initial).is_empty());
        pixels[4] = 1;
        // An intermediate capture changes one tile but is never delivered.
        let skipped = capture_bitmap(&pixels, 256, 256, 1024, false, target, false).unwrap();
        pixels[4] = 2;
        pixels[(130 * 256 + 130) * 4] = 3;
        let current = capture_bitmap(&pixels, 256, 256, 1024, false, target, false).unwrap();
        let damage = bitmap_damage(Some(&initial), &current);
        assert_eq!(damage.len(), 2);
        assert_eq!(
            (
                damage[0].x,
                damage[0].y,
                damage[0].width.get(),
                damage[0].height.get()
            ),
            (0, 0, 64, 64)
        );
        assert_eq!((damage[1].x, damage[1].y), (128, 128));
        assert_eq!(damage[0].data[4], 2);
        assert_eq!(skipped.data[4], 1);
        assert_eq!(damage[1].data[(2 * 256 + 2) * 4], 3);
        let mut broad = pixels.clone();
        broad.fill(99);
        let full = capture_bitmap(&broad, 256, 256, 1024, false, target, false).unwrap();
        let fallback = bitmap_damage(Some(&current), &full);
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].width.get(), 256);
        assert_eq!(fallback[0].height.get(), 256);
    }
    fn solid(width: u16, height: u16, value: u8) -> BitmapUpdate {
        let target = DesktopSize { width, height };
        let pixels = vec![value; usize::from(width) * usize::from(height) * 4];
        let stride = u32::from(width) * 4;
        capture_bitmap(
            &pixels,
            width.into(),
            height.into(),
            stride,
            false,
            target,
            false,
        )
        .unwrap()
    }
    fn with_pixel(image: &BitmapUpdate, x: usize, y: usize, value: u8) -> BitmapUpdate {
        let mut data = image.data.to_vec();
        data[y * image.stride.get() + x * 4] = value;
        BitmapUpdate {
            data: data.into(),
            ..image.clone()
        }
    }
    fn tiles(width: usize, height: usize, members: &[(usize, usize)]) -> TileSet {
        let mut set = TileSet::new(width, height);
        for &(column, row) in members {
            set.insert(column, row);
        }
        set
    }
    #[test]
    fn damage_rectangles_cover_every_touched_tile_and_clip_to_the_desktop() {
        let mut set = TileSet::new(130, 70);
        set.insert_rect(63, 0, 2, 1);
        assert_eq!(set, tiles(130, 70, &[(0, 0), (1, 0)]));
        set.insert_rect(128, 64, 100, 100);
        assert_eq!(set, tiles(130, 70, &[(0, 0), (1, 0), (2, 1)]));
        let mut empty = TileSet::new(130, 70);
        empty.insert_rect(200, 0, 10, 10);
        empty.insert_rect(0, 0, 0, 10);
        empty.insert_rect(0, 0, 10, 0);
        assert!(empty.is_empty());
        assert!(tiles(130, 70, &[(1, 0)]).is_subset(&set));
        assert!(!set.is_subset(&tiles(130, 70, &[(1, 0)])));
        assert_eq!(set.len(), 3);
    }
    #[test]
    fn damage_hint_limits_comparison_to_reported_tiles() {
        let base = solid(192, 192, 0);
        let current = with_pixel(&with_pixel(&base, 1, 1, 7), 130, 130, 9);
        let exact = changed_tiles(&base, &current, None);
        assert_eq!(exact, tiles(192, 192, &[(0, 0), (2, 2)]));
        // Tiles outside the hint are trusted to be unchanged and never read.
        let hint = tiles(192, 192, &[(0, 0), (1, 1)]);
        assert_eq!(
            changed_tiles(&base, &current, Some(&hint)),
            tiles(192, 192, &[(0, 0)])
        );
        assert!(same_pixels(&base, &current, Some(&TileSet::new(192, 192))));
        assert!(!same_pixels(&base, &current, Some(&hint)));
        assert!(!same_pixels(&base, &current, None));
        // A complete hint produces exactly the unhinted damage regions.
        let regions = |hint: Option<&TileSet>| {
            bitmap_damage_hinted(Some(&base), &current, hint)
                .iter()
                .map(|region| (region.x, region.y, region.width.get(), region.height.get()))
                .collect::<Vec<_>>()
        };
        assert_eq!(regions(Some(&exact)), regions(None));
        assert_eq!(regions(None), vec![(0, 0, 64, 64), (128, 128, 64, 64)]);
        assert!(regions(Some(&TileSet::new(192, 192))).is_empty());
        // A hint for another desktop size is ignored rather than misapplied.
        let foreign = TileSet::new(64, 64);
        assert_eq!(regions(Some(&foreign)), regions(None));
        assert_eq!(changed_tiles(&base, &current, Some(&foreign)), exact);
    }
    #[test]
    fn shared_snapshot_buffer_is_equal_without_reading_pixels() {
        let image = solid(128, 64, 3);
        let heartbeat = image.clone();
        assert!(same_pixels(&image, &heartbeat, None));
        assert!(bitmap_damage(Some(&image), &heartbeat).is_empty());
        assert!(changed_tiles(&image, &heartbeat, None).is_empty());
        let copy = with_pixel(&image, 0, 0, 3);
        assert!(same_pixels(&image, &copy, None));
        let resized = solid(64, 64, 3);
        assert!(!same_pixels(&image, &resized, None));
        assert_eq!(bitmap_damage(Some(&image), &resized).len(), 1);
    }
    #[test]
    fn native_shared_capture_is_zero_copy_and_converted_capture_owns_pixels() {
        let target = DesktopSize {
            width: 2,
            height: 1,
        };
        let data = bytes::Bytes::from(vec![1, 2, 3, 0, 4, 5, 6, 17]);
        let native = shared_capture_bitmap(data.clone(), 2, 1, 8, false, target, false).unwrap();
        assert_eq!(native.data.as_ptr(), data.as_ptr());
        let converted = shared_capture_bitmap(data.clone(), 2, 1, 8, false, target, true).unwrap();
        assert_ne!(converted.data.as_ptr(), data.as_ptr());
        assert_eq!(&converted.data[..], &[3, 2, 1, 255, 6, 5, 4, 255]);
        assert!(shared_capture_bitmap(data.slice(..4), 2, 1, 8, false, target, false).is_err());
    }
    #[test]
    fn capture_log_unions_history_and_rejects_unknown_or_rewritten_snapshots() {
        let now = Instant::now();
        let owner = |generation: &Arc<AtomicU64>| {
            Some(SnapshotOwner {
                generation: generation.clone(),
                value: generation.load(Ordering::Acquire),
            })
        };
        let generations: Vec<_> = (0..5).map(|_| Arc::new(AtomicU64::new(1))).collect();
        let images: Vec<_> = (0..5).map(|value| solid(128, 128, value)).collect();
        let mut log = CaptureLog::default();
        log.push(&images[0], now, None, owner(&generations[0]));
        log.push(
            &images[1],
            now,
            Some(tiles(128, 128, &[(0, 0)])),
            owner(&generations[1]),
        );
        log.push(
            &images[2],
            now,
            Some(tiles(128, 128, &[(1, 1)])),
            owner(&generations[2]),
        );
        assert_eq!(
            log.changes_between(&images[0], &images[2]),
            Some(tiles(128, 128, &[(0, 0), (1, 1)]))
        );
        assert_eq!(
            log.changes_between(&images[1], &images[2]),
            Some(tiles(128, 128, &[(1, 1)]))
        );
        assert_eq!(
            log.changes_between(&images[2], &images[2]),
            Some(TileSet::new(128, 128))
        );
        assert_eq!(log.changes_between(&images[2], &images[0]), None);
        assert_eq!(log.captured_at(&images[1]), Some(now));
        // Unknown changes in the range force a full comparison.
        log.push(&images[3], now, None, owner(&generations[3]));
        assert_eq!(log.changes_between(&images[1], &images[3]), None);
        // A converted or copied snapshot has no slot to vouch for its address.
        log.push(&images[4], now, Some(TileSet::new(128, 128)), None);
        assert_eq!(log.changes_between(&images[3], &images[4]), None);
        // Once a slot may be rewritten, its address no longer identifies it.
        generations[2].fetch_add(1, Ordering::AcqRel);
        assert_eq!(log.changes_between(&images[1], &images[2]), None);
        // An image that was never delivered is unknown.
        assert_eq!(log.changes_between(&images[1], &solid(128, 128, 9)), None);
        // A base older than the retained history is unknown.
        let latest = Arc::new(AtomicU64::new(1));
        let fresh: Vec<_> = (0..CAPTURE_LOG_LENGTH)
            .map(|value| solid(128, 128, value as u8))
            .collect();
        for image in &fresh {
            log.push(image, now, Some(TileSet::new(128, 128)), owner(&latest));
        }
        assert_eq!(log.changes_between(&images[0], &fresh[0]), None);
        assert_eq!(
            log.changes_between(&fresh[0], &fresh[CAPTURE_LOG_LENGTH - 1]),
            Some(TileSet::new(128, 128))
        );
    }
    #[test]
    fn scancode_extended_keys_are_distinct_and_unknown_is_rejected() {
        assert_eq!(evdev_key(0x1d, false), Some(29));
        assert_eq!(evdev_key(0x1d, true), Some(97));
        assert_eq!(evdev_key(0x48, true), Some(103));
        assert_eq!(evdev_key(0xff, true), None);
        assert_eq!(evdev_key(0, false), None);
    }
    #[test]
    fn japanese_scancodes_reach_distinct_evdev_keys() {
        for (scan, evdev) in [(0x70, 93), (0x73, 89), (0x79, 92), (0x7b, 94), (0x7d, 124)] {
            assert_eq!(evdev_key(scan, false), Some(evdev));
            assert_eq!(evdev_key(scan, true), None);
        }
    }
    #[test]
    fn shm_bounds_reject_malicious_dimensions_and_stride() {
        for (w, h, s) in [
            (0, 1, 4),
            (4097, 1, 16388),
            (1, 0, 4),
            (4, 1, 15),
            (1, 1, u32::MAX),
        ] {
            assert!(buffer_length(w, h, s).is_err());
        }
        assert_eq!(buffer_length(1920, 1080, 7680).unwrap(), 8_294_400);
    }
    #[test]
    fn bitmap_normalizes_padding_inversion_and_abgr() {
        let bytes = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, 7, 8, 9, 0, 10, 11, 12, 0, 8, 8, 8, 8,
        ];
        let result = bitmap(
            &bytes,
            2,
            2,
            12,
            true,
            DesktopSize {
                width: 2,
                height: 2,
            },
            true,
        )
        .unwrap();
        assert_eq!(
            &result.data[..],
            &[9, 8, 7, 255, 12, 11, 10, 255, 3, 2, 1, 255, 6, 5, 4, 255]
        );
    }
    #[test]
    fn native_capture_preserves_pixels_and_declares_unused_alpha() {
        let bytes = [1, 2, 3, 0, 4, 5, 6, 17];
        let target = DesktopSize {
            width: 2,
            height: 1,
        };
        let native = capture_bitmap(&bytes, 2, 1, 8, false, target, false).unwrap();
        assert_eq!(native.format, PixelFormat::BgrX32);
        assert_eq!(&native.data[..], &bytes);
        // Snapshot ownership must survive the reusable SHM buffer's next write.
        let mut shm = bytes.to_vec();
        let owned = capture_bitmap(&shm, 2, 1, 8, false, target, false).unwrap();
        shm.fill(99);
        assert_eq!(&owned.data[..], &bytes);
        let padded = [1, 2, 3, 0, 4, 5, 6, 17, 99, 99, 99, 99];
        let normalized = capture_bitmap(&padded, 2, 1, 12, false, target, false).unwrap();
        assert_eq!(normalized.format, PixelFormat::BgrA32);
        assert_eq!(&normalized.data[..], &[1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(capture_bitmap(&bytes[..4], 2, 1, 8, false, target, false).is_err());
    }
    #[test]
    fn bitmap_row_copy_and_scaled_paths_preserve_pixel_contract() {
        let bytes = [1, 2, 3, 0, 4, 5, 6, 17, 99, 99, 99, 99];
        let direct = bitmap(
            &bytes,
            2,
            1,
            12,
            false,
            DesktopSize {
                width: 2,
                height: 1,
            },
            false,
        )
        .unwrap();
        assert_eq!(&direct.data[..], &[1, 2, 3, 255, 4, 5, 6, 255]);
        let scaled = bitmap(
            &bytes,
            2,
            1,
            12,
            false,
            DesktopSize {
                width: 4,
                height: 2,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            &scaled.data[..],
            &[
                1, 2, 3, 255, 1, 2, 3, 255, 4, 5, 6, 255, 4, 5, 6, 255, 1, 2, 3, 255, 1, 2, 3, 255,
                4, 5, 6, 255, 4, 5, 6, 255
            ]
        );
    }
    #[test]
    fn broker_listener_defaults_to_direct_connections_and_accepts_custom_addresses() {
        assert_eq!(
            listen_addr("broker", None).unwrap().to_string(),
            "0.0.0.0:33991"
        );
        for address in ["192.168.1.20:3389", "127.0.0.1:33991", "[::]:33991"] {
            assert_eq!(
                listen_addr("broker", Some(address)).unwrap().to_string(),
                address
            );
        }
    }
    #[test]
    fn disposable_listener_never_accepts_non_loopback_configuration() {
        assert_eq!(
            listen_addr("disposable", None).unwrap().to_string(),
            "127.0.0.1:33991"
        );
        for address in ["127.0.0.1:33992", "[::1]:33992"] {
            assert!(listen_addr("disposable", Some(address)).is_ok());
        }
        for address in ["0.0.0.0:33991", "192.168.1.20:33991", "[::]:33991"] {
            assert!(listen_addr("disposable", Some(address)).is_err());
        }
    }
    #[test]
    fn listener_rejects_invalid_configuration() {
        assert!(listen_addr("unknown", None).is_err());
        for address in [
            "",
            "localhost:33991",
            "0.0.0.0",
            "0.0.0.0:0",
            "0.0.0.0:65536",
            "224.0.0.1:33991",
            "[ff02::1]:33991",
        ] {
            assert!(listen_addr("broker", Some(address)).is_err());
        }
    }
    #[test]
    fn microphone_setting_parses_auto_off_and_rejects_others() {
        assert_eq!(MicrophoneMode::parse(None).unwrap(), MicrophoneMode::Auto);
        assert_eq!(MicrophoneMode::default(), MicrophoneMode::Auto);
        assert_eq!(
            MicrophoneMode::parse(Some("auto")).unwrap(),
            MicrophoneMode::Auto
        );
        assert_eq!(
            MicrophoneMode::parse(Some("off")).unwrap(),
            MicrophoneMode::Off
        );
        for invalid in ["", "on", "Off", "AUTO", " auto", "0", "no"] {
            let error = MicrophoneMode::parse(Some(invalid)).unwrap_err();
            assert_eq!(error.to_string(), "NIRI_RDP_MICROPHONE must be auto or off");
        }
    }
}
