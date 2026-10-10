//! One worker owns the Wayland queue and its bounded SHM buffers.
use crate::{bitmap, buffer_length, evdev_key};
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
    size: DesktopSize,
    failed: Arc<Mutex<Option<String>>>,
    alive: Arc<AtomicBool>,
    lease: Arc<Mutex<Option<UnixStream>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
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
                size: target,
                failed,
                alive,
                lease,
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
        self.inner.lease.lock().unwrap().take();
        let _ = self.inner.commands.try_send(Command::Stop);
    }
    pub fn release_all(&self) {
        self.send(Command::Release);
    }
    pub fn alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
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
        if self.rx.changed().await.is_err() {
            return Ok(None);
        }
        Ok(self
            .rx
            .borrow_and_update()
            .as_ref()
            .map(|f| f.as_ref().clone()))
    }
}
#[async_trait::async_trait]
impl RdpServerDisplay for Desktop {
    async fn size(&mut self) -> DesktopSize {
        self.inner.size
    }
    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(Updates {
            desktop: self.clone(),
            rx: self.inner.frames.lock().unwrap().clone(),
        }))
    }
}

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    capture: Option<ZwlrScreencopyManagerV1>,
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    synced: bool,
    outputs: Vec<(u32, wl_output::WlOutput, String, i32, bool)>,
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
                )),
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
            wl_registry::Event::GlobalRemove { name } if s.outputs.iter().any(|o| o.0 == name) => {
                s.output_removed = true;
            }
            _ => {}
        }
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
    anyhow::ensure!(!s.output_removed, "Wayland output removed");
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
    target: DesktopSize,
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
    let output = output.1.clone();
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
                for _ in 0..256 {
                    match commands.try_recv() {
                        Ok(Command::Key(e)) => input.key_event(e),
                        Ok(Command::Mouse(e)) => input.mouse_event(e),
                        Ok(Command::Release) => input.release(),
                        Ok(Command::Stop) | Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                        Err(mpsc::TryRecvError::Empty) => break,
                    }
                }
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
    #[tokio::test(flavor = "current_thread")]
    async fn dead_or_closed_capture_is_terminal_not_a_retry_error() {
        for alive in [false, true] {
            let (commands, _worker) = mpsc::sync_channel(256);
            let (frames, rx) = tokio::sync::watch::channel(None);
            let desktop = Desktop {
                inner: Arc::new(Inner {
                    commands,
                    frames: Mutex::new(rx.clone()),
                    size: DesktopSize {
                        width: 640,
                        height: 480,
                    },
                    failed: Arc::new(Mutex::new(Some("broken compositor".into()))),
                    alive: Arc::new(AtomicBool::new(alive)),
                    lease: Arc::new(Mutex::new(None)),
                }),
            };
            drop(frames);
            let mut updates = Updates { desktop, rx };
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
    fn stop_closes_lease_even_when_handlers_retain_desktop_clones() {
        let (lease, mut broker) = UnixStream::pair().unwrap();
        broker
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let (commands, _worker) = mpsc::sync_channel(256);
        let (_frames, rx) = tokio::sync::watch::channel(None);
        let desktop = Desktop {
            inner: Arc::new(Inner {
                commands,
                frames: Mutex::new(rx),
                size: DesktopSize {
                    width: 640,
                    height: 480,
                },
                failed: Arc::new(Mutex::new(None)),
                alive: Arc::new(AtomicBool::new(true)),
                lease: Arc::new(Mutex::new(Some(lease))),
            }),
        };
        let retained = desktop.clone();
        desktop.stop();
        let mut byte = [0; 1];
        assert_eq!(broker.read(&mut byte).unwrap(), 0);
        assert!(!retained.alive());
        assert!(retained.inner.lease.lock().unwrap().is_none());
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
