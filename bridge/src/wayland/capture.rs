//! Pooled SHM capture slots and imported GPU (DMA-BUF) capture buffers.
use super::Commands;
use super::input::Input;
use super::output::{pump_wake, service_input};
use super::state::State;
use crate::{SnapshotOwner, TileSet, buffer_length};
use ironrdp_server::{BitmapUpdate, DesktopSize};
use std::{
    fs::File,
    os::fd::{AsFd, AsRawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1;

pub(super) type ShmLayout = (wl_shm::Format, u32, u32, u32);

/// One read-only SHM mapping the compositor writes into. Snapshots share it
/// without copying through `Bytes::from_owner`, which keeps an `Arc` clone
/// alive for as long as any slice of the snapshot lives. The compositor may
/// write the slot again only once the pool holds its sole reference.
pub(super) struct ShmSlot {
    mapping: *mut libc::c_void,
    length: usize,
    /// Advanced whenever the compositor may write the slot; a capture log
    /// record is trusted only while its generation is still current.
    generation: Arc<AtomicU64>,
    file: File,
}
// SAFETY: this process maps the slot read-only. The compositor writes it only
// while no snapshot references it (strong count one, checked by the single
// capture worker, which alone creates references).
unsafe impl Send for ShmSlot {}
unsafe impl Sync for ShmSlot {}
impl ShmSlot {
    fn new(length: usize) -> anyhow::Result<Self> {
        let file = tempfile::tempfile()?;
        file.set_len(length as u64)?;
        // SAFETY: file is at least length bytes and remains open for this mapping.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        anyhow::ensure!(
            mapping != libc::MAP_FAILED,
            "capture mmap failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self {
            mapping,
            length,
            generation: Arc::new(AtomicU64::new(0)),
            file,
        })
    }
    /// Call only after ready and before the slot is submitted again.
    fn bytes(&self) -> &[u8] {
        // SAFETY: mapping covers length readable bytes for this object's lifetime.
        // Its caller observes the screencopy ready barrier before reading.
        unsafe { std::slice::from_raw_parts(self.mapping.cast(), self.length) }
    }
}
impl Drop for ShmSlot {
    fn drop(&mut self) {
        self.generation.store(u64::MAX, Ordering::Release);
        // SAFETY: this object owns the exact mapping returned by mmap.
        unsafe { libc::munmap(self.mapping, self.length) };
    }
}

/// The `Bytes` owner of a zero-copy snapshot.
struct SlotView(Arc<ShmSlot>);
impl AsRef<[u8]> for SlotView {
    fn as_ref(&self) -> &[u8] {
        self.0.bytes()
    }
}

struct WaylandBuffer {
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
}
impl Drop for WaylandBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// A slot and the Wayland buffer through which the compositor writes it.
/// Dropping it stops compositor writes; snapshots keep the mapping alive.
pub(super) struct CaptureBuffer {
    slot: Arc<ShmSlot>,
    wayland: Option<WaylandBuffer>,
}
impl CaptureBuffer {
    pub(super) fn new(
        shm: &wl_shm::WlShm,
        layout: ShmLayout,
        h: &QueueHandle<State>,
    ) -> anyhow::Result<Self> {
        let (format, width, height, stride) = layout;
        let slot = ShmSlot::new(buffer_length(width, height, stride)?)?;
        let pool = shm.create_pool(slot.file.as_fd(), slot.length as i32, h, ());
        let buffer =
            pool.create_buffer(0, width as i32, height as i32, stride as i32, format, h, ());
        Ok(Self {
            slot: Arc::new(slot),
            wayland: Some(WaylandBuffer { buffer, pool }),
        })
    }
    fn busy(&self) -> bool {
        Arc::strong_count(&self.slot) > 1
    }
    /// The Wayland buffer the compositor writes, unless detached in tests.
    pub(super) fn buffer(&self) -> Option<&wl_buffer::WlBuffer> {
        self.wayland.as_ref().map(|wayland| &wayland.buffer)
    }
    /// The slot's pixels. Read only after the compositor reported the copy
    /// ready and before the buffer is submitted again.
    pub(super) fn pixels(&self) -> &[u8] {
        self.slot.bytes()
    }
    /// Write the slot as the compositor would.
    #[cfg(test)]
    pub(super) fn write_as_compositor(&self, pixels: &[u8]) {
        use std::os::unix::fs::FileExt;
        self.slot.file.write_all_at(pixels, 0).unwrap();
    }
}

/// Where the compositor writes the next SHM capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShmTarget {
    /// A pooled slot; native snapshots of it share its pixels.
    Shared(usize),
    /// The private overflow buffer; its snapshots are always copies.
    Scratch,
}

/// Shared slots, like the GPU pool. Diff bases and the encoder normally hold
/// at most two snapshots, so a third slot keeps capture from waiting.
pub(super) const SHARED_SLOTS: usize = 3;

/// SHM capture buffers for one layout. A slot still referenced by a delivered
/// snapshot (the latest desktop, a diff base or an encoder input) is never
/// handed back to the compositor; when every slot is held, capture uses a
/// private scratch buffer and copies its pixels, as before pooling.
pub(super) struct CapturePool {
    pub(super) layout: ShmLayout,
    slots: Vec<CaptureBuffer>,
    scratch: Option<CaptureBuffer>,
}
impl CapturePool {
    pub(super) fn new(layout: ShmLayout) -> Self {
        Self {
            layout,
            slots: Vec::new(),
            scratch: None,
        }
    }
    /// Choose a buffer nobody reads and advance its generation.
    pub(super) fn acquire(
        &mut self,
        mut create: impl FnMut() -> anyhow::Result<CaptureBuffer>,
    ) -> anyhow::Result<ShmTarget> {
        let target = if let Some(index) = self.slots.iter().position(|buffer| !buffer.busy()) {
            ShmTarget::Shared(index)
        } else if self.slots.len() < SHARED_SLOTS {
            self.slots.push(create()?);
            ShmTarget::Shared(self.slots.len() - 1)
        } else {
            if self.scratch.is_none() {
                self.scratch = Some(create()?);
            }
            ShmTarget::Scratch
        };
        // The last snapshot clone released the slot with a Release decrement;
        // order its reads before the compositor may write the slot again.
        std::sync::atomic::fence(Ordering::Acquire);
        self.buffer(target)
            .slot
            .generation
            .fetch_add(1, Ordering::AcqRel);
        Ok(target)
    }
    fn buffer(&self, target: ShmTarget) -> &CaptureBuffer {
        match target {
            ShmTarget::Shared(index) => &self.slots[index],
            ShmTarget::Scratch => self.scratch.as_ref().unwrap(),
        }
    }
    pub(super) fn wl_buffer(&self, target: ShmTarget) -> Option<&wl_buffer::WlBuffer> {
        self.buffer(target)
            .wayland
            .as_ref()
            .map(|wayland| &wayland.buffer)
    }
    /// Snapshot a ready capture. A native layout in a shared slot is zero-copy
    /// and returns the owner that vouches for the slot's change history.
    pub(super) fn snapshot(
        &self,
        target: ShmTarget,
        (width, height, stride, inverted): (u32, u32, u32, bool),
        desktop: DesktopSize,
        abgr: bool,
    ) -> anyhow::Result<(BitmapUpdate, Option<SnapshotOwner>)> {
        let slot = &self.buffer(target).slot;
        if target == ShmTarget::Scratch {
            let image = crate::capture_bitmap(
                slot.bytes(),
                width,
                height,
                stride,
                inverted,
                desktop,
                abgr,
            )?;
            return Ok((image, None));
        }
        let data = bytes::Bytes::from_owner(SlotView(slot.clone()));
        let image =
            crate::shared_capture_bitmap(data, width, height, stride, inverted, desktop, abgr)?;
        // A converted desktop is a separate allocation and releases the slot.
        let owner = (Arc::strong_count(slot) > 1).then(|| SnapshotOwner {
            generation: slot.generation.clone(),
            value: slot.generation.load(Ordering::Acquire),
        });
        Ok((image, owner))
    }
    #[cfg(test)]
    fn slots(&self) -> usize {
        self.slots.len() + usize::from(self.scratch.is_some())
    }
}

/// Damage reported for one capture as the tiles it may have changed. None
/// when the report cannot bound the change.
pub(super) fn damage_tiles(
    image: &BitmapUpdate,
    damage: &[(u32, u32, u32, u32)],
) -> Option<TileSet> {
    if damage.is_empty() {
        return None;
    }
    let mut tiles = TileSet::for_image(image);
    for &(x, y, width, height) in damage {
        tiles.insert_rect(x, y, width, height);
    }
    Some(tiles)
}

/// Changed-tile comparisons that use the capture log instead of reading the
/// complete desktops on the RDP runtime thread.
impl super::Desktop {
    fn capture_changes(&self, previous: &BitmapUpdate, image: &BitmapUpdate) -> Option<TileSet> {
        self.inner
            .capture_metadata
            .lock()
            .unwrap()
            .as_ref()?
            .changes_between(previous, image)
    }
    /// `bitmap_damage` limited to the tiles the capture worker saw change.
    pub(crate) fn capture_damage(
        &self,
        previous: Option<&BitmapUpdate>,
        image: &BitmapUpdate,
    ) -> Vec<BitmapUpdate> {
        let hint = previous.and_then(|previous| self.capture_changes(previous, image));
        crate::bitmap_damage_hinted(previous, image, hint.as_ref())
    }
    /// Pixel equality limited to the tiles the capture worker saw change.
    pub(crate) fn same_capture(&self, previous: &BitmapUpdate, image: &BitmapUpdate) -> bool {
        let hint = self.capture_changes(previous, image);
        crate::same_pixels(previous, image, hint.as_ref())
    }
}

pub(super) struct GpuBuffer {
    pub(super) owner: Arc<crate::gpu_capture::Buffer>,
    pub(super) buffer: wl_buffer::WlBuffer,
}
impl Drop for GpuBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}
pub(super) struct InputWait<'a> {
    pub(super) commands: &'a Commands,
    pub(super) input: &'a mut Input,
    pub(super) clipboard: &'a mut Option<crate::clipboard::ClipboardAttachment>,
    pub(super) alive: &'a AtomicBool,
}
pub(super) fn import_gpu_buffer(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    owner: Arc<crate::gpu_capture::Buffer>,
    size: (u32, u32),
    wait: InputWait<'_>,
) -> anyhow::Result<Option<GpuBuffer>> {
    let (width, height) = size;
    let InputWait {
        commands,
        input,
        clipboard,
        alive,
    } = wait;
    let manager = s
        .dmabuf
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("DMA-BUF capture protocol unavailable"))?;
    anyhow::ensure!(
        s.dmabuf_xrgb_modifiers.contains(&owner.modifier),
        "XRGB capture modifier not advertised"
    );
    let h = q.handle();
    let params = manager.create_params(&h, ());
    let fd = owner.fd()?;
    params.add(
        fd.as_fd(),
        0,
        owner.offset,
        owner.stride,
        (owner.modifier >> 32) as u32,
        owner.modifier as u32,
    );
    s.dmabuf_params = Some(params.id());
    s.dmabuf_created = None;
    s.dmabuf_failed = false;
    // Asynchronous creation fails gracefully; create_immed could terminate the
    // complete Wayland connection when the renderer cannot import a layout.
    params.create(
        width as i32,
        height as i32,
        0x34325258,
        zwp_linux_buffer_params_v1::Flags::empty(),
    );
    let deadline = Instant::now();
    let result = (|| -> anyhow::Result<Option<wl_buffer::WlBuffer>> {
        loop {
            if !alive.load(Ordering::Acquire) || service_input(commands, input, c, clipboard, s) {
                return Ok(None);
            }
            pump_wake(
                c,
                q,
                s,
                3000 - deadline.elapsed().as_millis().min(3000) as i32,
                Some(commands),
            )?;
            input.update_keymap(s)?;
            if let Some(buffer) = s.dmabuf_created.take() {
                return Ok(Some(buffer));
            }
            anyhow::ensure!(
                !s.dmabuf_failed && deadline.elapsed() < Duration::from_secs(3),
                "GPU capture import rejected or timed out"
            );
        }
    })();
    params.destroy();
    s.dmabuf_params = None;
    let buffer = result?;
    if buffer.is_none()
        && let Some(buffer) = s.dmabuf_created.take()
    {
        buffer.destroy();
    }
    Ok(buffer.map(|buffer| GpuBuffer { owner, buffer }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    const WIDTH: u32 = 128;
    const HEIGHT: u32 = 128;
    const STRIDE: u32 = WIDTH * 4;
    const DESKTOP: DesktopSize = DesktopSize {
        width: WIDTH as u16,
        height: HEIGHT as u16,
    };

    /// A slot without a Wayland buffer; the test writes it as the compositor.
    fn detached() -> anyhow::Result<CaptureBuffer> {
        Ok(CaptureBuffer {
            slot: Arc::new(ShmSlot::new((STRIDE * HEIGHT) as usize)?),
            wayland: None,
        })
    }
    fn compositor_writes(pool: &CapturePool, target: ShmTarget, value: u8) {
        let pixels = vec![value; (STRIDE * HEIGHT) as usize];
        pool.buffer(target)
            .slot
            .file
            .write_all_at(&pixels, 0)
            .unwrap();
    }
    fn capture(
        pool: &mut CapturePool,
        value: u8,
    ) -> (ShmTarget, BitmapUpdate, Option<SnapshotOwner>) {
        let target = pool.acquire(detached).unwrap();
        compositor_writes(pool, target, value);
        let (image, owner) = pool
            .snapshot(target, (WIDTH, HEIGHT, STRIDE, false), DESKTOP, false)
            .unwrap();
        (target, image, owner)
    }
    fn filled(image: &BitmapUpdate, value: u8) -> bool {
        image.data.iter().all(|byte| *byte == value)
    }
    fn pool() -> CapturePool {
        CapturePool::new((wl_shm::Format::Xrgb8888, WIDTH, HEIGHT, STRIDE))
    }

    #[test]
    fn pooled_slot_is_never_rewritten_while_a_diff_base_holds_it() {
        let mut pool = pool();
        let (first, base, owner) = capture(&mut pool, 1);
        assert!(owner.is_some(), "a native snapshot shares its slot");
        // The bases the RDP side keeps: VideoUpdates::last_image and
        // fallback_previous, Updates::previous, and a queued partial tile.
        let last_image = base.clone();
        let fallback_previous = base.clone();
        let previous = base.clone();
        let partial = crate::bitmap_damage(None, &base)[0]
            .sub(
                64,
                64,
                std::num::NonZeroU16::new(64).unwrap(),
                std::num::NonZeroU16::new(64).unwrap(),
            )
            .unwrap();
        drop(base);
        let mut latest: Option<BitmapUpdate> = None;
        for frame in 0..(SHARED_SLOTS as u8 * 4) {
            // The worker still holds `latest` while the next copy is written.
            let (target, image, _) = capture(&mut pool, 10 + frame);
            assert_ne!(target, first, "a held slot was handed to the compositor");
            assert!(filled(&image, 10 + frame));
            // Every retained base still holds frame one, and diffs against it
            // still see the complete change.
            for held in [&last_image, &fallback_previous, &previous] {
                assert!(filled(held, 1));
                assert!(!crate::same_pixels(held, &image, None));
                let damage = crate::bitmap_damage(Some(held), &image);
                assert_eq!(damage.len(), 1);
                assert_eq!(damage[0].width.get(), WIDTH as u16);
            }
            assert!(filled(&partial, 1));
            if let Some(latest) = &latest {
                assert!(filled(latest, 9 + frame));
            }
            latest = Some(image);
        }
        assert!(latest.is_some());
        drop((last_image, fallback_previous, previous, partial));
        drop(latest);
        // With every holder gone the first slot is reused, not a new one.
        let (target, image, _) = capture(&mut pool, 99);
        assert_eq!(target, first);
        assert!(filled(&image, 99));
        assert_eq!(pool.slots(), SHARED_SLOTS);
    }

    #[test]
    fn exhausted_pool_copies_through_scratch_without_touching_held_slots() {
        let mut pool = pool();
        let held: Vec<_> = (0..SHARED_SLOTS as u8)
            .map(|value| capture(&mut pool, value).1)
            .collect();
        let (target, copied, owner) = capture(&mut pool, 77);
        assert_eq!(target, ShmTarget::Scratch);
        assert!(owner.is_none(), "a scratch copy cannot vouch for history");
        assert!(filled(&copied, 77));
        // The scratch buffer is rewritten at once; its snapshot is a copy.
        let (target, next, _) = capture(&mut pool, 78);
        assert_eq!(target, ShmTarget::Scratch);
        assert!(filled(&copied, 77));
        assert!(filled(&next, 78));
        for (value, image) in held.iter().enumerate() {
            assert!(filled(image, value as u8));
        }
        assert_eq!(pool.slots(), SHARED_SLOTS + 1);
        drop(held);
        assert!(matches!(capture(&mut pool, 5).0, ShmTarget::Shared(_)));
    }

    #[test]
    fn slot_reuse_retires_capture_log_history() {
        let mut pool = pool();
        let mut log = crate::CaptureLog::default();
        let now = Instant::now();
        let (first, image, owner) = capture(&mut pool, 1);
        log.push(&image, now, None, owner);
        let (_, next, owner) = capture(&mut pool, 2);
        log.push(&next, now, Some(TileSet::for_image(&next)), owner);
        assert!(log.changes_between(&image, &next).is_some());
        let address = image.data.as_ptr();
        drop(image);
        // The released slot is rewritten: a snapshot at the same address is a
        // different desktop, so the old record must not describe it.
        let (target, reused, owner) = capture(&mut pool, 3);
        assert_eq!(target, first);
        assert_eq!(reused.data.as_ptr(), address);
        assert!(log.changes_between(&reused, &next).is_none());
        log.push(&reused, now, None, owner);
        assert!(log.changes_between(&next, &reused).is_none());
        // A converted capture is an owned copy and releases its slot.
        let target = pool.acquire(detached).unwrap();
        compositor_writes(&pool, target, 4);
        let (converted, owner) = pool
            .snapshot(target, (WIDTH, HEIGHT, STRIDE, true), DESKTOP, false)
            .unwrap();
        assert!(owner.is_none());
        assert!(!pool.buffer(target).busy());
        assert_eq!(converted.data[3], 255);
    }
}
