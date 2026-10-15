//! One worker owns the Wayland queue and its bounded SHM buffers.
//!
//! This module is the commands/desktop façade used by the RDP server. The
//! worker itself is split by responsibility: `state` (Wayland state and event
//! dispatch), `output` (output resize and the event pump), `input` (keymaps,
//! Unicode and virtual pointer/keyboard), `capture` (SHM and GPU buffers) and
//! `run` (the capture loop).
mod capture;
mod input;
mod output;
mod run;
mod state;

use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult,
};
use run::{WorkerChannels, run};
use std::{
    collections::VecDeque,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::protocol::wl_output;

#[derive(Clone, Default)]
struct CursorSnapshot {
    generation: u64,
    shape: Option<Arc<DisplayUpdate>>,
    position: (u16, u16),
    position_generation: u64,
    separate: bool,
    visible: bool,
}
/// Immutable capture ticket retains the GBM slot through encoder cancellation.
pub struct GpuFrame {
    pub fd: OwnedFd,
    pub stride: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub offset: u32,
    pub device: u64,
    pub sequence: u64,
    pub inverted: bool,
    pub captured_at: Instant,
    _owner: Arc<crate::gpu_capture::Buffer>,
}
#[derive(Default)]
struct GpuCapture {
    requested: AtomicBool,
    failed: AtomicBool,
    latest: Mutex<Option<Arc<GpuFrame>>>,
}
#[derive(Default)]
struct OutputStatus {
    suppressed: AtomicBool,
    refresh_epoch: AtomicU64,
    // Client pointerCacheSize from the latest activation; 0 disables caching.
    pointer_cache_size: AtomicU16,
}
type LatestFrames = tokio::sync::watch::Sender<Option<Arc<DisplayUpdate>>>;
type CaptureMetadata = Arc<Mutex<Option<crate::CaptureLog>>>;
type OutputRecord = (u32, wl_output::WlOutput, String, i32, bool, (u32, u32));
enum Command {
    Resize(DesktopSize, f64),
    Key(KeyboardEvent),
    Mouse(MouseEvent),
    Release,
    Stop,
    Clipboard(crate::clipboard::ClipboardAttachment),
    Cursor(u16),
    Status,
}
/// Bounded ordered input mailbox. Adjacent absolute moves and resizes replace
/// only their predecessor; keys, buttons, wheel and relative moves are barriers.
/// The eventfd wakes Wayland prepare-read waits without periodic input polling.
struct Commands {
    queue: Mutex<VecDeque<Command>>,
    wake: OwnedFd,
    drag_buttons: AtomicU32,
}
impl Commands {
    fn new() -> anyhow::Result<Arc<Self>> {
        // SAFETY: eventfd has no borrowed arguments; the returned fd is owned.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        anyhow::ensure!(
            fd >= 0,
            "input wake fd: {}",
            std::io::Error::last_os_error()
        );
        Ok(Arc::new(Self {
            queue: Mutex::new(VecDeque::with_capacity(256)),
            // SAFETY: fd is a fresh valid descriptor checked above.
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            drag_buttons: AtomicU32::new(0),
        }))
    }
    fn signal(&self) {
        let value = 1u64;
        // SAFETY: value is eight readable bytes and eventfd is valid. EAGAIN
        // means a wake is already pending; the queue is the source of truth.
        unsafe { libc::write(self.wake.as_raw_fd(), (&value as *const u64).cast(), 8) };
    }
    fn clear_wake(&self) {
        let mut value = 0u64;
        // SAFETY: value is eight writable bytes; a nonblocking read is bounded.
        unsafe { libc::read(self.wake.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
    }
    fn try_send(&self, command: Command) -> Result<(), ()> {
        let mut queue = self.queue.lock().unwrap();
        let adjacent_move = matches!(
            (queue.back(), &command),
            (
                Some(Command::Mouse(MouseEvent::Move { .. })),
                Command::Mouse(MouseEvent::Move { .. })
            )
        );
        // Keep ordinary drag samples. Under overload preserve the last sample
        // in each adjacent run; no compaction crosses a key/button/resize barrier.
        let replace = (adjacent_move
            && (self.drag_buttons.load(Ordering::Acquire) == 0 || queue.len() >= 224))
            || matches!(
                (queue.back(), &command),
                (Some(Command::Resize(..)), Command::Resize(..))
                    | (Some(Command::Status), Command::Status)
            );
        let buttons = match &command {
            Command::Mouse(
                MouseEvent::Button {
                    button, pressed, ..
                }
                | MouseEvent::ButtonRel {
                    button, pressed, ..
                },
            ) => {
                let mask = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 4,
                    MouseButton::X1 => 8,
                    MouseButton::X2 => 16,
                    _ => 0,
                };
                let old = self.drag_buttons.load(Ordering::Acquire);
                Some(if *pressed { old | mask } else { old & !mask })
            }
            Command::Release | Command::Stop => Some(0),
            _ => None,
        };
        if replace {
            *queue.back_mut().unwrap() = command;
        } else if matches!(command, Command::Stop) {
            queue.clear();
            queue.push_back(command);
        } else {
            if queue.len() == 256 {
                // Reclaim one redundant absolute sample before rejecting critical
                // traffic. Relative motion and wheel deltas are never discarded.
                let redundant = (1..queue.len()).find(|&index| {
                    matches!(
                        (&queue[index - 1], &queue[index]),
                        (
                            Command::Mouse(MouseEvent::Move { .. }),
                            Command::Mouse(MouseEvent::Move { .. })
                        )
                    )
                });
                if let Some(index) = redundant {
                    queue.remove(index - 1);
                } else {
                    return Err(());
                }
            }
            queue.push_back(command);
        }
        if let Some(buttons) = buttons {
            self.drag_buttons.store(buttons, Ordering::Release);
        }
        drop(queue);
        self.signal();
        Ok(())
    }
    fn pop(&self) -> Option<Command> {
        self.queue.lock().unwrap().pop_front()
    }
}

#[derive(Clone)]
pub struct Desktop {
    inner: Arc<Inner>,
}
struct Inner {
    commands: Arc<Commands>,
    frames: Mutex<tokio::sync::watch::Receiver<Option<Arc<DisplayUpdate>>>>,
    cursor_rx: Mutex<Option<tokio::sync::watch::Receiver<CursorSnapshot>>>,
    capture_metadata: CaptureMetadata,
    status: Arc<OutputStatus>,
    frame_rate: Arc<AtomicU32>,
    scale_policy: ScalePolicy,
    gpu: Arc<GpuCapture>,
    // Last announced RDP size, not the worker's possibly newer captured size.
    size: Arc<Mutex<DesktopSize>>,
    failed: Arc<Mutex<Option<String>>>,
    alive: Arc<AtomicBool>,
    // Retain ownership while handlers live; the worker closes it after teardown.
    _lease: Arc<Mutex<Option<UnixStream>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.commands.try_send(Command::Stop);
    }
}
impl Desktop {
    pub fn start(
        socket: UnixStream,
        output_name: String,
        target: DesktopSize,
        lease: Option<UnixStream>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=crate::MAX_DIMENSION as u16).contains(&target.width)
                && (1..=crate::MAX_DIMENSION as u16).contains(&target.height),
            "RDP size outside 1..4096"
        );
        let status = Arc::new(OutputStatus::default());
        let worker_status = status.clone();
        let capture_metadata = Arc::new(Mutex::new(None));
        let worker_capture_metadata = capture_metadata.clone();
        let gpu = Arc::new(GpuCapture::default());
        let worker_gpu = gpu.clone();
        let tx = Commands::new()?;
        let rx = tx.clone();
        let scale_policy = ScalePolicy::configured()?;
        let frame_rate = Arc::new(AtomicU32::new(crate::performance::frame_rate()?));
        let worker_frame_rate = frame_rate.clone();
        let (frames, frame_rx) = tokio::sync::watch::channel(None);
        let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(CursorSnapshot::default());
        let (ready, readiness) = mpsc::sync_channel(1);
        let failed = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(true));
        let lease = Arc::new(Mutex::new(lease));
        let worker_lease = lease.clone();
        let worker_failed = failed.clone();
        let worker_alive = alive.clone();
        let size = Arc::new(Mutex::new(target));
        std::thread::spawn(move || {
            if let Err(err) = run(
                socket,
                &output_name,
                target,
                scale_policy.initial(),
                WorkerChannels {
                    commands: rx,
                    frames,
                    ready: ready.clone(),
                    alive: worker_alive.clone(),
                    frame_rate: worker_frame_rate,
                    gpu: worker_gpu,
                    cursor_tx,
                    capture_metadata: worker_capture_metadata,
                    status: worker_status,
                },
            ) {
                let message = err.to_string();
                eprintln!("Niri capture worker ended: {message}");
                let _ = ready.try_send(Err(anyhow::anyhow!(message.clone())));
                *worker_failed.lock().unwrap() = Some(message);
            }
            worker_alive.store(false, Ordering::Release);
            worker_lease.lock().unwrap().take();
        });
        readiness
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("Wayland startup timed out"))??;
        Ok(Self {
            inner: Arc::new(Inner {
                commands: tx,
                frame_rate,
                scale_policy,
                gpu,
                cursor_rx: Mutex::new(Some(cursor_rx)),
                capture_metadata,
                status,
                frames: Mutex::new(frame_rx),
                size,
                failed,
                alive,
                _lease: lease,
            }),
        })
    }
    fn send(&self, command: Command) {
        // Never discard a release when the bounded input queue is full: closing
        // the connection releases every held key/button in the worker.
        if self.inner.commands.try_send(command).is_err() {
            self.inner.alive.store(false, Ordering::Release);
        }
    }
    pub fn attach_clipboard(&self, clipboard: crate::clipboard::Clipboard) {
        self.send(Command::Clipboard(clipboard.attachment()));
    }
    pub fn failure(&self) -> Option<String> {
        self.inner.failed.lock().unwrap().clone()
    }
    pub fn snapshot(&self) -> Option<Arc<DisplayUpdate>> {
        self.inner.frames.lock().unwrap().borrow().clone()
    }
    pub fn stop(&self) {
        self.release_all();
        self.inner.alive.store(false, Ordering::Release);
        // The worker owns lease teardown after releases have reached Niri.
        // Closing it here lets the broker kill the compositor before that sync.
        let _ = self.inner.commands.try_send(Command::Stop);
    }
    pub fn release_all(&self) {
        self.send(Command::Release);
    }
    pub fn alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }
    /// The concrete capture stream is Send, including when wrapped by an encoder.
    pub fn capture_time_for(&self, image: &ironrdp_server::BitmapUpdate) -> Option<Instant> {
        self.inner
            .capture_metadata
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|log| log.captured_at(image))
    }
    pub fn set_cursor_max_size(&self, max_size: u16) {
        self.send(Command::Cursor(max_size));
    }
    pub fn set_gpu_capture(&self, enabled: bool) {
        let enabled = enabled && !self.inner.gpu.failed.load(Ordering::Acquire);
        if self.inner.gpu.requested.swap(enabled, Ordering::AcqRel) != enabled {
            self.inner.commands.signal();
        }
    }
    pub fn disable_gpu_capture(&self) {
        self.inner.gpu.failed.store(true, Ordering::Release);
        self.set_gpu_capture(false);
    }
    /// An internal full-desktop GPU ticket uses an impossible pixel origin,
    /// so real 16-byte SHM images can never collide with its magic bytes.
    pub fn is_gpu_bitmap(&self, image: &ironrdp_server::BitmapUpdate) -> bool {
        image.x == u16::MAX
            && image.y == u16::MAX
            && image.data.len() == 16
            && &image.data[..8] == b"NIRIGPU1"
    }
    pub fn gpu_frame_for(&self, image: &ironrdp_server::BitmapUpdate) -> Option<Arc<GpuFrame>> {
        if !self.is_gpu_bitmap(image) {
            return None;
        }
        let sequence = u64::from_le_bytes(image.data[8..16].try_into().ok()?);
        self.inner
            .gpu
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .filter(|frame| {
                frame.sequence == sequence
                    && frame.width == u32::from(image.width.get())
                    && frame.height == u32::from(image.height.get())
            })
            .cloned()
    }
    pub fn set_video_frame_rate(&self, fps: u32) {
        self.inner
            .frame_rate
            .store(fps.clamp(1, 60), Ordering::Release);
        self.inner.commands.signal();
    }
    /// Full desktop snapshots and cheap idle heartbeats for video recovery.
    pub fn updates_full_send(&self) -> Box<dyn RdpServerDisplayUpdates + Send> {
        self.updates_mode(true)
    }
    pub fn updates_send(&self) -> Box<dyn RdpServerDisplayUpdates + Send> {
        self.updates_mode(false)
    }
    fn updates_mode(&self, full: bool) -> Box<dyn RdpServerDisplayUpdates + Send> {
        Box::new(Updates {
            desktop: self.clone(),
            rx: self.inner.frames.lock().unwrap().clone(),
            size: *self.inner.size.lock().unwrap(),
            pending: None,
            full,
            previous: None,
            status_epoch: self.inner.status.refresh_epoch.load(Ordering::Acquire),
            cursor_rx: self.inner.cursor_rx.lock().unwrap().clone(),
            cursor_previous: CursorSnapshot::default(),
            partial: VecDeque::new(),
            // One cache per activation: the server requests a new stream for
            // every (re)activation, so the client's cache starts empty too.
            pointer_cache: state::cursor_cache::PointerCache::new(
                self.inner.status.pointer_cache_size.load(Ordering::Acquire),
            ),
        })
    }
}
impl RdpServerInputHandler for Desktop {
    fn keyboard(&mut self, e: KeyboardEvent) {
        self.send(Command::Key(e));
    }
    fn mouse(&mut self, e: MouseEvent) {
        self.send(Command::Mouse(e));
    }
}
struct Updates {
    desktop: Desktop,
    rx: tokio::sync::watch::Receiver<Option<Arc<DisplayUpdate>>>,
    size: DesktopSize,
    pending: Option<DisplayUpdate>,
    full: bool,
    cursor_rx: Option<tokio::sync::watch::Receiver<CursorSnapshot>>,
    cursor_previous: CursorSnapshot,
    previous: Option<ironrdp_server::BitmapUpdate>,
    status_epoch: u64,
    partial: VecDeque<DisplayUpdate>,
    pointer_cache: state::cursor_cache::PointerCache,
}
impl Updates {
    fn refresh_diff_base(&mut self) {
        let epoch = self
            .desktop
            .inner
            .status
            .refresh_epoch
            .load(Ordering::Acquire);
        if epoch != self.status_epoch {
            self.previous = None;
            self.partial
                .retain(|update| !matches!(update, DisplayUpdate::Bitmap(_)));
            self.status_epoch = epoch;
        }
        if !self.full && self.desktop.inner.status.suppressed.load(Ordering::Acquire) {
            self.partial
                .retain(|update| !matches!(update, DisplayUpdate::Bitmap(_)));
        }
    }
    fn cursor_updates(&mut self) {
        let Some(rx) = &mut self.cursor_rx else {
            return;
        };
        let next = rx.borrow_and_update().clone();
        if next.generation == self.cursor_previous.generation {
            return;
        }
        if !next.separate || !next.visible {
            if self.cursor_previous.generation == 0
                || next.separate != self.cursor_previous.separate
                || next.visible != self.cursor_previous.visible
            {
                self.partial.push_back(DisplayUpdate::HidePointer);
            }
        } else {
            let shape_changed = match (&next.shape, &self.cursor_previous.shape) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (Some(_), None) => true,
                _ => false,
            };
            if (shape_changed || !self.cursor_previous.visible || !self.cursor_previous.separate)
                && let Some(shape) = &next.shape
            {
                // Resolve on the stream that delivers it: snapshots may be
                // skipped, so only this stream knows what the client holds.
                let update = self.pointer_cache.resolve(shape);
                self.partial.push_back(update);
            }
        }
        // Position PDUs warp the client's local pointer. Only forward a fresh
        // compositor warp, never a retained position on shape/fallback changes.
        // A repeated warp to the same coordinate still needs delivery after
        // intervening client-local motion, hence the independent generation.
        if next.separate && next.position_generation != self.cursor_previous.position_generation {
            self.partial.push_back(DisplayUpdate::PointerPosition(
                ironrdp_pdu::pointer::PointerPositionAttribute {
                    x: next.position.0,
                    y: next.position.1,
                },
            ));
        }
        self.cursor_previous = next;
    }

    fn prepare(&mut self, update: DisplayUpdate) -> DisplayUpdate {
        if let DisplayUpdate::Bitmap(image) = &update {
            let size = DesktopSize {
                width: image.width.get(),
                height: image.height.get(),
            };
            if size != self.size {
                self.size = size;
                *self.desktop.inner.size.lock().unwrap() = size;
                self.pending = Some(update);
                return DisplayUpdate::Resize(size);
            }
        }
        update
    }
}
#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        // IronRDP treats Err from next_update as recoverable and retries. An
        // immediate repeated Err can also starve its current-thread event loop.
        // None is the library's terminal desktop-stream contract and causes it
        // to close this RDP connection rather than retry a dead compositor.
        if !self.desktop.alive() {
            return Ok(None);
        }
        if let Some(update) = self.pending.take() {
            return Ok(Some(update));
        }
        self.refresh_diff_base();
        self.cursor_updates();
        if let Some(update) = self.partial.pop_front() {
            return Ok(Some(update));
        }
        loop {
            let changed = if let Some(cursor_rx) = &mut self.cursor_rx {
                tokio::select! {
                    result = self.rx.changed() => result.is_ok(),
                    result = cursor_rx.changed() => {
                        if result.is_err() { self.cursor_rx = None; }
                        self.cursor_updates();
                        if let Some(update) = self.partial.pop_front() { return Ok(Some(update)); }
                        continue;
                    }
                }
            } else {
                self.rx.changed().await.is_ok()
            };
            if !changed || !self.desktop.alive() {
                return Ok(None);
            }
            self.refresh_diff_base();
            let update = self
                .rx
                .borrow_and_update()
                .as_ref()
                .map(|f| f.as_ref().clone());
            let Some(update) = update else { continue };
            if let DisplayUpdate::Bitmap(image) = &update
                && !self.full
            {
                if self.desktop.inner.status.suppressed.load(Ordering::Acquire)
                    && self.previous.is_some()
                {
                    continue;
                }
                if self.desktop.is_gpu_bitmap(image) {
                    self.desktop.set_gpu_capture(false);
                    continue;
                }
                let regions = self.desktop.capture_damage(self.previous.as_ref(), image);
                self.previous = Some(image.clone());
                self.partial
                    .extend(regions.into_iter().map(DisplayUpdate::Bitmap));
                if let Some(partial) = self.partial.pop_front() {
                    // Size is always derived from the complete retained frame.
                    if image.width.get() != self.size.width
                        || image.height.get() != self.size.height
                    {
                        self.partial.clear();
                        return Ok(Some(self.prepare(update)));
                    }
                    return Ok(Some(partial));
                }
                continue;
            }
            return Ok(Some(self.prepare(update)));
        }
    }
}
#[async_trait::async_trait]
impl RdpServerDisplay for Desktop {
    async fn size(&mut self) -> DesktopSize {
        *self.inner.size.lock().unwrap()
    }
    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(self.updates_send())
    }
    fn request_refresh(&mut self) {
        self.inner
            .status
            .refresh_epoch
            .fetch_add(1, Ordering::AcqRel);
        self.send(Command::Status);
    }
    fn set_output_suppressed(&mut self, suppressed: bool) {
        let previous = self
            .inner
            .status
            .suppressed
            .swap(suppressed, Ordering::AcqRel);
        if previous != suppressed {
            if !suppressed {
                self.inner
                    .status
                    .refresh_epoch
                    .fetch_add(1, Ordering::AcqRel);
            }
            self.send(Command::Status);
        }
    }
    fn pointer_capabilities(&mut self, max_size: u16) {
        // No reported client cache size: never reference cache slots.
        self.pointer_cache_capabilities(max_size, 0);
    }
    fn pointer_cache_capabilities(&mut self, max_size: u16, cache_size: u16) {
        self.inner
            .status
            .pointer_cache_size
            .store(cache_size, Ordering::Release);
        self.set_cursor_max_size(max_size);
    }
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(size) = layout_size(&layout) {
            let scale = self.inner.scale_policy.for_layout(&layout);
            self.send(Command::Resize(size, scale));
        }
    }
}

fn layout_size(layout: &DisplayControlMonitorLayout) -> Option<DesktopSize> {
    let [monitor] = layout.monitors() else {
        return None;
    };
    let (width, height) = monitor.dimensions();
    if !monitor.is_primary()
        || monitor.position() != Some((0, 0))
        || !(200..=crate::MAX_DIMENSION).contains(&width)
        || !(200..=crate::MAX_DIMENSION).contains(&height)
        || width % 2 != 0
    {
        return None;
    }
    Some(DesktopSize {
        width: width as u16,
        height: height as u16,
    })
}

/// Pixels remain the RDP/capture contract; scale changes compositor/app logical
/// coordinates only. Virtual-pointer absolute extents normalize into the output.
#[derive(Clone, Copy)]
enum ScalePolicy {
    Client,
    Fixed(f64),
}
impl ScalePolicy {
    fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value {
            None | Some("client") => Ok(Self::Client),
            Some(value) => {
                let scale: f64 = value.parse()?;
                anyhow::ensure!(
                    scale.is_finite() && (1.0..=5.0).contains(&scale),
                    "NIRI_RDP_SCALE must be client or a scale in 1..5"
                );
                Ok(Self::Fixed(scale))
            }
        }
    }
    fn configured() -> anyhow::Result<Self> {
        Self::parse(std::env::var("NIRI_RDP_SCALE").ok().as_deref())
    }
    fn initial(self) -> f64 {
        match self {
            Self::Client => 1.0,
            Self::Fixed(scale) => scale,
        }
    }
    fn for_layout(self, layout: &DisplayControlMonitorLayout) -> f64 {
        match self {
            Self::Fixed(scale) => scale,
            Self::Client => layout
                .monitors()
                .first()
                .and_then(|m| m.desktop_scale_factor())
                .map_or(1.0, |percent| f64::from(percent) / 100.0),
        }
    }
}

#[cfg(test)]
pub(crate) mod lifecycle_tests {
    use super::*;
    use crate::{bitmap, capture_bitmap};
    use std::io::Read;
    #[test]
    fn bounded_mailbox_coalesces_motion_and_resize_but_preserves_barriers() {
        let queue = Commands::new().unwrap();
        for x in 0..10_000u32 {
            queue
                .try_send(Command::Mouse(MouseEvent::Move { x: x as u16, y: 20 }))
                .unwrap();
        }
        assert_eq!(queue.queue.lock().unwrap().len(), 1);
        queue
            .try_send(Command::Key(KeyboardEvent::Pressed {
                code: 30,
                extended: false,
            }))
            .unwrap();
        queue
            .try_send(Command::Mouse(MouseEvent::RelMove { x: 1, y: 2 }))
            .unwrap();
        queue
            .try_send(Command::Mouse(MouseEvent::VerticalScroll { value: 120 }))
            .unwrap();
        for width in 640..1000 {
            queue
                .try_send(Command::Resize(DesktopSize { width, height: 480 }, 1.5))
                .unwrap();
        }
        queue
            .try_send(Command::Mouse(MouseEvent::Button {
                x: 400,
                y: 200,
                button: MouseButton::Left,
                pressed: false,
            }))
            .unwrap();
        assert_eq!(queue.queue.lock().unwrap().len(), 6);
        assert!(matches!(
            queue.pop(),
            Some(Command::Mouse(MouseEvent::Move { x: 9999, .. }))
        ));
        assert!(matches!(queue.pop(), Some(Command::Key(_))));
        assert!(matches!(
            queue.pop(),
            Some(Command::Mouse(MouseEvent::RelMove { x: 1, y: 2 }))
        ));
        assert!(matches!(
            queue.pop(),
            Some(Command::Mouse(MouseEvent::VerticalScroll { value: 120 }))
        ));
        assert!(matches!(
            queue.pop(),
            Some(Command::Resize(DesktopSize { width: 999, .. }, 1.5))
        ));
        assert!(matches!(
            queue.pop(),
            Some(Command::Mouse(MouseEvent::Button { pressed: false, .. }))
        ));
    }
    #[test]
    fn drag_samples_remain_ordered_and_overload_preserves_final_point_and_release() {
        let queue = Commands::new().unwrap();
        queue
            .try_send(Command::Mouse(MouseEvent::Button {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                pressed: true,
            }))
            .unwrap();
        for x in 0..10_000u16 {
            queue
                .try_send(Command::Mouse(MouseEvent::Move { x, y: 20 }))
                .unwrap();
        }
        queue
            .try_send(Command::Key(KeyboardEvent::Released {
                code: 29,
                extended: false,
            }))
            .unwrap();
        queue
            .try_send(Command::Mouse(MouseEvent::VerticalScroll { value: 120 }))
            .unwrap();
        queue
            .try_send(Command::Mouse(MouseEvent::Button {
                x: 9999,
                y: 20,
                button: MouseButton::Left,
                pressed: false,
            }))
            .unwrap();
        let mut queue = queue.queue.lock().unwrap();
        assert!(queue.len() < 256);
        assert!(matches!(
            queue.pop_front(),
            Some(Command::Mouse(MouseEvent::Button { pressed: true, .. }))
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(Command::Mouse(MouseEvent::Move { x: 0, .. }))
        ));
        let mut last = 0;
        while let Some(Command::Mouse(MouseEvent::Move { x, .. })) = queue.front() {
            last = *x;
            queue.pop_front();
        }
        assert_eq!(last, 9999);
        assert!(matches!(
            queue.pop_front(),
            Some(Command::Key(KeyboardEvent::Released { .. }))
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(Command::Mouse(MouseEvent::VerticalScroll { value: 120 }))
        ));
        assert!(matches!(
            queue.pop_front(),
            Some(Command::Mouse(MouseEvent::Button {
                x: 9999,
                pressed: false,
                ..
            }))
        ));
        assert!(queue.is_empty());
    }
    #[test]
    fn critical_overflow_is_bounded_and_stop_is_always_deliverable() {
        let queue = Commands::new().unwrap();
        for _ in 0..256 {
            queue
                .try_send(Command::Key(KeyboardEvent::Pressed {
                    code: 30,
                    extended: false,
                }))
                .unwrap();
        }
        assert!(queue.try_send(Command::Release).is_err());
        assert_eq!(queue.queue.lock().unwrap().len(), 256);
        queue.try_send(Command::Stop).unwrap();
        assert!(matches!(queue.pop(), Some(Command::Stop)));
        assert!(queue.pop().is_none());
    }
    #[test]
    fn explicit_scale_overrides_client_metadata_and_invalid_values_fail_closed() {
        let layout =
            DisplayControlMonitorLayout::new_single_primary_monitor(1920, 1080, Some(150), None)
                .unwrap();
        assert_eq!(ScalePolicy::Client.for_layout(&layout), 1.5);
        assert_eq!(ScalePolicy::Fixed(2.0).for_layout(&layout), 2.0);
        for value in ["0", "0.99", "5.1", "nan", "inf", ""] {
            assert!(ScalePolicy::parse(Some(value)).is_err(), "{value}");
        }
        for value in ["1", "1.5", "2", "5", "client"] {
            assert!(ScalePolicy::parse(Some(value)).is_ok());
        }
    }
    pub(crate) fn fake_desktop(size: DesktopSize) -> (Desktop, LatestFrames) {
        let (frames, rx) = tokio::sync::watch::channel(None);
        (
            Desktop {
                inner: Arc::new(Inner {
                    commands: Commands::new().unwrap(),
                    frames: Mutex::new(rx),
                    cursor_rx: Mutex::new(None),
                    capture_metadata: Arc::new(Mutex::new(None)),
                    status: Arc::new(OutputStatus::default()),
                    frame_rate: Arc::new(AtomicU32::new(30)),
                    scale_policy: ScalePolicy::Client,
                    gpu: Arc::new(GpuCapture::default()),
                    size: Arc::new(Mutex::new(size)),
                    failed: Arc::new(Mutex::new(None)),
                    alive: Arc::new(AtomicBool::new(true)),
                    _lease: Arc::new(Mutex::new(None)),
                }),
            },
            frames,
        )
    }
    /// Whether a fresh update stream of `desktop` answers a repeated pointer
    /// shape with `CachedPointer`, i.e. a usable negotiated pointer cache size
    /// reached the desktop through whatever display wrappers sit above it.
    pub(crate) async fn repeated_pointer_is_cached(desktop: &Desktop) -> bool {
        let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(CursorSnapshot::default());
        *desktop.inner.cursor_rx.lock().unwrap() = Some(cursor_rx);
        let mut updates = desktop.updates_send();
        let mut last = None;
        for (generation, seed) in [(1, 1), (2, 2), (3, 1)] {
            cursor_tx.send_replace(CursorSnapshot {
                generation,
                shape: Some(Arc::new(DisplayUpdate::RGBAPointer(
                    ironrdp_server::RGBAPointer {
                        cache_index: 0,
                        width: 1,
                        height: 1,
                        hot_x: 0,
                        hot_y: 0,
                        data: vec![seed, 0, 0, 255],
                    },
                ))),
                visible: true,
                separate: true,
                ..CursorSnapshot::default()
            });
            last = tokio::time::timeout(Duration::from_secs(1), updates.next_update())
                .await
                .unwrap()
                .unwrap();
        }
        matches!(last, Some(DisplayUpdate::CachedPointer(0)))
    }
    #[tokio::test(flavor = "current_thread")]
    async fn explicit_refresh_and_resume_replay_full_pixels_after_diff_suppression() {
        let size = DesktopSize {
            width: 256,
            height: 256,
        };
        let (mut desktop, frames) = fake_desktop(size);
        let mut updates = desktop.updates_send();
        let mut pixels = vec![0u8; 256 * 256 * 4];
        let initial = capture_bitmap(&pixels, 256, 256, 1024, false, size, false).unwrap();
        let publish = |image| frames.send_replace(Some(Arc::new(DisplayUpdate::Bitmap(image))));
        publish(initial.clone());
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        publish(initial.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), updates.next_update())
                .await
                .is_err()
        );
        desktop.request_refresh();
        publish(initial.clone()); // Model the worker's cached replay, without readback.
        assert!(
            matches!(updates.next_update().await.unwrap(), Some(DisplayUpdate::Bitmap(image))
            if image.x == 0 && image.y == 0 && image.width.get() == 256 && image.height.get() == 256)
        );
        desktop.set_output_suppressed(true);
        pixels[4] = 20;
        let changed = capture_bitmap(&pixels, 256, 256, 1024, false, size, false).unwrap();
        publish(changed.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), updates.next_update())
                .await
                .is_err()
        );
        desktop.set_output_suppressed(false);
        publish(changed);
        assert!(
            matches!(updates.next_update().await.unwrap(), Some(DisplayUpdate::Bitmap(image))
            if image.x == 0 && image.y == 0 && image.width.get() == 256 && image.data[4] == 20)
        );
    }
    #[test]
    fn tiny_shm_pixels_cannot_be_mistaken_for_an_internal_gpu_ticket() {
        let size = DesktopSize {
            width: 2,
            height: 2,
        };
        let (desktop, _frames) = fake_desktop(size);
        let mut bytes = b"NIRIGPU1".to_vec();
        bytes.extend_from_slice(&1u64.to_le_bytes());
        let mut image = capture_bitmap(&bytes, 2, 2, 8, false, size, false).unwrap();
        assert!(!desktop.is_gpu_bitmap(&image));
        image.x = u16::MAX;
        image.y = u16::MAX;
        assert!(desktop.is_gpu_bitmap(&image));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn pointer_cache_follows_negotiated_size_and_each_activation_starts_empty() {
        let (_frames, rx) = tokio::sync::watch::channel(None);
        let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(CursorSnapshot::default());
        let mut desktop = Desktop {
            inner: Arc::new(Inner {
                commands: Commands::new().unwrap(),
                frames: Mutex::new(rx),
                frame_rate: Arc::new(AtomicU32::new(30)),
                scale_policy: ScalePolicy::Client,
                gpu: Arc::new(GpuCapture::default()),
                cursor_rx: Mutex::new(Some(cursor_rx)),
                capture_metadata: Arc::new(Mutex::new(None)),
                status: Arc::new(OutputStatus::default()),
                size: Arc::new(Mutex::new(DesktopSize {
                    width: 640,
                    height: 480,
                })),
                failed: Arc::new(Mutex::new(None)),
                alive: Arc::new(AtomicBool::new(true)),
                _lease: Arc::new(Mutex::new(None)),
            }),
        };
        let shape = |seed: u8| {
            Arc::new(DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
                cache_index: 0,
                width: 1,
                height: 1,
                hot_x: 0,
                hot_y: 0,
                data: vec![seed, 0, 0, 255],
            }))
        };
        let show = |generation: u64, seed: u8| {
            cursor_tx.send_replace(CursorSnapshot {
                generation,
                shape: Some(shape(seed)),
                visible: true,
                separate: true,
                ..CursorSnapshot::default()
            });
        };
        async fn next(updates: &mut Box<dyn RdpServerDisplayUpdates + Send>) -> DisplayUpdate {
            tokio::time::timeout(Duration::from_secs(1), updates.next_update())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        }
        let full = |update: DisplayUpdate| match update {
            DisplayUpdate::RGBAPointer(pointer) => pointer.cache_index,
            _ => panic!("expected a full pointer shape"),
        };
        let cached = |update: DisplayUpdate| match update {
            DisplayUpdate::CachedPointer(index) => index,
            _ => panic!("expected a cached pointer reference"),
        };
        desktop.pointer_cache_capabilities(96, 25);
        assert_eq!(
            desktop
                .inner
                .status
                .pointer_cache_size
                .load(Ordering::Acquire),
            25
        );
        let mut first = desktop.updates_send();
        show(1, 1);
        assert_eq!(full(next(&mut first).await), 0);
        show(2, 2);
        assert_eq!(full(next(&mut first).await), 1);
        show(3, 1);
        assert_eq!(cached(next(&mut first).await), 0);
        // Reactivation: the new stream must resend the current shape in full,
        // and a shape skipped by the watch channel is never referenced.
        desktop.pointer_cache_capabilities(96, 25);
        let mut second = desktop.updates_send();
        assert_eq!(full(next(&mut second).await), 0);
        show(4, 2);
        show(5, 1);
        assert_eq!(cached(next(&mut second).await), 0);
        show(6, 2);
        assert_eq!(full(next(&mut second).await), 1);
        // A wrapper that reports no cache size disables cache references.
        desktop.pointer_capabilities(96);
        let mut third = desktop.updates_send();
        assert_eq!(full(next(&mut third).await), 0);
        show(7, 1);
        assert_eq!(full(next(&mut third).await), 0);
        show(8, 2);
        assert_eq!(full(next(&mut third).await), 0);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn skipped_cursor_positions_retain_shape_and_visibility_is_ordered() {
        let (frames, rx) = tokio::sync::watch::channel(None);
        let (cursor_tx, cursor_rx) = tokio::sync::watch::channel(CursorSnapshot::default());
        let size = DesktopSize {
            width: 640,
            height: 480,
        };
        let desktop = Desktop {
            inner: Arc::new(Inner {
                commands: Commands::new().unwrap(),
                frames: Mutex::new(rx.clone()),
                frame_rate: Arc::new(AtomicU32::new(30)),
                scale_policy: ScalePolicy::Client,
                gpu: Arc::new(GpuCapture::default()),
                cursor_rx: Mutex::new(Some(cursor_rx.clone())),
                capture_metadata: Arc::new(Mutex::new(None)),
                status: Arc::new(OutputStatus::default()),
                size: Arc::new(Mutex::new(size)),
                failed: Arc::new(Mutex::new(None)),
                alive: Arc::new(AtomicBool::new(true)),
                _lease: Arc::new(Mutex::new(None)),
            }),
        };
        let mut updates = Updates {
            desktop,
            rx,
            size,
            pending: None,
            full: false,
            cursor_rx: Some(cursor_rx),
            cursor_previous: CursorSnapshot::default(),
            previous: None,
            status_epoch: 0,
            partial: VecDeque::new(),
            pointer_cache: state::cursor_cache::PointerCache::new(0),
        };
        let shape = Arc::new(DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
            cache_index: 0,
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            data: vec![1, 2, 3, 255],
        }));
        cursor_tx.send_replace(CursorSnapshot {
            generation: 1,
            shape: Some(shape.clone()),
            visible: true,
            separate: true,
            ..CursorSnapshot::default()
        });
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::RGBAPointer(_))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), updates.next_update())
                .await
                .is_err(),
            "initial cursor shape must not warp mstsc to the default position"
        );
        cursor_tx.send_replace(CursorSnapshot {
            generation: 2,
            shape: Some(shape.clone()),
            position: (10, 20),
            position_generation: 1,
            visible: true,
            separate: true,
        });
        cursor_tx.send_replace(CursorSnapshot {
            generation: 3,
            shape: Some(shape),
            position: (320, 240),
            position_generation: 2,
            visible: true,
            separate: true,
        });
        assert!(
            matches!(updates.next_update().await.unwrap(), Some(DisplayUpdate::PointerPosition(position))
            if position.x == 320 && position.y == 240)
        );
        // Returning from embedded fallback must restore the shape without
        // forcing mstsc back to an old server-observed pointer position.
        let mut fallback = cursor_tx.borrow().clone();
        fallback.generation = 4;
        fallback.separate = false;
        cursor_tx.send_replace(fallback.clone());
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::HidePointer)
        ));
        fallback.generation = 5;
        fallback.separate = true;
        cursor_tx.send_replace(fallback);
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::RGBAPointer(_))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), updates.next_update())
                .await
                .is_err(),
            "shape restoration must not replay a stale pointer position"
        );
        // Client-local motion may intervene without changing the last emitted
        // position. A subsequent real warp back to it must still be forwarded.
        let mut warp = cursor_tx.borrow().clone();
        warp.generation = 6;
        warp.position_generation += 1;
        cursor_tx.send_replace(warp);
        assert!(
            matches!(updates.next_update().await.unwrap(), Some(DisplayUpdate::PointerPosition(position))
            if position.x == 320 && position.y == 240)
        );
        let mut hidden = cursor_tx.borrow().clone();
        hidden.generation = 7;
        hidden.visible = false;
        cursor_tx.send_replace(hidden);
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::HidePointer)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), updates.next_update())
                .await
                .is_err()
        );
        drop(frames);
    }
    #[test]
    fn layout_accepts_portrait_and_odd_height_but_rejects_unsupported_sizes() {
        for (width, height, accepted) in [
            (1002, 701, true),
            (702, 1001, true),
            (4096, 4096, true),
            (4098, 700, false),
            (1000, 4097, false),
        ] {
            let layout =
                DisplayControlMonitorLayout::new_single_primary_monitor(width, height, None, None)
                    .unwrap();
            assert_eq!(layout_size(&layout).is_some(), accepted);
        }
        let primary =
            ironrdp_displaycontrol::pdu::MonitorLayoutEntry::new_primary(1000, 700).unwrap();
        let secondary =
            ironrdp_displaycontrol::pdu::MonitorLayoutEntry::new_secondary(1000, 700).unwrap();
        assert!(
            layout_size(&DisplayControlMonitorLayout::new(&[primary, secondary]).unwrap())
                .is_none()
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn latest_frame_announces_resize_before_pixels_and_survives_cancellation() {
        let initial = DesktopSize {
            width: 640,
            height: 480,
        };
        let commands = Commands::new().unwrap();
        let (frames, rx) = tokio::sync::watch::channel(None);
        let desktop = Desktop {
            inner: Arc::new(Inner {
                commands,
                frame_rate: Arc::new(AtomicU32::new(30)),
                scale_policy: ScalePolicy::Client,
                gpu: Arc::new(GpuCapture::default()),
                cursor_rx: Mutex::new(None),
                capture_metadata: Arc::new(Mutex::new(None)),
                status: Arc::new(OutputStatus::default()),
                frames: Mutex::new(rx.clone()),
                size: Arc::new(Mutex::new(initial)),
                failed: Arc::new(Mutex::new(None)),
                alive: Arc::new(AtomicBool::new(true)),
                _lease: Arc::new(Mutex::new(None)),
            }),
        };
        let mut updates = Updates {
            desktop,
            rx,
            size: initial,
            pending: None,
            full: true,
            cursor_rx: None,
            cursor_previous: CursorSnapshot::default(),
            previous: None,
            status_epoch: 0,
            partial: VecDeque::new(),
            pointer_cache: state::cursor_cache::PointerCache::new(0),
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(1), updates.next_update())
                .await
                .is_err()
        );
        let publish = |width, height| {
            let target = DesktopSize { width, height };
            let image = bitmap(&[1, 2, 3, 0], 1, 1, 4, false, target, false).unwrap();
            frames.send_replace(Some(Arc::new(DisplayUpdate::Bitmap(image))));
        };
        // A watch receiver can skip several captures; derive control updates
        // from the retained frame rather than relying on a transient Resize.
        publish(800, 600);
        publish(702, 1001);
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Resize(DesktopSize {
                width: 702,
                height: 1001
            }))
        ));
        assert!(
            matches!(updates.next_update().await.unwrap(), Some(DisplayUpdate::Bitmap(image)) if image.width.get() == 702 && image.height.get() == 1001)
        );
        publish(702, 1001);
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        publish(1002, 701);
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Resize(DesktopSize {
                width: 1002,
                height: 701
            }))
        ));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn dead_or_closed_capture_is_terminal_not_a_retry_error() {
        for alive in [false, true] {
            let commands = Commands::new().unwrap();
            let (frames, rx) = tokio::sync::watch::channel(None);
            let desktop = Desktop {
                inner: Arc::new(Inner {
                    commands,
                    frame_rate: Arc::new(AtomicU32::new(30)),
                    scale_policy: ScalePolicy::Client,
                    gpu: Arc::new(GpuCapture::default()),
                    cursor_rx: Mutex::new(None),
                    capture_metadata: Arc::new(Mutex::new(None)),
                    status: Arc::new(OutputStatus::default()),
                    frames: Mutex::new(rx.clone()),
                    size: Arc::new(Mutex::new(DesktopSize {
                        width: 640,
                        height: 480,
                    })),
                    failed: Arc::new(Mutex::new(Some("broken compositor".into()))),
                    alive: Arc::new(AtomicBool::new(alive)),
                    _lease: Arc::new(Mutex::new(None)),
                }),
            };
            drop(frames);
            let mut updates = Updates {
                desktop,
                rx,
                size: DesktopSize {
                    width: 640,
                    height: 480,
                },
                pending: None,
                full: true,
                cursor_rx: None,
                cursor_previous: CursorSnapshot::default(),
                previous: None,
                status_epoch: 0,
                partial: VecDeque::new(),
                pointer_cache: state::cursor_cache::PointerCache::new(0),
            };
            let result = tokio::time::timeout(Duration::from_millis(100), updates.next_update())
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.is_none(),
                "dead compositor must end display stream and RDP connection"
            );
        }
    }
    #[test]
    fn stop_keeps_lease_until_worker_finishes_input_teardown() {
        let (lease, mut broker) = UnixStream::pair().unwrap();
        broker
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let commands = Commands::new().unwrap();
        let (_frames, rx) = tokio::sync::watch::channel(None);
        let desktop = Desktop {
            inner: Arc::new(Inner {
                commands,
                frame_rate: Arc::new(AtomicU32::new(30)),
                scale_policy: ScalePolicy::Client,
                gpu: Arc::new(GpuCapture::default()),
                cursor_rx: Mutex::new(None),
                capture_metadata: Arc::new(Mutex::new(None)),
                status: Arc::new(OutputStatus::default()),
                frames: Mutex::new(rx),
                size: Arc::new(Mutex::new(DesktopSize {
                    width: 640,
                    height: 480,
                })),
                failed: Arc::new(Mutex::new(None)),
                alive: Arc::new(AtomicBool::new(true)),
                _lease: Arc::new(Mutex::new(Some(lease))),
            }),
        };
        let retained = desktop.clone();
        let worker_lease = desktop.inner._lease.clone();
        desktop.stop();
        let mut byte = [0; 1];
        let error = broker.read(&mut byte).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        assert!(!retained.alive());
        assert!(retained.inner._lease.lock().unwrap().is_some());
        // Model the worker completing the compositor sync and returning.
        worker_lease.lock().unwrap().take();
        assert_eq!(broker.read(&mut byte).unwrap(), 0);
        assert!(retained.inner._lease.lock().unwrap().is_none());
    }
    #[test]
    fn broken_compositor_startup_closes_lease_and_never_returns_a_desktop() {
        let (socket, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let (lease, mut broker) = UnixStream::pair().unwrap();
        broker
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(
            Desktop::start(
                socket,
                "HEADLESS-1".into(),
                DesktopSize {
                    width: 640,
                    height: 480
                },
                Some(lease)
            )
            .is_err()
        );
        let mut byte = [0; 1];
        assert_eq!(broker.read(&mut byte).unwrap(), 0);
    }
}
