//! One worker owns the Wayland queue and its bounded SHM buffers.
use crate::cursor_protocol::client::{
    niri_rdp_cursor_manager_v1::NiriRdpCursorManagerV1,
    niri_rdp_cursor_v1::{self, NiriRdpCursorV1},
};
use crate::{buffer_length, capture_bitmap, evdev_key};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult,
};
use std::{
    collections::{BTreeSet, VecDeque},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::{
    output_management::v1::client::{
        zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
        zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
        zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
        zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
        zwlr_output_mode_v1::ZwlrOutputModeV1,
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};
use xkbcommon::xkb;

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
}
type LatestFrames = tokio::sync::watch::Sender<Option<Arc<DisplayUpdate>>>;
type CaptureMetadata = Arc<Mutex<Option<(usize, usize, Instant)>>>;
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
            .filter(|(pointer, length, _)| {
                *pointer == image.data.as_ptr() as usize && *length == image.data.len()
            })
            .map(|(_, _, instant)| *instant)
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
                self.partial.push_back(shape.as_ref().clone());
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
                let regions = crate::bitmap_damage(self.previous.as_ref(), image);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigurationResult {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Default)]
struct State {
    output_manager: Option<ZwlrOutputManagerV1>,
    heads: Vec<(ZwlrOutputHeadV1, String, bool, Option<f64>)>,
    output_serial: Option<u32>,
    applied_scale: Option<f64>,
    applied_size: Option<DesktopSize>,
    configuration_result: Option<ConfigurationResult>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    capture: Option<ZwlrScreencopyManagerV1>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    seat_keyboard: Option<wl_keyboard::WlKeyboard>,
    seat_keymap: Option<String>,
    keymap_error: Option<String>,
    synced: bool,
    outputs: Vec<OutputRecord>,
    selected_output: Option<u32>,
    output_removed: bool,
    refresh_requested: bool,
    cursor_manager: Option<NiriRdpCursorManagerV1>,
    cursor: Option<NiriRdpCursorV1>,
    cursor_max: u16,
    cursor_tx: Option<tokio::sync::watch::Sender<CursorSnapshot>>,
    cursor_snapshot: CursorSnapshot,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    dmabuf_xrgb_modifiers: BTreeSet<u64>,
    dmabuf_params: Option<wayland_client::backend::ObjectId>,
    dmabuf_created: Option<wl_buffer::WlBuffer>,
    dmabuf_failed: bool,
    gpu_format: Option<(u32, u32, u32)>,
    active_frame: Option<wayland_client::backend::ObjectId>,
    dimensions: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
    inverted: bool,
    damage: Vec<(u32, u32, u32, u32)>,
    full_damage: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &(),
        _: &Connection,
        q: &QueueHandle<Self>,
    ) {
        match e {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "niri_rdp_cursor_manager_v1" => {
                    s.cursor_manager = Some(registry.bind(name, 1, q, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    s.dmabuf = Some(registry.bind(name, version.min(3), q, ()));
                }
                "wl_shm" => s.shm = Some(registry.bind(name, 1, q, ())),
                "wl_seat" if s.seat.is_none() => {
                    s.seat = Some(registry.bind(name, version.min(7), q, ()))
                }
                "wl_output" => s.outputs.push((
                    name,
                    registry.bind(name, version.min(4), q, ()),
                    String::new(),
                    1,
                    true,
                    (0, 0),
                )),
                "zwlr_output_manager_v1" => {
                    s.output_manager = Some(registry.bind(name, version.min(4), q, ()))
                }
                "zwlr_screencopy_manager_v1" => {
                    s.capture = Some(registry.bind(name, version.min(3), q, ()))
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    s.pointer_manager = Some(registry.bind(name, version.min(2), q, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    s.keyboard_manager = Some(registry.bind(name, 1, q, ()))
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => s.remove_output(name),
            _ => {}
        }
    }
}
impl State {
    fn remove_output(&mut self, name: u32) {
        self.output_removed |= self.selected_output == Some(name);
        self.outputs.retain(|o| o.0 != name);
    }
    fn validate_output(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.output_removed, "Wayland output removed");
        if let Some(name) = self.selected_output {
            let output = self
                .outputs
                .iter()
                .find(|o| o.0 == name)
                .ok_or_else(|| anyhow::anyhow!("Wayland output removed"))?;
            anyhow::ensure!(
                output.3 > 0 && output.4,
                "bridge requires positive scale and normal transform"
            );
        }
        Ok(())
    }
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        p: &wl_output::WlOutput,
        e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(o) = s.outputs.iter_mut().find(|o| &o.1 == p) {
            match e {
                wl_output::Event::Name { name } => o.2 = name,
                wl_output::Event::Scale { factor } => o.3 = factor,
                wl_output::Event::Mode {
                    flags: WEnum::Value(flags),
                    width,
                    height,
                    ..
                } if flags.contains(wl_output::Mode::Current) => {
                    o.5 = (width as u32, height as u32);
                }
                wl_output::Event::Geometry { transform, .. } => {
                    o.4 = transform == WEnum::Value(wl_output::Transform::Normal)
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        s: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        e: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if s.active_frame.as_ref() != Some(&frame.id()) {
            return;
        }
        match e {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => s.dimensions = Some((format, width, height, stride)),
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: WEnum::Value(flags),
            } => s.inverted = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                s.gpu_format = Some((format, width, height));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => s.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => s.ready = true,
            zwlr_screencopy_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                if s.damage.len() < 32 {
                    s.damage.push((x, y, width, height));
                } else {
                    s.full_damage = true;
                }
            }
            zwlr_screencopy_frame_v1::Event::Failed => s.failed = true,
            _ => {}
        }
    }
}
fn cursor_pixels(
    mut file: File,
    width: u32,
    height: u32,
    hot_x: u32,
    hot_y: u32,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        (1..=384).contains(&width)
            && (1..=384).contains(&height)
            && hot_x < width
            && hot_y < height,
        "invalid cursor geometry"
    );
    let length = (width * height * 4) as usize;
    anyhow::ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == length as u64,
        "invalid cursor image file"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut rgba = Vec::with_capacity(length);
    file.take(length as u64 + 1).read_to_end(&mut rgba)?;
    anyhow::ensure!(rgba.len() == length, "cursor image length mismatch");
    // MS-RDPBCGR Color/New Pointer masks are bottom-up BGRA. The compositor
    // supplies premultiplied top-down RGBA; restore straight colors and flip.
    let stride = (width * 4) as usize;
    let mut bgra = Vec::with_capacity(length);
    for row in rgba.chunks_exact(stride).rev() {
        for pixel in row.as_chunks::<4>().0 {
            let alpha = u32::from(pixel[3]);
            let color = |channel: u8| {
                (u32::from(channel) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255) as u8
            };
            bgra.extend_from_slice(&[color(pixel[2]), color(pixel[1]), color(pixel[0]), pixel[3]]);
        }
    }
    Ok(bgra)
}
impl State {
    fn observed_scale(&self, name: &str) -> Option<f64> {
        self.heads.iter().find(|head| head.1 == name)?.3
    }
    fn output_policy_matches(&self, name: &str, target: DesktopSize, scale: f64) -> bool {
        let Some(actual) = self.observed_scale(name) else {
            return false;
        };
        self.outputs.iter().any(|output| {
            output.2 == name
                && output.5 == (u32::from(target.width), u32::from(target.height))
                && output.3 == actual.ceil() as i32
        }) && actual.is_finite()
            && (actual - scale).abs() <= 1.0 / 256.0
    }
    fn remote_geometry_changed(&self) -> bool {
        let (Some(scale), Some(target), Some(selected)) =
            (self.applied_scale, self.applied_size, self.selected_output)
        else {
            return false;
        };
        self.outputs
            .iter()
            .find(|output| output.0 == selected)
            .is_some_and(|output| !self.output_policy_matches(&output.2, target, scale))
    }
    fn publish_cursor(&mut self) {
        self.cursor_snapshot.generation = self.cursor_snapshot.generation.wrapping_add(1);
        if let Some(tx) = &self.cursor_tx {
            tx.send_replace(self.cursor_snapshot.clone());
        }
    }
    fn set_cursor_max(&mut self, max_size: u16) {
        self.cursor_max = max_size;
        // Reactivation invalidates the client's pointer cache. Start embedded
        // and subscribe again so a fresh supported shape precedes separation.
        self.cursor_snapshot.separate = false;
        self.cursor_snapshot.visible = false;
        self.publish_cursor();
        if let Some(cursor) = self.cursor.take() {
            cursor.destroy();
        }
    }
}
impl Dispatch<NiriRdpCursorV1, ()> for State {
    fn event(
        s: &mut Self,
        cursor: &NiriRdpCursorV1,
        event: niri_rdp_cursor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if s.cursor.as_ref() != Some(cursor) {
            return;
        }
        match event {
            niri_rdp_cursor_v1::Event::Position { x, y } => {
                s.cursor_snapshot.position = (
                    x.min(u32::from(u16::MAX)) as u16,
                    y.min(u32::from(u16::MAX)) as u16,
                );
                s.cursor_snapshot.position_generation =
                    s.cursor_snapshot.position_generation.wrapping_add(1);
            }
            niri_rdp_cursor_v1::Event::Shape {
                fd,
                width,
                height,
                hot_x,
                hot_y,
            } => {
                let pixels = cursor_pixels(File::from(fd), width, height, hot_x, hot_y);
                if width <= u32::from(s.cursor_max)
                    && height <= u32::from(s.cursor_max)
                    && let Ok(pixels) = pixels
                {
                    let shape = if width > 96 || height > 96 {
                        DisplayUpdate::LargePointer(ironrdp_server::LargePointer {
                            cache_index: 0,
                            width: width as u16,
                            height: height as u16,
                            hot_x: hot_x as u16,
                            hot_y: hot_y as u16,
                            data: pixels,
                        })
                    } else {
                        DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
                            cache_index: 0,
                            width: width as u16,
                            height: height as u16,
                            hot_x: hot_x as u16,
                            hot_y: hot_y as u16,
                            data: pixels,
                        })
                    };
                    s.cursor_snapshot.shape = Some(Arc::new(shape));
                    s.cursor_snapshot.separate = true;
                    s.cursor_snapshot.visible = true;
                } else {
                    s.cursor_snapshot.separate = false;
                }
            }
            niri_rdp_cursor_v1::Event::Hidden => {
                s.cursor_snapshot.separate = s.cursor_max != 0;
                s.cursor_snapshot.visible = false;
            }
            niri_rdp_cursor_v1::Event::Unsupported => s.cursor_snapshot.separate = false,
        }
        s.publish_cursor();
    }
}
delegate_noop!(State: ignore NiriRdpCursorManagerV1);
impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
            && format == 0x34325258
        {
            let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
            if modifier != u64::MAX && s.dmabuf_xrgb_modifiers.len() < 128 {
                s.dmabuf_xrgb_modifiers.insert(modifier);
            }
        }
    }
}
impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        s: &mut Self,
        params: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if s.dmabuf_params.as_ref() != Some(&params.id()) {
            if let zwp_linux_buffer_params_v1::Event::Created { buffer } = event {
                buffer.destroy();
            }
            return;
        }
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                s.dmabuf_created = Some(buffer)
            }
            zwp_linux_buffer_params_v1::Event::Failed => s.dmabuf_failed = true,
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, ZwpLinuxBufferParamsV1, [0 => (wl_buffer::WlBuffer, ())]);
}
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);
delegate_noop!(State: ignore ZwlrOutputModeV1);
delegate_noop!(State: ignore ZwlrOutputConfigurationHeadV1);

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            let result = (|| -> anyhow::Result<String> {
                anyhow::ensure!(
                    format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                    "unsupported compositor keyboard map format"
                );
                read_keymap(File::from(fd), size)
            })();
            match result {
                // Unicode injection temporarily changes Niri's seat map. Never
                // adopt that synthetic map as the user's physical layout.
                Ok(text) if text.contains("xkb_keycodes \"niri_rdp_unicode\"") => {}
                Ok(text) => s.seat_keymap = Some(text),
                Err(error) => s.keymap_error = Some(error.to_string()),
            }
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZwlrOutputManagerV1,
        e: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_output_manager_v1::Event::Head { head } => {
                s.heads.push((head, String::new(), false, None))
            }
            zwlr_output_manager_v1::Event::Done { serial } => s.output_serial = Some(serial),
            zwlr_output_manager_v1::Event::Finished => s.output_manager = None,
            _ => {}
        }
    }
    wayland_client::event_created_child!(State, ZwlrOutputManagerV1, [0 => (ZwlrOutputHeadV1, ())]);
}
impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        s: &mut Self,
        head: &ZwlrOutputHeadV1,
        e: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(entry) = s.heads.iter_mut().find(|entry| &entry.0 == head) {
            match e {
                zwlr_output_head_v1::Event::Name { name } => entry.1 = name,
                zwlr_output_head_v1::Event::Enabled { enabled } => entry.2 = enabled != 0,
                zwlr_output_head_v1::Event::Scale { scale } => entry.3 = Some(scale),
                zwlr_output_head_v1::Event::Finished => s.heads.retain(|entry| &entry.0 != head),
                _ => {}
            }
        }
    }
    wayland_client::event_created_child!(State, ZwlrOutputHeadV1, [3 => (ZwlrOutputModeV1, ())]);
}
impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        e: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.configuration_result = match e {
            zwlr_output_configuration_v1::Event::Succeeded => Some(ConfigurationResult::Succeeded),
            zwlr_output_configuration_v1::Event::Failed => Some(ConfigurationResult::Failed),
            zwlr_output_configuration_v1::Event::Cancelled => Some(ConfigurationResult::Cancelled),
            _ => return,
        };
    }
}

fn resize_output(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    name: &str,
    target: DesktopSize,
    scale: f64,
    alive: &AtomicBool,
) -> anyhow::Result<()> {
    if s.output_policy_matches(name, target, scale)
        && s.applied_scale == Some(scale)
        && s.applied_size == Some(target)
    {
        return Ok(());
    }
    let started = Instant::now();
    while s.output_serial.is_none() || s.heads.iter().all(|head| head.1 != name) {
        anyhow::ensure!(
            alive.load(Ordering::Acquire),
            "desktop stopped during resize"
        );
        anyhow::ensure!(
            s.output_manager.is_some(),
            "missing output management protocol"
        );
        pump(c, q, s)?;
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(3),
            "output management timed out"
        );
    }
    let h = q.handle();
    loop {
        let manager = s
            .output_manager
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("output manager closed"))?;
        let config = manager.create_configuration(s.output_serial.unwrap(), &h, ());
        for (head, head_name, enabled, _) in &s.heads {
            if head_name == name {
                let conf = config.enable_head(head, &h, ());
                conf.set_custom_mode(target.width.into(), target.height.into(), 60_000);
                conf.set_scale(scale);
                conf.set_transform(wl_output::Transform::Normal);
                conf.set_position(0, 0);
            } else if *enabled {
                // Niri retains the existing properties of other enabled heads.
                config.enable_head(head, &h, ());
            } else {
                config.disable_head(head);
            }
        }
        s.configuration_result = None;
        config.apply();
        let result = (|| -> anyhow::Result<bool> {
            loop {
                anyhow::ensure!(
                    alive.load(Ordering::Acquire),
                    "desktop stopped during resize"
                );
                pump(c, q, s)?;
                anyhow::ensure!(
                    started.elapsed() < Duration::from_secs(3),
                    "output resize not applied"
                );
                match s.configuration_result {
                    Some(ConfigurationResult::Failed) => anyhow::bail!("output resize rejected"),
                    Some(ConfigurationResult::Cancelled) => return Ok(false),
                    Some(ConfigurationResult::Succeeded)
                        if s.output_policy_matches(name, target, scale) =>
                    {
                        return Ok(true);
                    }
                    _ => {}
                }
            }
        })();
        config.destroy();
        if result? {
            s.applied_scale = Some(scale);
            s.applied_size = Some(target);
            return Ok(());
        }
        // Geometry notifications can invalidate the serial between creating
        // and applying a configuration. Sync the updated metadata and retry
        // within the same deadline, using the compositor's latest serial.
        s.synced = false;
        c.display().sync(&h, ());
        while !s.synced {
            anyhow::ensure!(
                alive.load(Ordering::Acquire),
                "desktop stopped during resize"
            );
            pump(c, q, s)?;
            anyhow::ensure!(
                started.elapsed() < Duration::from_secs(3),
                "output resize retry timed out"
            );
        }
    }
}

fn pump(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
) -> anyhow::Result<()> {
    pump_with_timeout(c, q, s, 50)
}

fn pump_with_timeout(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    timeout_ms: i32,
) -> anyhow::Result<()> {
    pump_wake(c, q, s, timeout_ms, None)
}

fn complete_wayland_io<T>(
    result: Result<T, wayland_client::backend::WaylandError>,
) -> anyhow::Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(wayland_client::backend::WaylandError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            // Another coordinated event queue can consume socket readiness.
            // The next pump retries reads/flushes without losing pending events.
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn pump_wake(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    timeout_ms: i32,
    commands: Option<&Commands>,
) -> anyhow::Result<()> {
    let dispatched = q.dispatch_pending(s)?;
    // A full Wayland socket needs POLLOUT; never block while holding a read
    // guard or mistake command-only readiness for readable Wayland bytes.
    let want_write = match c.flush() {
        Ok(()) => false,
        Err(wayland_client::backend::WaylandError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            true
        }
        Err(error) => return Err(error.into()),
    };
    // Pending callbacks can already satisfy negotiation/readback. Recheck the
    // caller's condition before entering another potentially long poll; the
    // Wayland socket may now legitimately be quiet after those events.
    if dispatched > 0 {
        s.validate_output()?;
        return Ok(());
    }
    if let Some(guard) = q.prepare_read() {
        poll_prepared_wayland(c, q, s, guard, timeout_ms, commands, want_write)?;
    }
    q.dispatch_pending(s)?;
    s.validate_output()?;
    Ok(())
}

fn poll_prepared_wayland(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    guard: wayland_client::backend::ReadEventsGuard,
    timeout_ms: i32,
    commands: Option<&Commands>,
    want_write: bool,
) -> anyhow::Result<()> {
    // A different coordinated queue can read between dispatch_pending and
    // prepare_read, enqueueing our callbacks while draining socket readiness.
    // Once this guard is acquired, the backend prevents another socket read;
    // checking again here closes that window before any potentially long poll.
    if q.dispatch_pending(s)? > 0 {
        drop(guard);
        return Ok(());
    }
    let mut fds = [
        libc::pollfd {
            fd: c.as_fd().as_raw_fd(),
            events: libc::POLLIN | if want_write { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: commands.map_or(-1, |c| c.wake.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: both entries are initialized and valid for this call; -1 is ignored.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) };
    if n < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    if n > 0 && fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        complete_wayland_io(guard.read())?;
    } else {
        // Cancels prepared read on command wakes, deadlines, EINTR and POLLOUT.
        drop(guard);
    }
    if fds[1].revents & libc::POLLIN != 0 {
        commands.unwrap().clear_wake();
    }
    if want_write && fds[0].revents & libc::POLLOUT != 0 {
        complete_wayland_io(c.flush())?;
    }
    Ok(())
}

/// Resize is a geometry barrier. Cancel the pending capture before consuming
/// it, so later coordinates are delivered only after its output mode commits.
fn service_input(
    commands: &Commands,
    input: &mut Input,
    connection: &Connection,
    clipboard: &mut Option<crate::clipboard::ClipboardAttachment>,
    state: &mut State,
) -> bool {
    // A config reload can change scale without a client resize. Keep queued
    // coordinates behind the same geometry barrier as explicit layout changes.
    if state.remote_geometry_changed() {
        return true;
    }
    for _ in 0..256 {
        if matches!(
            commands.queue.lock().unwrap().front(),
            Some(Command::Resize(..) | Command::Stop)
        ) {
            return true;
        }
        match commands.pop() {
            Some(Command::Resize(..)) => unreachable!("single command consumer"),
            Some(Command::Status) => state.refresh_requested = true,
            Some(Command::Cursor(max)) => {
                state.set_cursor_max(max);
                return true;
            }
            Some(Command::Key(event)) => input.key_event(event),
            Some(Command::Mouse(event)) => input.mouse_event(event),
            Some(Command::Release) => input.release(),
            Some(Command::Clipboard(next)) => {
                if next.attach_connection(connection.clone()) {
                    *clipboard = Some(next);
                }
            }
            Some(Command::Stop) => return true,
            None => break,
        }
    }
    false
}

use crate::keyboard::UnicodeInput;

fn unicode_keymap(character: char) -> String {
    let symbol = match character {
        '\u{8}' => "BackSpace".into(),
        '\t' => "Tab".into(),
        '\r' | '\n' => "Return".into(),
        '\u{1b}' => "Escape".into(),
        '\u{7f}' => "Delete".into(),
        _ => format!("0x{:08x}", 0x01000000 | character as u32),
    };
    // A one-level map commits exact text, independently of Shift or CapsLock.
    // The caller restores the physical keymap and modifiers after the tap.
    format!(
        r#"xkb_keymap {{
        xkb_keycodes "niri_rdp_unicode" {{ minimum = 8; maximum = 255; <TEXT> = 255; }};
        xkb_types {{ type "ONE_LEVEL" {{ modifiers = None; level_name[Level1] = "Any"; }}; }};
        xkb_compatibility {{ }};
        xkb_symbols {{ key <TEXT> {{ type = "ONE_LEVEL", [ {symbol} ] }}; }};
    }};"#
    )
}

fn read_keymap(mut file: File, size: u32) -> anyhow::Result<String> {
    anyhow::ensure!(
        (1..=1024 * 1024).contains(&size),
        "invalid compositor keyboard map size"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(u64::from(size)).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() == size as usize && bytes.pop() == Some(0),
        "invalid compositor keyboard map data"
    );
    Ok(String::from_utf8(bytes)?)
}

struct PhysicalKeymap {
    file: File,
    size: u32,
    text: String,
    state: xkb::State,
}

impl PhysicalKeymap {
    fn new(text: String) -> anyhow::Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_string(
            &context,
            text.clone(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("invalid compositor XKB keymap"))?;
        let mut file = tempfile::tempfile()?;
        file.write_all(text.as_bytes())?;
        file.write_all(&[0])?;
        Ok(Self {
            file,
            size: text.len() as u32 + 1,
            text,
            state: xkb::State::new(&map),
        })
    }
}

struct Input {
    keyboard: ZwpVirtualKeyboardV1,
    text_keyboard: ZwpVirtualKeyboardV1,
    physical_keymap: PhysicalKeymap,
    unicode: UnicodeInput,
    pointer: ZwlrVirtualPointerV1,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    point: (u32, u32),
    size: (u32, u32),
    target: DesktopSize,
    scale: f64,
    started: Instant,
}
impl Input {
    fn update_keymap(&mut self, s: &mut State) -> anyhow::Result<()> {
        if let Some(error) = s.keymap_error.take() {
            anyhow::bail!(error);
        }
        if let Some(text) = s.seat_keymap.take()
            && text != self.physical_keymap.text
        {
            let mut next = PhysicalKeymap::new(text)?;
            // Reapply held keys when the user reloads the desktop layout.
            for key in &self.keys {
                next.state
                    .update_key((key + 8).into(), xkb::KeyDirection::Down);
            }
            let old = &self.physical_keymap.state;
            let old_map = old.get_keymap();
            let next_map = next.state.get_keymap();
            let old_locked = old.serialize_mods(xkb::STATE_MODS_LOCKED);
            let mut locked = 0;
            for index in 0..old_map.num_mods().min(32) {
                if old_locked & (1 << index) != 0 {
                    let next_index = next_map.mod_get_index(old_map.mod_get_name(index));
                    if next_index < 32 {
                        locked |= 1 << next_index;
                    }
                }
            }
            next.state.update_mask(
                next.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                0,
                locked,
                0,
                0,
                0,
            );
            self.physical_keymap = next;
        }
        Ok(())
    }
    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
    fn modifiers(&self) {
        self.keyboard.modifiers(
            self.physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_LATCHED),
            self.physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_LOCKED),
            self.physical_keymap
                .state
                .serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        );
    }
    fn key(&mut self, key: u32, pressed: bool) {
        let changed = if pressed {
            self.keys.insert(key)
        } else {
            self.keys.remove(&key)
        };
        if changed {
            self.keyboard.key(self.time(), key, u32::from(pressed));
            self.physical_keymap.state.update_key(
                (key + 8).into(),
                if pressed {
                    xkb::KeyDirection::Down
                } else {
                    xkb::KeyDirection::Up
                },
            );
            self.modifiers();
        }
    }
    fn key_event(&mut self, e: KeyboardEvent) {
        match e {
            KeyboardEvent::Pressed { code, extended } => {
                self.unicode = UnicodeInput::default();
                if let Some(k) = evdev_key(code, extended) {
                    self.key(k, true)
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(k) = evdev_key(code, extended) {
                    self.key(k, false)
                }
            }
            KeyboardEvent::Synchronize(flags) => {
                let km = self.physical_keymap.state.get_keymap();
                let mut locked = 0;
                // RDP synchronize bits: Scroll=1, Num=2, Caps=4, Kana=8.
                for (bit, name) in [(1, "ScrollLock"), (2, "NumLock"), (4, "Lock")] {
                    let i = km.mod_get_index(name);
                    if flags.bits() & bit != 0 && i < 32 {
                        locked |= 1 << i;
                    }
                }
                self.physical_keymap.state.update_mask(
                    self.physical_keymap
                        .state
                        .serialize_mods(xkb::STATE_MODS_DEPRESSED),
                    0,
                    locked,
                    0,
                    0,
                    0,
                );
                self.modifiers();
            }
            KeyboardEvent::UnicodePressed(unit) => {
                if let Some(character) = self.unicode.press(unit)
                    && let Err(error) = self.text(character)
                {
                    eprintln!("cannot inject Unicode keyboard input: {error}");
                }
            }
            // Unicode input describes committed text, rather than a held physical
            // key. Each completed scalar is sent as a balanced tap immediately.
            KeyboardEvent::UnicodeReleased(_) => {}
        }
    }
    fn text(&self, character: char) -> anyhow::Result<()> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_string(
            &ctx,
            unicode_keymap(character),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("Unicode XKB keymap unavailable"))?;
        let mut file = tempfile::tempfile()?;
        let mut text = km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        text.push('\0');
        file.write_all(text.as_bytes())?;
        self.text_keyboard
            .keymap(1, file.as_fd(), text.len() as u32);
        self.text_keyboard.modifiers(0, 0, 0, 0);
        self.text_keyboard.key(self.time(), 247, 1);
        self.text_keyboard.key(self.time(), 247, 0);
        // Committed Unicode uses this device's temporary map without changing
        // the seat. Restore its map and masks for subsequent protocol events.
        self.text_keyboard.keymap(
            1,
            self.physical_keymap.file.as_fd(),
            self.physical_keymap.size,
        );
        self.modifiers();
        Ok(())
    }
    fn move_to(&mut self, x: u32, y: u32) {
        let x = x.min(u32::from(self.target.width) - 1);
        let y = y.min(u32::from(self.target.height) - 1);
        self.point = (x, y);
        self.pointer.motion_absolute(
            self.time(),
            x * self.size.0 / u32::from(self.target.width),
            y * self.size.1 / u32::from(self.target.height),
            self.size.0,
            self.size.1,
        );
    }
    fn relative(&mut self, x: i32, y: i32) {
        let x = x.clamp(-i32::from(self.target.width), i32::from(self.target.width));
        let y = y.clamp(
            -i32::from(self.target.height),
            i32::from(self.target.height),
        );
        self.point = (
            (i64::from(self.point.0) + i64::from(x)).clamp(0, i64::from(self.target.width) - 1)
                as u32,
            (i64::from(self.point.1) + i64::from(y)).clamp(0, i64::from(self.target.height) - 1)
                as u32,
        );
        // Relative input follows the compositor's actual pointer, including
        // application warps/grabs. RDP deltas are physical pixels; Wayland's
        // relative motion is in compositor logical coordinates.
        self.pointer.motion(
            self.time(),
            f64::from(x) / self.scale,
            f64::from(y) / self.scale,
        );
    }

    fn button(&mut self, b: MouseButton, down: bool) {
        let code = match b {
            MouseButton::Left => 272,
            MouseButton::Right => 273,
            MouseButton::Middle => 274,
            MouseButton::X1 => 275,
            MouseButton::X2 => 276,
            _ => return,
        };
        let changed = if down {
            self.buttons.insert(code)
        } else {
            self.buttons.remove(&code)
        };
        if changed {
            self.pointer.button(
                self.time(),
                code,
                if down {
                    wl_seat_button_pressed()
                } else {
                    wayland_client::protocol::wl_pointer::ButtonState::Released
                },
            );
        }
    }
    fn scroll(&self, x: f64, y: f64) {
        use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
        self.pointer.axis_source(AxisSource::Wheel);
        if y != 0.0 {
            self.pointer.axis(self.time(), Axis::VerticalScroll, y);
        }
        if x != 0.0 {
            self.pointer.axis(self.time(), Axis::HorizontalScroll, x);
        }
    }
    fn mouse_event(&mut self, e: MouseEvent) {
        match e {
            MouseEvent::Move { x, y } => self.move_to(x.into(), y.into()),
            MouseEvent::RelMove { x, y } => self.relative(x, y),
            MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                self.move_to(x.into(), y.into());
                self.button(button, pressed);
            }
            MouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                self.relative(x, y);
                self.button(button, pressed);
            }
            MouseEvent::VerticalScroll { value } => self.scroll(0.0, -f64::from(value) / 12.0),
            MouseEvent::HorizontalScroll { value } => self.scroll(f64::from(value) / 12.0, 0.0),
            MouseEvent::Scroll { x, y } => self.scroll(f64::from(x), -f64::from(y)),
            _ => {}
        }
        self.pointer.frame();
    }
    fn release(&mut self) {
        self.unicode = UnicodeInput::default();
        for k in self.keys.clone() {
            self.key(k, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.pointer.button(
                self.time(),
                b,
                wayland_client::protocol::wl_pointer::ButtonState::Released,
            );
        }
        self.pointer.frame();
    }
}
fn wl_seat_button_pressed() -> wayland_client::protocol::wl_pointer::ButtonState {
    wayland_client::protocol::wl_pointer::ButtonState::Pressed
}

/// Keep one SHM allocation and Wayland buffer for the current capture layout.
/// The compositor writes it only between copy and ready; conversion finishes
/// before it is submitted again, so no Rust slice overlaps a compositor write.
struct CaptureBuffer {
    layout: (wl_shm::Format, u32, u32, u32),
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    mapping: *mut libc::c_void,
    length: usize,
    _file: File,
}
impl CaptureBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        layout: (wl_shm::Format, u32, u32, u32),
        h: &QueueHandle<State>,
    ) -> anyhow::Result<Self> {
        let (format, width, height, stride) = layout;
        let length = buffer_length(width, height, stride)?;
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
        let pool = shm.create_pool(file.as_fd(), length as i32, h, ());
        let buffer =
            pool.create_buffer(0, width as i32, height as i32, stride as i32, format, h, ());
        Ok(Self {
            layout,
            buffer,
            pool,
            mapping,
            length,
            _file: file,
        })
    }
    /// Call only after ready and before the next copy request.
    fn bytes(&self) -> &[u8] {
        // SAFETY: mapping covers length readable bytes for this object's lifetime.
        // Its caller observes the screencopy ready barrier before reading.
        unsafe { std::slice::from_raw_parts(self.mapping.cast(), self.length) }
    }
}
impl Drop for CaptureBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        // SAFETY: this object owns the exact mapping returned by mmap.
        unsafe { libc::munmap(self.mapping, self.length) };
    }
}

struct GpuBuffer {
    owner: Arc<crate::gpu_capture::Buffer>,
    buffer: wl_buffer::WlBuffer,
}
impl Drop for GpuBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}
struct InputWait<'a> {
    commands: &'a Commands,
    input: &'a mut Input,
    clipboard: &'a mut Option<crate::clipboard::ClipboardAttachment>,
    alive: &'a AtomicBool,
}
fn import_gpu_buffer(
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

struct WorkerChannels {
    commands: Arc<Commands>,
    frames: LatestFrames,
    ready: mpsc::SyncSender<anyhow::Result<()>>,
    alive: Arc<AtomicBool>,
    frame_rate: Arc<AtomicU32>,
    gpu: Arc<GpuCapture>,
    cursor_tx: tokio::sync::watch::Sender<CursorSnapshot>,
    capture_metadata: CaptureMetadata,
    status: Arc<OutputStatus>,
}
fn run(
    socket: UnixStream,
    name: &str,
    mut target: DesktopSize,
    mut scale: f64,
    worker: WorkerChannels,
) -> anyhow::Result<()> {
    let WorkerChannels {
        commands,
        frames,
        ready,
        alive,
        frame_rate,
        gpu,
        cursor_tx,
        capture_metadata,
        status,
    } = worker;
    let c = Connection::from_socket(socket)?;
    let mut q = c.new_event_queue();
    let h = q.handle();
    let mut s = State {
        cursor_tx: Some(cursor_tx),
        ..State::default()
    };
    c.display().get_registry(&h, ());
    // Bound roundtrips with a monotonic deadline through nonblocking dispatch.
    let startup = Instant::now();
    c.display().sync(&h, ());
    while !s.synced {
        pump(&c, &mut q, &mut s)?;
        anyhow::ensure!(
            startup.elapsed() < Duration::from_secs(5),
            "Wayland registry timed out"
        );
    }
    // Initial globals and output metadata arrive over the same ordered stream.
    let wait = Instant::now();
    while s.outputs.iter().all(|o| o.2 != name) {
        pump(&c, &mut q, &mut s)?;
        anyhow::ensure!(
            wait.elapsed() < Duration::from_secs(5),
            "requested output unavailable"
        );
    }
    let output = s.outputs.iter().find(|o| o.2 == name).unwrap();
    anyhow::ensure!(
        output.3 > 0 && output.4,
        "bridge requires positive scale and normal transform"
    );
    s.selected_output = Some(output.0);
    let output = output.1.clone();
    resize_output(&c, &mut q, &mut s, name, target, scale, &alive)?;
    let seat = s
        .seat
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing Wayland seat"))?
        .clone();
    s.seat_keyboard = Some(seat.get_keyboard(&h, ()));
    let keymap_deadline = Instant::now();
    while s.seat_keymap.is_none() {
        pump(&c, &mut q, &mut s)?;
        anyhow::ensure!(
            s.keymap_error.is_none(),
            "{}",
            s.keymap_error.as_deref().unwrap_or("")
        );
        anyhow::ensure!(
            keymap_deadline.elapsed() < Duration::from_secs(5),
            "compositor keyboard map timed out"
        );
    }
    let physical_keymap = PhysicalKeymap::new(s.seat_keymap.take().unwrap())?;
    let keyboard = s
        .keyboard_manager
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing virtual keyboard protocol"))?
        .create_virtual_keyboard(&seat, &h, ());
    let text_keyboard = s
        .keyboard_manager
        .as_ref()
        .unwrap()
        .create_virtual_keyboard(&seat, &h, ());
    let pm = s
        .pointer_manager
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing virtual pointer protocol"))?;
    anyhow::ensure!(
        pm.version() >= 2,
        "virtual pointer output mapping requires version 2"
    );
    let pointer = pm.create_virtual_pointer_with_output(Some(&seat), Some(&output), &h, ());
    keyboard.keymap(1, physical_keymap.file.as_fd(), physical_keymap.size);
    let mut input = Input {
        keyboard,
        text_keyboard,
        physical_keymap,
        unicode: UnicodeInput::default(),
        pointer,
        keys: BTreeSet::new(),
        buttons: BTreeSet::new(),
        point: (0, 0),
        size: (1, 1),
        target,
        scale: s.observed_scale(name).unwrap_or(scale),
        started: Instant::now(),
    };
    let capture = s
        .capture
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing screencopy protocol"))?;
    let mut clipboard: Option<crate::clipboard::ClipboardAttachment> = None;
    let mut telemetry = crate::performance::Telemetry::from_env();
    let result = (|| -> anyhow::Result<()> {
        let mut first = true;
        let mut shared: Option<CaptureBuffer> = None;
        let mut gpu_allocator: Option<crate::gpu_capture::Allocator> = None;
        let mut gpu_buffers: Vec<GpuBuffer> = Vec::new();
        let mut gpu_size = (0, 0);
        let mut gpu_sequence = 0u64;
        let mut previous_gpu = false;
        let mut previous_separate_cursor = false;
        let mut pause_health_at = Instant::now();
        let mut pause_health_pending: Option<Instant> = None;
        let mut next_capture = Instant::now();
        let mut latest: Option<Arc<DisplayUpdate>> = None;
        'capture: loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            pump_wake(&c, &mut q, &mut s, 0, Some(&commands))?;
            input.update_keymap(&mut s)?;
            if s.remote_geometry_changed() {
                // Niri config reloads may overwrite runtime output settings.
                // Restore the authenticated remote policy before input or pixels.
                resize_output(&c, &mut q, &mut s, name, target, scale, &alive)?;
                latest = None;
                next_capture = Instant::now();
                telemetry.count("capture_output_policy_restored", 1);
            }
            input.scale = s.observed_scale(name).unwrap_or(scale);
            // Adjacent resizes were already collapsed in the bounded mailbox.
            // Input behind a resize remains ordered against the committed mode.
            for _ in 0..256 {
                if !alive.load(Ordering::Acquire) {
                    return Ok(());
                }
                match commands.pop() {
                    Some(Command::Resize(next, next_scale))
                        if next != target || next_scale != scale =>
                    {
                        resize_output(&c, &mut q, &mut s, name, next, next_scale, &alive)?;
                        target = next;
                        scale = next_scale;
                        input.target = target;
                        input.scale = s.observed_scale(name).unwrap_or(scale);
                        input.size = (u32::from(target.width), u32::from(target.height));
                        input.point = (
                            input.point.0.min(u32::from(target.width) - 1),
                            input.point.1.min(u32::from(target.height) - 1),
                        );
                        // A new geometry requires immediate complete pixels.
                        latest = None;
                        next_capture = Instant::now();
                    }
                    Some(Command::Resize(..)) => {}
                    Some(Command::Status) => s.refresh_requested = true,
                    Some(Command::Cursor(max)) => s.set_cursor_max(max),
                    Some(Command::Key(event)) => input.key_event(event),
                    Some(Command::Mouse(event)) => input.mouse_event(event),
                    Some(Command::Release) => input.release(),
                    Some(Command::Clipboard(next)) => {
                        if next.attach_connection(c.clone()) {
                            clipboard = Some(next);
                        }
                    }
                    Some(Command::Stop) => return Ok(()),
                    None => break,
                }
            }
            if s.cursor_max != 0
                && s.cursor.is_none()
                && let Some(manager) = &s.cursor_manager
            {
                s.cursor = Some(manager.get_cursor(&output, &h, ()));
            }
            if previous_separate_cursor != s.cursor_snapshot.separate {
                latest = None;
                previous_separate_cursor = s.cursor_snapshot.separate;
                next_capture = Instant::now();
            }
            if s.refresh_requested {
                if latest.is_some() {
                    frames.send_replace(latest.clone());
                }
                s.refresh_requested = false;
            }
            if status.suppressed.load(Ordering::Acquire) && latest.is_some() {
                let now = Instant::now();
                if s.synced {
                    pause_health_pending = None;
                }
                anyhow::ensure!(
                    pause_health_pending.is_none_or(|sent| sent.elapsed() < Duration::from_secs(3)),
                    "suppressed compositor health check timed out"
                );
                if now >= pause_health_at {
                    frames.send_replace(latest.clone());
                    if pause_health_pending.is_none() {
                        s.synced = false;
                        c.display().sync(&h, ());
                        pause_health_pending = Some(now);
                    }
                    pause_health_at = now + Duration::from_secs(1);
                    telemetry.count("capture_suppressed_heartbeats", 1);
                    telemetry.report("capture", now);
                }
                pump_wake(
                    &c,
                    &mut q,
                    &mut s,
                    pause_health_at
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(1000) as i32,
                    Some(&commands),
                )?;
                continue;
            }
            pause_health_at = Instant::now();
            pause_health_pending = None;
            let delay = next_capture.saturating_duration_since(Instant::now());
            if !delay.is_zero() {
                pump_wake(
                    &c,
                    &mut q,
                    &mut s,
                    delay.as_millis().min(i32::MAX as u128) as i32,
                    Some(&commands),
                )?;
                continue;
            }
            let frame_started = Instant::now();
            s.dimensions = None;
            s.gpu_format = None;
            s.buffer_done = false;
            s.ready = false;
            s.failed = false;
            s.inverted = false;
            s.damage.clear();
            s.full_damage = false;
            let frame = capture.capture_output(
                u32::from(!s.cursor_snapshot.separate) as i32,
                &output,
                &h,
                (),
            );
            s.active_frame = Some(frame.id());
            let deadline = Instant::now();
            while s.dimensions.is_none() || (capture.version() >= 3 && !s.buffer_done) {
                if !alive.load(Ordering::Acquire) {
                    frame.destroy();
                    return Ok(());
                }
                if service_input(&commands, &mut input, &c, &mut clipboard, &mut s) {
                    frame.destroy();
                    continue 'capture;
                }
                pump_wake(
                    &c,
                    &mut q,
                    &mut s,
                    3000 - deadline.elapsed().as_millis().min(3000) as i32,
                    Some(&commands),
                )?;
                anyhow::ensure!(
                    !s.failed,
                    "compositor rejected capture negotiation (buffer={:?}, buffer_done={})",
                    s.dimensions,
                    s.buffer_done,
                );
                anyhow::ensure!(
                    deadline.elapsed() < Duration::from_secs(3),
                    "capture negotiation timed out after {:?} (buffer={:?}, buffer_done={})",
                    deadline.elapsed(),
                    s.dimensions,
                    s.buffer_done,
                );
            }
            telemetry.sample("capture_negotiation", deadline.elapsed());
            if s.remote_geometry_changed() {
                frame.destroy();
                continue 'capture;
            }
            let (format, w, height, stride) = s.dimensions.unwrap();
            let abgr = match format {
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => false,
                wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => true,
                _ => anyhow::bail!("unsupported SHM format"),
            };
            let layout = (format, w, height, stride);
            let wants_gpu =
                gpu.requested.load(Ordering::Acquire) && !gpu.failed.load(Ordering::Acquire);
            if previous_gpu != wants_gpu {
                latest = None;
                previous_gpu = wants_gpu;
                if !wants_gpu {
                    gpu.latest.lock().unwrap().take();
                }
            }
            let mut gpu_slot = None;
            let mut gpu_import_interrupted = false;
            if wants_gpu {
                let prepare = (|| -> anyhow::Result<()> {
                    anyhow::ensure!(
                        w == u32::from(target.width) && height == u32::from(target.height),
                        "GPU capture geometry mismatch"
                    );
                    anyhow::ensure!(
                        s.gpu_format == Some((0x34325258, w, height)),
                        "GPU screencopy format unavailable"
                    );
                    anyhow::ensure!(
                        !s.dmabuf_xrgb_modifiers.is_empty(),
                        "XRGB capture modifiers unavailable"
                    );
                    if gpu_allocator.is_none() {
                        gpu_allocator = Some(crate::gpu_capture::Allocator::new()?);
                    }
                    if gpu_size != (w, height) {
                        gpu_buffers.clear();
                        gpu.latest.lock().unwrap().take();
                        gpu_size = (w, height);
                    }
                    if gpu_buffers.len() < 3 {
                        let owner = gpu_allocator.as_ref().unwrap().allocate(
                            w,
                            height,
                            &s.dmabuf_xrgb_modifiers.iter().copied().collect::<Vec<_>>(),
                        )?;
                        if let Some(buffer) = import_gpu_buffer(
                            &c,
                            &mut q,
                            &mut s,
                            owner,
                            (w, height),
                            InputWait {
                                commands: &commands,
                                input: &mut input,
                                clipboard: &mut clipboard,
                                alive: &alive,
                            },
                        )? {
                            gpu_buffers.push(buffer);
                        } else {
                            gpu_import_interrupted = true;
                            return Ok(());
                        }
                    }
                    gpu_slot = gpu_buffers
                        .iter()
                        .position(|buffer| Arc::strong_count(&buffer.owner) == 1);
                    Ok(())
                })();
                if gpu_import_interrupted {
                    frame.destroy();
                    next_capture = Instant::now();
                    continue 'capture;
                }
                if let Err(error) = prepare {
                    eprintln!("GPU capture unavailable; using SHM: {error}");
                    gpu.failed.store(true, Ordering::Release);
                    gpu.requested.store(false, Ordering::Release);
                    gpu.latest.lock().unwrap().take();
                    previous_gpu = false;
                    latest = None;
                } else if gpu_slot.is_none() {
                    // Active/canceled encoder tasks own every pool slot. Keep
                    // work bounded and never overwrite an encoder's DMA-BUF.
                    frame.destroy();
                    next_capture = Instant::now() + Duration::from_millis(5);
                    continue;
                }
            }

            if shared.as_ref().is_none_or(|buffer| buffer.layout != layout) {
                shared = Some(CaptureBuffer::new(
                    s.shm
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("missing SHM"))?,
                    layout,
                    &h,
                )?);
            }
            let buffer = shared.as_ref().unwrap();
            let capture_buffer = gpu_slot.map_or(&buffer.buffer, |slot| &gpu_buffers[slot].buffer);
            let damage_wait = latest.is_some() && frame.version() >= 2;
            if damage_wait {
                frame.copy_with_damage(capture_buffer);
            } else {
                frame.copy(capture_buffer);
            }
            input.size = (w, height);
            // CopyWithDamage intentionally waits forever on a quiet desktop.
            // Lightweight display.sync health probes distinguish idle from a
            // dead compositor without periodic readback. Heartbeats reuse Bytes.
            let mut health_at = Instant::now() + Duration::from_secs(1);
            let mut health_pending: Option<Instant> = None;
            while !s.ready {
                if status.suppressed.load(Ordering::Acquire) && latest.is_some() {
                    frame.destroy();
                    continue 'capture;
                }

                if previous_gpu != gpu.requested.load(Ordering::Acquire)
                    || previous_separate_cursor != s.cursor_snapshot.separate
                {
                    frame.destroy();
                    next_capture = Instant::now();
                    continue 'capture;
                }
                if !alive.load(Ordering::Acquire) {
                    frame.destroy();
                    return Ok(());
                }
                if service_input(&commands, &mut input, &c, &mut clipboard, &mut s) {
                    frame.destroy();
                    continue 'capture;
                }
                if s.refresh_requested {
                    frames.send_replace(latest.clone());
                    s.refresh_requested = false;
                }
                let now = Instant::now();
                if damage_wait && now >= health_at {
                    if let Some(image) = &latest {
                        frames.send_replace(Some(image.clone()));
                    }
                    if health_pending.is_none() {
                        s.synced = false;
                        c.display().sync(&h, ());
                        health_pending = Some(now);
                    }
                    telemetry.count("capture_idle_heartbeats", 1);
                    telemetry.report("capture", now);
                    health_at = now + Duration::from_secs(1);
                }
                if s.synced {
                    health_pending = None;
                }
                if s.failed && gpu_slot.is_some() {
                    gpu.failed.store(true, Ordering::Release);
                    gpu.requested.store(false, Ordering::Release);
                    frame.destroy();
                    next_capture = Instant::now();
                    continue 'capture;
                }
                anyhow::ensure!(!s.failed, "capture failed");
                anyhow::ensure!(
                    damage_wait || deadline.elapsed() < Duration::from_secs(3),
                    "capture timed out"
                );
                anyhow::ensure!(
                    health_pending.is_none_or(|sent| sent.elapsed() < Duration::from_secs(3)),
                    "compositor health check timed out"
                );
                let timeout = if damage_wait {
                    health_at
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(1000) as i32
                } else {
                    3000 - deadline.elapsed().as_millis().min(3000) as i32
                };
                pump_wake(&c, &mut q, &mut s, timeout, Some(&commands))?;
                input.update_keymap(&mut s)?;
            }
            if s.remote_geometry_changed() {
                frame.destroy();
                continue 'capture;
            }
            telemetry.sample(
                if damage_wait {
                    "capture_damage_wait_and_readback"
                } else {
                    "capture_wait_readback"
                },
                deadline.elapsed(),
            );
            telemetry.count("capture_damage_rectangles", s.damage.len() as u64);
            telemetry.count("capture_damage_overflow", u64::from(s.full_damage));
            let snapshot_started = Instant::now();
            // Full snapshots retain every change even when watch notifications
            // are overwritten. Partial bitmap diffs compare each receiver's last
            // delivered full desktop, never only the latest compositor damage.
            let image = if let Some(slot) = gpu_slot {
                gpu_sequence = gpu_sequence.wrapping_add(1);
                let owner = gpu_buffers[slot].owner.clone();
                let ticket = Arc::new(GpuFrame {
                    fd: owner.fd()?,
                    stride: owner.stride,
                    fourcc: 0x34325258,
                    modifier: owner.modifier,
                    width: w,
                    height,
                    offset: owner.offset,
                    device: owner.device(),
                    sequence: gpu_sequence,
                    inverted: s.inverted,
                    captured_at: snapshot_started,
                    _owner: owner,
                });
                *gpu.latest.lock().unwrap() = Some(ticket);
                let mut token = b"NIRIGPU1".to_vec();
                token.extend_from_slice(&gpu_sequence.to_le_bytes());
                ironrdp_server::BitmapUpdate {
                    x: u16::MAX,
                    y: u16::MAX,
                    width: std::num::NonZeroU16::new(target.width).unwrap(),
                    height: std::num::NonZeroU16::new(target.height).unwrap(),
                    format: ironrdp_server::PixelFormat::BgrX32,
                    data: token.into(),
                    stride: std::num::NonZeroUsize::new(usize::from(target.width) * 4).unwrap(),
                }
            } else {
                capture_bitmap(buffer.bytes(), w, height, stride, s.inverted, target, abgr)?
            };
            let changed = latest.as_ref().is_none_or(|old| match old.as_ref() {
                DisplayUpdate::Bitmap(old) => {
                    old.width != image.width
                        || old.height != image.height
                        || old.format != image.format
                        || old.data != image.data
                }
                _ => true,
            });
            telemetry.sample("capture_snapshot", snapshot_started.elapsed());
            telemetry.count(
                if gpu_slot.is_some() {
                    "gpu_capture_stride_bytes"
                } else {
                    "capture_bytes"
                },
                u64::from(gpu_slot.map_or(stride, |slot| gpu_buffers[slot].owner.stride))
                    * u64::from(height),
            );
            telemetry.count(
                if changed {
                    "capture_frames"
                } else {
                    "capture_suppressed"
                },
                1,
            );
            telemetry.report("capture", Instant::now());
            if changed {
                *capture_metadata.lock().unwrap() = Some((
                    image.data.as_ptr() as usize,
                    image.data.len(),
                    snapshot_started,
                ));
                latest = Some(Arc::new(DisplayUpdate::Bitmap(image)));
                frames.send_replace(latest.clone());
            }
            frame.destroy();
            if first {
                let _ = ready.send(Ok(()));
                first = false;
            }
            let fps = frame_rate.load(Ordering::Acquire).clamp(1, 60);
            next_capture = frame_started + Duration::from_secs_f64(1.0 / f64::from(fps));
        }
    })();
    if let Some(clipboard) = clipboard {
        clipboard.clear();
    }
    input.release();
    input.keyboard.destroy();
    input.text_keyboard.destroy();
    input.pointer.destroy();
    // A successful flush only hands bytes to the socket. Await wl_display.sync
    // so Niri processes releases/destruction before this client socket closes;
    // otherwise a readable+hangup wake may discard final injected releases.
    s.synced = false;
    c.display().sync(&h, ());
    let cleanup_deadline = Instant::now();
    while !s.synced && cleanup_deadline.elapsed() < Duration::from_millis(500) {
        if pump(&c, &mut q, &mut s).is_err() {
            break;
        }
    }
    result
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for State {
    fn event(
        s: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.synced = true;
    }
}

#[cfg(test)]
pub(crate) mod lifecycle_tests {
    use super::*;
    use crate::bitmap;
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
    fn pending_wayland_completion_is_dispatched_without_an_extra_timeout() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut queue = connection.new_event_queue();
        let callback = connection.display().sync(&queue.handle(), ());
        connection.flush().unwrap();
        let mut reply = Vec::new();
        reply.extend_from_slice(&callback.id().protocol_id().to_ne_bytes());
        reply.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        reply.extend_from_slice(&1u32.to_ne_bytes());
        compositor.write_all(&reply).unwrap();
        queue.prepare_read().unwrap().read().unwrap();
        let mut state = State::default();
        let started = Instant::now();
        pump_wake(&connection, &mut queue, &mut state, 1000, None).unwrap();
        assert!(state.synced);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a completed callback must not consume the next capture deadline"
        );
    }
    #[test]
    fn coordinated_empty_wayland_read_and_full_flush_are_retryable() {
        let (socket, _compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<State>();
        // Reading after another queue drained readiness is a normal condition.
        complete_wayland_io(queue.prepare_read().unwrap().read()).unwrap();
        complete_wayland_io::<()>(Err(wayland_client::backend::WaylandError::Io(
            std::io::Error::from(std::io::ErrorKind::WouldBlock),
        )))
        .unwrap();
        assert!(
            complete_wayland_io::<()>(Err(wayland_client::backend::WaylandError::Io(
                std::io::Error::from(std::io::ErrorKind::BrokenPipe),
            )))
            .is_err()
        );
        let mut state = State::default();
        pump_wake(
            &connection,
            &mut connection.new_event_queue(),
            &mut state,
            0,
            None,
        )
        .unwrap();
    }
    #[test]
    fn foreign_queue_read_between_dispatch_and_prepare_does_not_sleep_on_drained_socket() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut capture = connection.new_event_queue::<State>();
        let clipboard = connection.clone().new_event_queue::<State>();
        let callback = connection.display().sync(&capture.handle(), ());
        connection.flush().unwrap();
        let mut state = State::default();
        assert_eq!(capture.dispatch_pending(&mut state).unwrap(), 0);
        let mut reply = Vec::new();
        reply.extend_from_slice(&callback.id().protocol_id().to_ne_bytes());
        reply.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        reply.extend_from_slice(&1u32.to_ne_bytes());
        compositor.write_all(&reply).unwrap();
        // Deterministically model the clipboard reader winning immediately
        // after capture's first dispatch and before its prepared-read guard.
        clipboard.prepare_read().unwrap().read().unwrap();
        let mut fd = libc::pollfd {
            fd: connection.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd is initialized and refers to the live connection.
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, 0) },
            0,
            "foreign reader drained socket readiness"
        );
        let guard = capture.prepare_read().unwrap();
        let started = Instant::now();
        poll_prepared_wayland(
            &connection,
            &mut capture,
            &mut state,
            guard,
            1000,
            None,
            false,
        )
        .unwrap();
        // pump_wake dispatches again after its prepared-read polling stage.
        capture.dispatch_pending(&mut state).unwrap();
        assert!(
            state.synced,
            "completion queued by the foreign reader must dispatch"
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "queued completion must not wait for quiet socket readiness"
        );
        // The early return canceled the guard; later prepared reads still work.
        drop(capture.prepare_read().unwrap());
    }
    #[test]
    fn input_wakes_pending_capture_without_waiting_for_wayland_readiness() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        compositor
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut events = connection.new_event_queue();
        let queue = Commands::new().unwrap();
        let producer = queue.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer
                .try_send(Command::Key(KeyboardEvent::Pressed {
                    code: 30,
                    extended: false,
                }))
                .unwrap();
        });
        let handle = events.handle();
        let registry = connection.display().get_registry(&handle, ());
        let seat: wl_seat::WlSeat = registry.bind(1, 7, &handle, ());
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = registry.bind(2, 1, &handle, ());
        let pointer_manager: ZwlrVirtualPointerManagerV1 = registry.bind(3, 2, &handle, ());
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut input = Input {
            keyboard: keyboard_manager.create_virtual_keyboard(&seat, &handle, ()),
            text_keyboard: keyboard_manager.create_virtual_keyboard(&seat, &handle, ()),
            physical_keymap: PhysicalKeymap::new(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
                .unwrap(),
            unicode: UnicodeInput::default(),
            pointer: pointer_manager.create_virtual_pointer(Some(&seat), &handle, ()),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            point: (0, 0),
            size: (640, 480),
            target: DesktopSize {
                width: 640,
                height: 480,
            },
            scale: 1.0,
            started: Instant::now(),
        };
        let started = Instant::now();
        let mut state = State::default();
        // No compositor negotiation/ready message is sent. The only readable
        // descriptor is the command eventfd, which must cancel prepare_read.
        pump_wake(&connection, &mut events, &mut state, 1000, Some(&queue)).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!state.ready);
        assert!(!service_input(
            &queue,
            &mut input,
            &connection,
            &mut None,
            &mut state
        ));
        connection.flush().unwrap();
        assert!(input.keys.contains(&30));
        let mut wire = [0u8; 4096];
        let size = compositor.read(&mut wire).unwrap();
        let mut offset = 0;
        let mut delivered = false;
        while offset + 8 <= size {
            let object = u32::from_ne_bytes(wire[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(wire[offset + 4..offset + 8].try_into().unwrap());
            let length = (header >> 16) as usize;
            if object == input.keyboard.id().protocol_id() && (header & 0xffff) == 1 {
                assert_eq!(
                    u32::from_ne_bytes(wire[offset + 12..offset + 16].try_into().unwrap()),
                    30
                );
                delivered = true;
            }
            offset += length;
        }
        assert!(
            delivered,
            "virtual key must reach the compositor socket before capture ready"
        );
        assert!(!state.ready);
        input.scale = 1.5;
        input.point = (320, 240);
        input.relative(15, -30);
        input.pointer.frame();
        connection.flush().unwrap();
        let size = compositor.read(&mut wire).unwrap();
        let mut offset = 0;
        let mut relative_delivered = false;
        while offset + 8 <= size {
            let object = u32::from_ne_bytes(wire[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(wire[offset + 4..offset + 8].try_into().unwrap());
            if object == input.pointer.id().protocol_id() && header & 0xffff == 0 {
                assert_eq!(
                    i32::from_ne_bytes(wire[offset + 12..offset + 16].try_into().unwrap()),
                    10 * 256
                );
                assert_eq!(
                    i32::from_ne_bytes(wire[offset + 16..offset + 20].try_into().unwrap()),
                    -20 * 256
                );
                relative_delivered = true;
            }
            offset += (header >> 16) as usize;
        }
        assert!(
            relative_delivered,
            "physical deltas must reach Wayland as scaled relative motion"
        );
        assert_eq!(input.point, (335, 210));
        drop(input);
        drop(seat);
        drop(keyboard_manager);
        drop(pointer_manager);
        drop(registry);
        drop(handle);
        // A later prepared read remains valid after cancellation and shutdown.
        queue.try_send(Command::Stop).unwrap();
        pump_wake(&connection, &mut events, &mut state, 1000, Some(&queue)).unwrap();
        assert!(matches!(queue.pop(), Some(Command::Stop)));
        worker.join().unwrap();
        drop(events);
        drop(connection);
        let mut bytes = [0u8; 16];
        assert_eq!(compositor.read(&mut bytes).unwrap(), 0);
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
    #[test]
    fn fractional_head_scale_detects_reload_with_unchanged_integer_scale() {
        let (socket, _compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let events = connection.new_event_queue::<State>();
        let handle = events.handle();
        let registry = connection.display().get_registry(&handle, ());
        let head: ZwlrOutputHeadV1 = registry.bind(1, 1, &handle, ());
        let output: wl_output::WlOutput = registry.bind(2, 4, &handle, ());
        let size = DesktopSize {
            width: 640,
            height: 480,
        };
        let mut state = State {
            heads: vec![(head.clone(), "HEADLESS-1".into(), true, Some(1.5))],
            outputs: vec![(2, output, "HEADLESS-1".into(), 2, true, (640, 480))],
            selected_output: Some(2),
            applied_scale: Some(1.5),
            applied_size: Some(size),
            ..State::default()
        };
        assert!(!state.remote_geometry_changed());
        <State as Dispatch<ZwlrOutputHeadV1, ()>>::event(
            &mut state,
            &head,
            zwlr_output_head_v1::Event::Scale { scale: 1.25 },
            &(),
            &connection,
            &handle,
        );
        assert_eq!(state.observed_scale("HEADLESS-1"), Some(1.25));
        assert!(state.remote_geometry_changed());
        state.heads[0].3 = Some(1.5);
        assert!(!state.remote_geometry_changed());
        state.heads[0].3 = Some(281.0 / 256.0);
        assert!(state.output_policy_matches("HEADLESS-1", size, 1.1));
        state.outputs[0].5 = (1920, 1080);
        assert!(state.remote_geometry_changed());
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
    #[test]
    fn cursor_masks_are_bounded_bottom_up_bgra_with_straight_alpha() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&[10, 20, 30, 255, 20, 10, 5, 128]).unwrap();
        let image = cursor_pixels(file.try_clone().unwrap(), 1, 2, 0, 1).unwrap();
        assert_eq!(image, [10, 20, 40, 128, 30, 20, 10, 255]);
        for (width, height, hot_x, hot_y) in [
            (0, 2, 0, 0),
            (385, 1, 0, 0),
            (1, 2, 1, 0),
            (1, 2, 0, 2),
            (1, 1, 0, 0),
        ] {
            assert!(cursor_pixels(file.try_clone().unwrap(), width, height, hot_x, hot_y).is_err());
        }
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
    fn jp106_keymap_preserves_jis_punctuation_and_extra_keys() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "jp106",
            "jp",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let inherited = PhysicalKeymap::new(km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
        let mut state = inherited.state;
        for (evdev, character) in [
            (26u32, "@"),
            (27, "["),
            (39, ";"),
            (40, ":"),
            (43, "]"),
            (89, "\\"),
            (124, "\\"),
        ] {
            assert_eq!(state.key_get_utf8((evdev + 8).into()), character);
        }
        state.update_key(50u32.into(), xkb::KeyDirection::Down);
        for (evdev, character) in [(3u32, "\""), (89, "_"), (124, "|")] {
            assert_eq!(state.key_get_utf8((evdev + 8).into()), character);
        }
    }
    #[test]
    fn inherited_compositor_keymaps_preserve_swedish_us_and_multiple_groups() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for (layout, expected) in [
            ("se", [(26u32, "å"), (39, "ö"), (40, "ä")]),
            ("us", [(26u32, "["), (39, ";"), (40, "'")]),
        ] {
            let map = xkb::Keymap::new_from_names(
                &context,
                "evdev",
                "pc105",
                layout,
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let inherited =
                PhysicalKeymap::new(map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
            let text = read_keymap(inherited.file.try_clone().unwrap(), inherited.size).unwrap();
            let restored = PhysicalKeymap::new(text).unwrap();
            for (key, character) in expected {
                assert_eq!(restored.state.key_get_utf8((key + 8).into()), character);
            }
        }
        let map = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us,se",
            "",
            Some("grp:alt_shift_toggle".into()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut inherited =
            PhysicalKeymap::new(map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
        assert_eq!(inherited.state.get_keymap().num_layouts(), 2);
        inherited.state.update_mask(0, 0, 0, 0, 0, 1);
        assert_eq!(inherited.state.key_get_utf8(34u32.into()), "å");
        inherited.state.update_mask(0, 0, 0, 0, 0, 0);
        assert_eq!(inherited.state.key_get_utf8(34u32.into()), "[");
    }
    #[test]
    fn compositor_keymap_reader_rejects_truncated_and_unbounded_data() {
        for (bytes, size) in [
            (b"abc".as_slice(), 4),
            (b"abc".as_slice(), 3),
            (b"abc\0".as_slice(), 1024 * 1024 + 1),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            assert!(read_keymap(file, size).is_err());
        }
    }
    #[test]
    fn unicode_input_decodes_utf16_and_recovers_from_malformed_pairs() {
        let mut input = UnicodeInput::default();
        assert_eq!(input.press(0x3042), Some('あ'));
        assert_eq!(input.press(0xd83d), None);
        assert_eq!(input.press(0xde00), Some('😀'));
        assert_eq!(input.press(0xde00), None);
        assert_eq!(input.press(0xd83d), None);
        assert_eq!(input.press(b'a' as u16), Some('a'));
        assert_eq!(input.press(0xde00), None);
        assert_eq!(input.press(0xd800), None);
        assert_eq!(input.press(0xd83d), None);
        assert_eq!(input.press(0xde00), Some('😀'));
        assert_eq!(input.press(0), None);
    }
    #[test]
    fn unicode_keymap_produces_exact_text_without_modifiers() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for character in ['a', 'A', '@', 'あ', '漢', '😀'] {
            let km = xkb::Keymap::new_from_string(
                &context,
                unicode_keymap(character),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            assert!(
                km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
                    .contains("xkb_keycodes \"niri_rdp_unicode\"")
            );
            let state = xkb::State::new(&km);
            assert_eq!(state.key_get_utf8(255u32.into()), character.to_string());
            assert_eq!(state.serialize_mods(xkb::STATE_MODS_EFFECTIVE), 0);
        }
        for (character, symbol) in [
            ('\t', "Tab"),
            ('\r', "Return"),
            ('\n', "Return"),
            ('\u{8}', "BackSpace"),
            ('\u{1b}', "Escape"),
            ('\u{7f}', "Delete"),
        ] {
            let km = xkb::Keymap::new_from_string(
                &context,
                unicode_keymap(character),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let state = xkb::State::new(&km);
            assert_eq!(
                xkb::keysym_get_name(state.key_get_one_sym(255u32.into())),
                symbol
            );
        }
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
    fn unrelated_output_removal_does_not_end_capture() {
        let mut state = State {
            selected_output: Some(10),
            ..State::default()
        };
        state.remove_output(11);
        assert!(!state.output_removed);
        state.remove_output(10);
        assert!(state.output_removed);
        assert!(state.validate_output().is_err());
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
