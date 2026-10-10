//! One worker owns the Wayland queue and its bounded SHM buffers.
use crate::{bitmap, buffer_length, evdev_key};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult,
};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_buffer, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool},
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

enum Command {
    Resize(DesktopSize),
    Key(KeyboardEvent),
    Mouse(MouseEvent),
    Release,
    Stop,
}
#[derive(Clone)]
pub struct Desktop {
    inner: Arc<Inner>,
}
struct Inner {
    commands: mpsc::SyncSender<Command>,
    frames: Mutex<tokio::sync::watch::Receiver<Option<Arc<DisplayUpdate>>>>,
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
        let (tx, rx) = mpsc::sync_channel(256);
        let (frames, frame_rx) = tokio::sync::watch::channel(None);
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
                rx,
                frames,
                ready.clone(),
                worker_alive.clone(),
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
    pub fn updates_send(&self) -> Box<dyn RdpServerDisplayUpdates + Send> {
        Box::new(Updates {
            desktop: self.clone(),
            rx: self.inner.frames.lock().unwrap().clone(),
            size: *self.inner.size.lock().unwrap(),
            pending: None,
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
}
impl Updates {
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
        if self.rx.changed().await.is_err() {
            return Ok(None);
        }
        let update = self
            .rx
            .borrow_and_update()
            .as_ref()
            .map(|f| f.as_ref().clone());
        Ok(update.map(|update| self.prepare(update)))
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
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(size) = layout_size(&layout) {
            self.send(Command::Resize(size));
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfigurationResult {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Default)]
struct State {
    output_manager: Option<ZwlrOutputManagerV1>,
    heads: Vec<(ZwlrOutputHeadV1, String, bool)>,
    output_serial: Option<u32>,
    configuration_result: Option<ConfigurationResult>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    capture: Option<ZwlrScreencopyManagerV1>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    synced: bool,
    outputs: Vec<(u32, wl_output::WlOutput, String, i32, bool, (u32, u32))>,
    selected_output: Option<u32>,
    output_removed: bool,
    dimensions: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    ready: bool,
    failed: bool,
    inverted: bool,
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
                output.3 == 1 && output.4,
                "bridge requires scale=1 normal transform"
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
        _: &ZwlrScreencopyFrameV1,
        e: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
            zwlr_screencopy_frame_v1::Event::BufferDone => s.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => s.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => s.failed = true,
            _ => {}
        }
    }
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
                s.heads.push((head, String::new(), false))
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
    alive: &AtomicBool,
) -> anyhow::Result<()> {
    let expected = (u32::from(target.width), u32::from(target.height));
    if s.outputs.iter().any(|o| o.2 == name && o.5 == expected) {
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
        for (head, head_name, enabled) in &s.heads {
            if head_name == name {
                let conf = config.enable_head(head, &h, ());
                conf.set_custom_mode(target.width.into(), target.height.into(), 60_000);
                conf.set_scale(1.0);
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
                        if s.outputs.iter().any(|o| o.2 == name && o.5 == expected) =>
                    {
                        return Ok(true);
                    }
                    _ => {}
                }
            }
        })();
        config.destroy();
        if result? {
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
    q.dispatch_pending(s)?;
    c.flush()?;
    if let Some(guard) = q.prepare_read() {
        let mut fd = libc::pollfd {
            fd: c.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a single initialized pollfd is valid for this synchronous call.
        let n = unsafe { libc::poll(&mut fd, 1, 50) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
        if n > 0 {
            guard.read()?;
        } else {
            drop(guard);
        }
    }
    q.dispatch_pending(s)?;
    s.validate_output()?;
    Ok(())
}

struct Input {
    keyboard: ZwpVirtualKeyboardV1,
    pointer: ZwlrVirtualPointerV1,
    xkb: xkb::State,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    point: (u32, u32),
    size: (u32, u32),
    target: DesktopSize,
    started: Instant,
}
impl Input {
    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
    fn modifiers(&self) {
        self.keyboard.modifiers(
            self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
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
            self.xkb.update_key(
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
                let km = self.xkb.get_keymap();
                let mut locked = 0;
                // RDP synchronize bits: Scroll=1, Num=2, Caps=4, Kana=8.
                for (bit, name) in [(1, "ScrollLock"), (2, "NumLock"), (4, "Lock")] {
                    let i = km.mod_get_index(name);
                    if flags.bits() & bit != 0 && i < 32 {
                        locked |= 1 << i;
                    }
                }
                self.xkb.update_mask(
                    self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                    0,
                    locked,
                    0,
                    0,
                    0,
                );
                self.modifiers();
            }
            // Unicode has no hardware-key identity. Reject it rather than mapping
            // UTF-16 values to unrelated physical keys; IME support remains pending.
            KeyboardEvent::UnicodePressed(_) | KeyboardEvent::UnicodeReleased(_) => {}
        }
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
        self.move_to(
            (i64::from(self.point.0) + i64::from(x)).clamp(0, i64::from(self.target.width) - 1)
                as u32,
            (i64::from(self.point.1) + i64::from(y)).clamp(0, i64::from(self.target.height) - 1)
                as u32,
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

fn run(
    socket: UnixStream,
    name: &str,
    mut target: DesktopSize,
    commands: mpsc::Receiver<Command>,
    frames: tokio::sync::watch::Sender<Option<Arc<DisplayUpdate>>>,
    ready: mpsc::SyncSender<anyhow::Result<()>>,
    alive: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let c = Connection::from_socket(socket)?;
    let mut q = c.new_event_queue();
    let h = q.handle();
    let mut s = State::default();
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
        output.3 == 1 && output.4,
        "bridge requires scale=1 normal transform"
    );
    s.selected_output = Some(output.0);
    let output = output.1.clone();
    resize_output(&c, &mut q, &mut s, name, target, &alive)?;
    let seat = s
        .seat
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing Wayland seat"))?;
    let keyboard = s
        .keyboard_manager
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing virtual keyboard protocol"))?
        .create_virtual_keyboard(seat, &h, ());
    let pm = s
        .pointer_manager
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing virtual pointer protocol"))?;
    anyhow::ensure!(
        pm.version() >= 2,
        "virtual pointer output mapping requires version 2"
    );
    let pointer = pm.create_virtual_pointer_with_output(Some(seat), Some(&output), &h, ());
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let km = xkb::Keymap::new_from_names(
        &ctx,
        "evdev",
        "pc105",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow::anyhow!("XKB US keymap unavailable"))?;
    let mut keymap = tempfile::tempfile()?;
    let mut text = km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    text.push('\0');
    keymap.write_all(text.as_bytes())?;
    keyboard.keymap(1, keymap.as_fd(), text.len() as u32);
    let mut input = Input {
        keyboard,
        pointer,
        xkb: xkb::State::new(&km),
        keys: BTreeSet::new(),
        buttons: BTreeSet::new(),
        point: (0, 0),
        size: (1, 1),
        target,
        started: Instant::now(),
    };
    let capture = s
        .capture
        .clone()
        .ok_or_else(|| anyhow::anyhow!("missing screencopy protocol"))?;
    let result = (|| -> anyhow::Result<()> {
        let mut first = true;
        loop {
            if !alive.load(Ordering::Acquire) {
                return Ok(());
            }
            // Apply resize only between captures, preserving the coordinate
            // contract for every queued input event and in-flight SHM buffer.
            for _ in 0..256 {
                if !alive.load(Ordering::Acquire) {
                    return Ok(());
                }
                match commands.try_recv() {
                    Ok(Command::Resize(next)) if next != target => {
                        resize_output(&c, &mut q, &mut s, name, next, &alive)?;
                        target = next;
                        input.target = target;
                        input.size = (u32::from(target.width), u32::from(target.height));
                        input.point = (
                            input.point.0.min(u32::from(target.width) - 1),
                            input.point.1.min(u32::from(target.height) - 1),
                        );
                    }
                    Ok(Command::Resize(_)) => {}
                    Ok(Command::Key(e)) => input.key_event(e),
                    Ok(Command::Mouse(e)) => input.mouse_event(e),
                    Ok(Command::Release) => input.release(),
                    Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            s.dimensions = None;
            s.buffer_done = false;
            s.ready = false;
            s.failed = false;
            s.inverted = false;
            let frame = capture.capture_output(1, &output, &h, ());
            let deadline = Instant::now();
            while s.dimensions.is_none() || (capture.version() >= 3 && !s.buffer_done) {
                if !alive.load(Ordering::Acquire) {
                    return Ok(());
                }
                pump(&c, &mut q, &mut s)?;
                anyhow::ensure!(
                    !s.failed && deadline.elapsed() < Duration::from_secs(3),
                    "capture negotiation failed or timed out"
                );
            }
            let (format, w, height, stride) = s.dimensions.unwrap();
            let abgr = match format {
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => false,
                wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => true,
                _ => anyhow::bail!("unsupported SHM format"),
            };
            let len = buffer_length(w, height, stride)?;
            let mut file: File = tempfile::tempfile()?;
            file.set_len(len as u64)?;
            let pool = s
                .shm
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing SHM"))?
                .create_pool(file.as_fd(), len as i32, &h, ());
            let buffer =
                pool.create_buffer(0, w as i32, height as i32, stride as i32, format, &h, ());
            frame.copy(&buffer);
            input.size = (w, height);
            while !s.ready {
                if !alive.load(Ordering::Acquire) {
                    return Ok(());
                }
                pump(&c, &mut q, &mut s)?;
                anyhow::ensure!(
                    !s.failed && deadline.elapsed() < Duration::from_secs(3),
                    "capture failed or timed out"
                );
            }
            let mut bytes = vec![0; len];
            file.seek(SeekFrom::Start(0))?;
            file.read_exact(&mut bytes)?;
            let image = bitmap(&bytes, w, height, stride, s.inverted, target, abgr)?;
            frames.send_replace(Some(Arc::new(DisplayUpdate::Bitmap(image))));
            frame.destroy();
            buffer.destroy();
            pool.destroy();
            if first {
                let _ = ready.send(Ok(()));
                first = false;
            }
            // At most 30 full frames per second, retaining only the newest one.
            std::thread::sleep(Duration::from_millis(33));
        }
    })();
    input.release();
    input.keyboard.destroy();
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
mod lifecycle_tests {
    use super::*;
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
        let (commands, _worker) = mpsc::sync_channel(256);
        let (frames, rx) = tokio::sync::watch::channel(None);
        let desktop = Desktop {
            inner: Arc::new(Inner {
                commands,
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
            let (commands, _worker) = mpsc::sync_channel(256);
            let (frames, rx) = tokio::sync::watch::channel(None);
            let desktop = Desktop {
                inner: Arc::new(Inner {
                    commands,
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
        let (commands, _worker) = mpsc::sync_channel(256);
        let (_frames, rx) = tokio::sync::watch::channel(None);
        let desktop = Desktop {
            inner: Arc::new(Inner {
                commands,
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
