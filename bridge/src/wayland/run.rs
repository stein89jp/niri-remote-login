//! The capture worker: Wayland startup, the capture loop and input teardown.
//!
//! `CaptureLoop` owns everything the worker thread uses. Each loop iteration
//! runs its phases in order; a phase either proceeds, restarts the loop (for
//! example after a geometry barrier or input interrupted a capture), or stops
//! the worker because the desktop ended.
use super::capture::{
    CaptureBuffer, CapturePool, GpuBuffer, InputWait, ShmLayout, ShmTarget, damage_tiles,
    import_gpu_buffer,
};
use super::input::{Input, PhysicalKeymap, WheelAccumulator, follow_seat_keymap};
use super::output::{pump, pump_wake, resize_output, service_input};
use super::state::State;
use super::{
    CaptureMetadata, Command, Commands, CursorSnapshot, GpuCapture, GpuFrame, LatestFrames,
    OutputStatus,
};
use crate::TileSet;
use crate::keyboard::UnicodeInput;
use ironrdp_server::{DesktopSize, DisplayUpdate};
use std::{
    collections::BTreeSet,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle,
    protocol::{wl_output, wl_shm},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

pub(super) struct WorkerChannels {
    pub(super) commands: Arc<Commands>,
    pub(super) frames: LatestFrames,
    pub(super) ready: mpsc::SyncSender<anyhow::Result<()>>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) frame_rate: Arc<AtomicU32>,
    pub(super) gpu: Arc<GpuCapture>,
    pub(super) cursor_tx: tokio::sync::watch::Sender<CursorSnapshot>,
    pub(super) capture_metadata: CaptureMetadata,
    pub(super) status: Arc<OutputStatus>,
}

pub(super) fn run(
    socket: UnixStream,
    name: &str,
    target: DesktopSize,
    scale: f64,
    worker: WorkerChannels,
) -> anyhow::Result<()> {
    let mut worker = CaptureLoop::start(socket, name, target, scale, worker)?;
    let result = worker.capture();
    worker.finish();
    result
}

/// How a capture phase continues the loop.
enum Flow<T = ()> {
    /// Proceed to the next phase of this iteration with the phase's result.
    Next(T),
    /// Restart the capture loop from its first phase.
    Restart,
    /// The desktop stopped; the worker ends successfully.
    Stop,
}

/// Unwrap a phase's result, or return its restart/stop from this iteration.
macro_rules! proceed {
    ($phase:expr) => {
        match $phase? {
            Flow::Next(value) => value,
            Flow::Restart => return Ok(Flow::Restart),
            Flow::Stop => return Ok(Flow::Stop),
        }
    };
}

/// GPU capture pool state for one frame.
enum GpuPreparation {
    /// The free pool slot, or None while encoder tasks own every slot.
    Slot(Option<usize>),
    /// Input or a geometry barrier interrupted a buffer import.
    Interrupted,
}

struct CaptureLoop<'a> {
    name: &'a str,
    target: DesktopSize,
    scale: f64,
    c: Connection,
    q: EventQueue<State>,
    h: QueueHandle<State>,
    s: State,
    output: wl_output::WlOutput,
    capture: ZwlrScreencopyManagerV1,
    input: Input,
    clipboard: Option<crate::clipboard::ClipboardAttachment>,
    telemetry: crate::performance::Telemetry,
    commands: Arc<Commands>,
    frames: LatestFrames,
    ready: mpsc::SyncSender<anyhow::Result<()>>,
    alive: Arc<AtomicBool>,
    frame_rate: Arc<AtomicU32>,
    gpu: Arc<GpuCapture>,
    capture_metadata: CaptureMetadata,
    status: Arc<OutputStatus>,
    // Capture loop state.
    first: bool,
    shm: Option<CapturePool>,
    gpu_allocator: Option<crate::gpu_capture::Allocator>,
    gpu_buffers: Vec<GpuBuffer>,
    gpu_size: (u32, u32),
    gpu_sequence: u64,
    previous_gpu: bool,
    previous_separate_cursor: bool,
    pause_health_at: Instant,
    pause_health_pending: Option<Instant>,
    next_capture: Instant,
    latest: Option<Arc<DisplayUpdate>>,
    /// Whether the compositor's damage for the next copy is relative to the
    /// pixels of `latest`: every earlier copy was read, none was abandoned.
    damage_continuous: bool,
    /// Whether the current copy's damage report is relative to `latest`.
    damage_relative: bool,
    /// A full comparison periodically audits compositor damage.
    damage_audit_at: Instant,
}

impl<'a> CaptureLoop<'a> {
    /// Bind the compositor globals, apply the remote output policy and create
    /// the virtual input devices.
    fn start(
        socket: UnixStream,
        name: &'a str,
        target: DesktopSize,
        scale: f64,
        worker: WorkerChannels,
    ) -> anyhow::Result<Self> {
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
        follow_seat_keymap(&keyboard, &physical_keymap);
        let input = Input {
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
            wheel: WheelAccumulator::default(),
            started: Instant::now(),
        };
        let capture = s
            .capture
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing screencopy protocol"))?;
        let clipboard: Option<crate::clipboard::ClipboardAttachment> = None;
        let telemetry = crate::performance::Telemetry::from_env();
        Ok(Self {
            name,
            target,
            scale,
            c,
            q,
            h,
            s,
            output,
            capture,
            input,
            clipboard,
            telemetry,
            commands,
            frames,
            ready,
            alive,
            frame_rate,
            gpu,
            capture_metadata,
            status,
            first: true,
            shm: None,
            gpu_allocator: None,
            gpu_buffers: Vec::new(),
            gpu_size: (0, 0),
            gpu_sequence: 0,
            previous_gpu: false,
            previous_separate_cursor: false,
            pause_health_at: Instant::now(),
            pause_health_pending: None,
            next_capture: Instant::now(),
            latest: None,
            damage_continuous: false,
            damage_relative: false,
            damage_audit_at: Instant::now(),
        })
    }

    /// Capture until the desktop stops or an error ends the worker.
    fn capture(&mut self) -> anyhow::Result<()> {
        loop {
            if let Flow::Stop = self.capture_once()? {
                return Ok(());
            }
        }
    }

    fn capture_once(&mut self) -> anyhow::Result<Flow> {
        proceed!(self.poll_compositor());
        proceed!(self.drain_commands());
        self.update_cursor_and_refresh();
        proceed!(self.wait_while_suppressed());
        proceed!(self.wait_for_cadence());
        let frame_started = Instant::now();
        let frame = self.request_frame();
        let deadline = Instant::now();
        proceed!(self.negotiate(&frame, deadline));
        if self.s.remote_geometry_changed() {
            frame.destroy();
            return Ok(Flow::Restart);
        }
        let (format, w, height, stride) = self.s.dimensions.unwrap();
        let abgr = match format {
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => false,
            wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => true,
            _ => anyhow::bail!("unsupported SHM format"),
        };
        let layout = (format, w, height, stride);
        let gpu_slot = proceed!(self.select_gpu_slot(&frame, w, height));
        let (damage_wait, shm_target) = self.start_copy(&frame, layout, gpu_slot)?;
        proceed!(self.wait_ready(&frame, damage_wait, gpu_slot, deadline));
        if self.s.remote_geometry_changed() {
            frame.destroy();
            return Ok(Flow::Restart);
        }
        self.publish(
            &frame,
            (w, height, stride, abgr),
            (gpu_slot, shm_target),
            damage_wait,
            deadline,
            frame_started,
        )?;
        Ok(Flow::Next(()))
    }

    /// Dispatch compositor events and restore the remote output policy.
    fn poll_compositor(&mut self) -> anyhow::Result<Flow> {
        if !self.alive.load(Ordering::Acquire) {
            return Ok(Flow::Stop);
        }
        pump_wake(&self.c, &mut self.q, &mut self.s, 0, Some(&self.commands))?;
        self.input.update_keymap(&mut self.s)?;
        if self.s.remote_geometry_changed() {
            // Niri config reloads may overwrite runtime output settings.
            // Restore the authenticated remote policy before input or pixels.
            resize_output(
                &self.c,
                &mut self.q,
                &mut self.s,
                self.name,
                self.target,
                self.scale,
                &self.alive,
            )?;
            self.latest = None;
            self.next_capture = Instant::now();
            self.telemetry.count("capture_output_policy_restored", 1);
        }
        self.input.scale = self.s.observed_scale(self.name).unwrap_or(self.scale);
        Ok(Flow::Next(()))
    }

    /// Adjacent resizes were already collapsed in the bounded mailbox.
    /// Input behind a resize remains ordered against the committed mode.
    fn drain_commands(&mut self) -> anyhow::Result<Flow> {
        for _ in 0..256 {
            if !self.alive.load(Ordering::Acquire) {
                return Ok(Flow::Stop);
            }
            match self.commands.pop() {
                Some(Command::Resize(next, next_scale))
                    if next != self.target || next_scale != self.scale =>
                {
                    resize_output(
                        &self.c,
                        &mut self.q,
                        &mut self.s,
                        self.name,
                        next,
                        next_scale,
                        &self.alive,
                    )?;
                    self.target = next;
                    self.scale = next_scale;
                    let target = self.target;
                    self.input.target = target;
                    self.input.scale = self.s.observed_scale(self.name).unwrap_or(self.scale);
                    self.input.size = (u32::from(target.width), u32::from(target.height));
                    self.input.point = (
                        self.input.point.0.min(u32::from(target.width) - 1),
                        self.input.point.1.min(u32::from(target.height) - 1),
                    );
                    // A new geometry requires immediate complete pixels.
                    self.latest = None;
                    self.next_capture = Instant::now();
                }
                Some(Command::Resize(..)) => {}
                Some(Command::Status) => self.s.refresh_requested = true,
                Some(Command::Cursor(max)) => self.s.set_cursor_max(max),
                Some(Command::Key(event)) => self.input.key_event(event),
                Some(Command::Mouse(event)) => {
                    self.input.mouse_event(event);
                    self.s.note_injected(self.input.point);
                }
                Some(Command::Release) => self.input.release(),
                Some(Command::Clipboard(next)) => {
                    if next.attach_connection(self.c.clone()) {
                        self.clipboard = Some(next);
                    }
                }
                Some(Command::Stop) => return Ok(Flow::Stop),
                None => break,
            }
        }
        Ok(Flow::Next(()))
    }

    /// Open the separate cursor session and replay the latest desktop on a
    /// client refresh request.
    fn update_cursor_and_refresh(&mut self) {
        // Every event pump also does this; commands just drained may have
        // changed the client's pointer capability.
        self.s.service_cursor(&self.h);
        if self.previous_separate_cursor != self.s.cursor_snapshot.separate {
            self.latest = None;
            self.previous_separate_cursor = self.s.cursor_snapshot.separate;
            self.next_capture = Instant::now();
        }
        if self.s.refresh_requested {
            if self.latest.is_some() {
                self.frames.send_replace(self.latest.clone());
            }
            self.s.refresh_requested = false;
        }
    }

    /// While the client suppresses output, send heartbeats of the retained
    /// desktop and probe compositor health instead of capturing.
    fn wait_while_suppressed(&mut self) -> anyhow::Result<Flow> {
        if self.status.suppressed.load(Ordering::Acquire) && self.latest.is_some() {
            let now = Instant::now();
            if self.s.synced {
                self.pause_health_pending = None;
            }
            anyhow::ensure!(
                self.pause_health_pending
                    .is_none_or(|sent| sent.elapsed() < Duration::from_secs(3)),
                "suppressed compositor health check timed out"
            );
            if now >= self.pause_health_at {
                self.frames.send_replace(self.latest.clone());
                if self.pause_health_pending.is_none() {
                    self.s.synced = false;
                    self.c.display().sync(&self.h, ());
                    self.pause_health_pending = Some(now);
                }
                self.pause_health_at = now + Duration::from_secs(1);
                self.telemetry.count("capture_suppressed_heartbeats", 1);
                self.telemetry.report("capture", now);
            }
            pump_wake(
                &self.c,
                &mut self.q,
                &mut self.s,
                self.pause_health_at
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(1000) as i32,
                Some(&self.commands),
            )?;
            return Ok(Flow::Restart);
        }
        self.pause_health_at = Instant::now();
        self.pause_health_pending = None;
        Ok(Flow::Next(()))
    }

    /// Wait for the selected frame rate's next capture time.
    fn wait_for_cadence(&mut self) -> anyhow::Result<Flow> {
        let delay = self.next_capture.saturating_duration_since(Instant::now());
        if !delay.is_zero() {
            pump_wake(
                &self.c,
                &mut self.q,
                &mut self.s,
                delay.as_millis().min(i32::MAX as u128) as i32,
                Some(&self.commands),
            )?;
            return Ok(Flow::Restart);
        }
        Ok(Flow::Next(()))
    }

    fn request_frame(&mut self) -> ZwlrScreencopyFrameV1 {
        let s = &mut self.s;
        s.dimensions = None;
        s.gpu_format = None;
        s.buffer_done = false;
        s.ready = false;
        s.failed = false;
        s.inverted = false;
        s.damage.clear();
        s.full_damage = false;
        let frame = self.capture.capture_output(
            u32::from(!s.cursor_snapshot.separate) as i32,
            &self.output,
            &self.h,
            (),
        );
        s.active_frame = Some(frame.id());
        frame
    }

    /// Wait for the compositor's buffer parameters, servicing input meanwhile.
    fn negotiate(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        deadline: Instant,
    ) -> anyhow::Result<Flow> {
        while self.s.dimensions.is_none() || (self.capture.version() >= 3 && !self.s.buffer_done) {
            if !self.alive.load(Ordering::Acquire) {
                frame.destroy();
                return Ok(Flow::Stop);
            }
            if service_input(
                &self.commands,
                &mut self.input,
                &self.c,
                &mut self.clipboard,
                &mut self.s,
            ) {
                frame.destroy();
                return Ok(Flow::Restart);
            }
            pump_wake(
                &self.c,
                &mut self.q,
                &mut self.s,
                3000 - deadline.elapsed().as_millis().min(3000) as i32,
                Some(&self.commands),
            )?;
            anyhow::ensure!(
                !self.s.failed,
                "compositor rejected capture negotiation (buffer={:?}, buffer_done={})",
                self.s.dimensions,
                self.s.buffer_done,
            );
            anyhow::ensure!(
                deadline.elapsed() < Duration::from_secs(3),
                "capture negotiation timed out after {:?} (buffer={:?}, buffer_done={})",
                deadline.elapsed(),
                self.s.dimensions,
                self.s.buffer_done,
            );
        }
        self.telemetry
            .sample("capture_negotiation", deadline.elapsed());
        Ok(Flow::Next(()))
    }

    /// Choose a free GPU pool slot when GPU capture is requested. Failures
    /// fall back to SHM capture; a busy pool retries shortly.
    fn select_gpu_slot(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        w: u32,
        height: u32,
    ) -> anyhow::Result<Flow<Option<usize>>> {
        let wants_gpu =
            self.gpu.requested.load(Ordering::Acquire) && !self.gpu.failed.load(Ordering::Acquire);
        if self.previous_gpu != wants_gpu {
            self.latest = None;
            self.previous_gpu = wants_gpu;
            if !wants_gpu {
                self.gpu.latest.lock().unwrap().take();
            }
        }
        if !wants_gpu {
            return Ok(Flow::Next(None));
        }
        match self.prepare_gpu(w, height) {
            Ok(GpuPreparation::Interrupted) => {
                frame.destroy();
                self.next_capture = Instant::now();
                Ok(Flow::Restart)
            }
            Err(error) => {
                eprintln!("GPU capture unavailable; using SHM: {error}");
                self.gpu.failed.store(true, Ordering::Release);
                self.gpu.requested.store(false, Ordering::Release);
                self.gpu.latest.lock().unwrap().take();
                self.previous_gpu = false;
                self.latest = None;
                Ok(Flow::Next(None))
            }
            Ok(GpuPreparation::Slot(None)) => {
                // Active/canceled encoder tasks own every pool slot. Keep
                // work bounded and never overwrite an encoder's DMA-BUF.
                frame.destroy();
                self.next_capture = Instant::now() + Duration::from_millis(5);
                Ok(Flow::Restart)
            }
            Ok(GpuPreparation::Slot(slot)) => Ok(Flow::Next(slot)),
        }
    }

    fn prepare_gpu(&mut self, w: u32, height: u32) -> anyhow::Result<GpuPreparation> {
        anyhow::ensure!(
            w == u32::from(self.target.width) && height == u32::from(self.target.height),
            "GPU capture geometry mismatch"
        );
        anyhow::ensure!(
            self.s.gpu_format == Some((0x34325258, w, height)),
            "GPU screencopy format unavailable"
        );
        anyhow::ensure!(
            !self.s.dmabuf_xrgb_modifiers.is_empty(),
            "XRGB capture modifiers unavailable"
        );
        if self.gpu_allocator.is_none() {
            self.gpu_allocator = Some(crate::gpu_capture::Allocator::new()?);
        }
        if self.gpu_size != (w, height) {
            self.gpu_buffers.clear();
            self.gpu.latest.lock().unwrap().take();
            self.gpu_size = (w, height);
        }
        if self.gpu_buffers.len() < 3 {
            let owner = self.gpu_allocator.as_ref().unwrap().allocate(
                w,
                height,
                &self
                    .s
                    .dmabuf_xrgb_modifiers
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
            )?;
            if let Some(buffer) = import_gpu_buffer(
                &self.c,
                &mut self.q,
                &mut self.s,
                owner,
                (w, height),
                InputWait {
                    commands: &self.commands,
                    input: &mut self.input,
                    clipboard: &mut self.clipboard,
                    alive: &self.alive,
                },
            )? {
                self.gpu_buffers.push(buffer);
            } else {
                return Ok(GpuPreparation::Interrupted);
            }
        }
        Ok(GpuPreparation::Slot(
            self.gpu_buffers
                .iter()
                .position(|buffer| Arc::strong_count(&buffer.owner) == 1),
        ))
    }

    /// Submit a free SHM slot or the GPU buffer. Returns whether the copy waits
    /// for damage, and the SHM slot.
    fn start_copy(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        layout: ShmLayout,
        gpu_slot: Option<usize>,
    ) -> anyhow::Result<(bool, Option<ShmTarget>)> {
        let shm_target = if gpu_slot.is_none() {
            if self.shm.as_ref().is_none_or(|pool| pool.layout != layout) {
                self.shm = Some(CapturePool::new(layout));
            }
            let shm = self
                .s
                .shm
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing SHM"))?;
            let h = &self.h;
            let pool = self.shm.as_mut().unwrap();
            let target = pool.acquire(|| CaptureBuffer::new(shm, layout, h))?;
            if target == ShmTarget::Scratch {
                self.telemetry.count("capture_pool_exhausted", 1);
            }
            Some(target)
        } else {
            None
        };
        let capture_buffer = match (gpu_slot, shm_target) {
            (Some(slot), _) => &self.gpu_buffers[slot].buffer,
            (None, Some(target)) => self.shm.as_ref().unwrap().wl_buffer(target).unwrap(),
            (None, None) => unreachable!("SHM capture always selects a target"),
        };
        let damage_wait = self.latest.is_some() && frame.version() >= 2;
        if damage_wait {
            frame.copy_with_damage(capture_buffer);
        } else {
            frame.copy(capture_buffer);
        }
        // Until this copy is read, damage continuity is unknown: an abandoned
        // copy may still have advanced the compositor's damage tracker.
        self.damage_relative = damage_wait && self.damage_continuous;
        self.damage_continuous = false;
        let (_, w, height, _) = layout;
        self.input.size = (w, height);
        Ok((damage_wait, shm_target))
    }

    /// CopyWithDamage intentionally waits forever on a quiet desktop.
    /// Lightweight display.sync health probes distinguish idle from a
    /// dead compositor without periodic readback. Heartbeats reuse Bytes.
    fn wait_ready(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        damage_wait: bool,
        gpu_slot: Option<usize>,
        deadline: Instant,
    ) -> anyhow::Result<Flow> {
        let mut health_at = Instant::now() + Duration::from_secs(1);
        let mut health_pending: Option<Instant> = None;
        while !self.s.ready {
            if self.status.suppressed.load(Ordering::Acquire) && self.latest.is_some() {
                frame.destroy();
                return Ok(Flow::Restart);
            }

            if self.previous_gpu != self.gpu.requested.load(Ordering::Acquire)
                || self.previous_separate_cursor != self.s.cursor_snapshot.separate
            {
                frame.destroy();
                self.next_capture = Instant::now();
                return Ok(Flow::Restart);
            }
            if !self.alive.load(Ordering::Acquire) {
                frame.destroy();
                return Ok(Flow::Stop);
            }
            if service_input(
                &self.commands,
                &mut self.input,
                &self.c,
                &mut self.clipboard,
                &mut self.s,
            ) {
                frame.destroy();
                return Ok(Flow::Restart);
            }
            if self.s.refresh_requested {
                self.frames.send_replace(self.latest.clone());
                self.s.refresh_requested = false;
            }
            let now = Instant::now();
            if damage_wait && now >= health_at {
                if let Some(image) = &self.latest {
                    self.frames.send_replace(Some(image.clone()));
                }
                if health_pending.is_none() {
                    self.s.synced = false;
                    self.c.display().sync(&self.h, ());
                    health_pending = Some(now);
                }
                self.telemetry.count("capture_idle_heartbeats", 1);
                self.telemetry.report("capture", now);
                health_at = now + Duration::from_secs(1);
            }
            if self.s.synced {
                health_pending = None;
            }
            if self.s.failed && gpu_slot.is_some() {
                self.gpu.failed.store(true, Ordering::Release);
                self.gpu.requested.store(false, Ordering::Release);
                frame.destroy();
                self.next_capture = Instant::now();
                return Ok(Flow::Restart);
            }
            anyhow::ensure!(!self.s.failed, "capture failed");
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
            pump_wake(
                &self.c,
                &mut self.q,
                &mut self.s,
                timeout,
                Some(&self.commands),
            )?;
            self.input.update_keymap(&mut self.s)?;
        }
        Ok(Flow::Next(()))
    }

    /// Snapshot the ready buffer, publish changed desktops and schedule the
    /// next capture.
    fn publish(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        (w, height, stride, abgr): (u32, u32, u32, bool),
        (gpu_slot, shm_target): (Option<usize>, Option<ShmTarget>),
        damage_wait: bool,
        deadline: Instant,
        frame_started: Instant,
    ) -> anyhow::Result<()> {
        self.telemetry.sample(
            if damage_wait {
                "capture_damage_wait_and_readback"
            } else {
                "capture_wait_readback"
            },
            deadline.elapsed(),
        );
        self.telemetry
            .count("capture_damage_rectangles", self.s.damage.len() as u64);
        self.telemetry
            .count("capture_damage_overflow", u64::from(self.s.full_damage));
        let snapshot_started = Instant::now();
        let target = self.target;
        // Full snapshots retain every change even when watch notifications
        // are overwritten. Partial bitmap diffs compare each receiver's last
        // delivered full desktop, never only the latest compositor damage.
        let (image, owner) = if let Some(slot) = gpu_slot {
            self.gpu_sequence = self.gpu_sequence.wrapping_add(1);
            let owner = self.gpu_buffers[slot].owner.clone();
            let ticket = Arc::new(GpuFrame {
                fd: owner.fd()?,
                stride: owner.stride,
                fourcc: 0x34325258,
                modifier: owner.modifier,
                width: w,
                height,
                offset: owner.offset,
                device: owner.device(),
                sequence: self.gpu_sequence,
                inverted: self.s.inverted,
                captured_at: snapshot_started,
                _owner: owner,
            });
            *self.gpu.latest.lock().unwrap() = Some(ticket);
            let mut token = b"NIRIGPU1".to_vec();
            token.extend_from_slice(&self.gpu_sequence.to_le_bytes());
            let image = ironrdp_server::BitmapUpdate {
                x: u16::MAX,
                y: u16::MAX,
                width: std::num::NonZeroU16::new(target.width).unwrap(),
                height: std::num::NonZeroU16::new(target.height).unwrap(),
                format: ironrdp_server::PixelFormat::BgrX32,
                data: token.into(),
                stride: std::num::NonZeroUsize::new(usize::from(target.width) * 4).unwrap(),
            };
            (image, None)
        } else {
            let pool = self.shm.as_ref().unwrap();
            pool.snapshot(
                shm_target.unwrap(),
                (w, height, stride, self.s.inverted),
                target,
                abgr,
            )?
        };
        let (changed, changes) = self.compare_with_latest(&image, gpu_slot.is_some(), (w, height));
        // This copy was read: the next damage report is relative to it, and
        // an unchanged copy has the pixels of `latest`.
        self.damage_continuous = true;
        self.telemetry
            .sample("capture_snapshot", snapshot_started.elapsed());
        self.telemetry.count(
            if gpu_slot.is_some() {
                "gpu_capture_stride_bytes"
            } else {
                "capture_bytes"
            },
            u64::from(gpu_slot.map_or(stride, |slot| self.gpu_buffers[slot].owner.stride))
                * u64::from(height),
        );
        self.telemetry.count(
            if changed {
                "capture_frames"
            } else {
                "capture_suppressed"
            },
            1,
        );
        self.telemetry.report("capture", Instant::now());
        if changed {
            self.capture_metadata
                .lock()
                .unwrap()
                .get_or_insert_with(Default::default)
                .push(&image, snapshot_started, changes, owner);
            self.latest = Some(Arc::new(DisplayUpdate::Bitmap(image)));
            self.frames.send_replace(self.latest.clone());
        }
        frame.destroy();
        if self.first {
            let _ = self.ready.send(Ok(()));
            self.first = false;
        }
        let fps = self.frame_rate.load(Ordering::Acquire).clamp(1, 60);
        self.next_capture = frame_started + Duration::from_secs_f64(1.0 / f64::from(fps));
        Ok(())
    }

    /// Whether a snapshot differs from `latest`, and exactly which tiles do.
    /// Compositor damage limits the comparison when it is relative to
    /// `latest` and covers the captured pixels one to one; a periodic full
    /// comparison audits it. Consumers reuse the recorded tiles instead of
    /// comparing complete desktops on the RDP runtime thread.
    fn compare_with_latest(
        &mut self,
        image: &ironrdp_server::BitmapUpdate,
        gpu: bool,
        (w, height): (u32, u32),
    ) -> (bool, Option<TileSet>) {
        let previous = self.latest.as_deref().and_then(|latest| match latest {
            DisplayUpdate::Bitmap(previous) if crate::comparable(previous, image) => Some(previous),
            _ => None,
        });
        let Some(previous) = previous else {
            return (true, None);
        };
        if gpu {
            // GPU tickets carry a sequence, never pixels.
            return (!crate::same_pixels(previous, image, None), None);
        }
        let hint = (self.damage_relative
            && !self.s.full_damage
            && !self.s.inverted
            && (w, height) == (u32::from(image.width.get()), u32::from(image.height.get())))
            .then(|| damage_tiles(image, &self.s.damage))
            .flatten();
        let now = Instant::now();
        let audit = hint.is_some() && now >= self.damage_audit_at;
        let tiles = if audit {
            self.damage_audit_at = now + Duration::from_secs(1);
            crate::changed_tiles(previous, image, None)
        } else {
            crate::changed_tiles(previous, image, hint.as_ref())
        };
        self.telemetry.count(
            if hint.is_some() && !audit {
                "capture_damage_limited_compares"
            } else {
                "capture_full_compares"
            },
            1,
        );
        self.telemetry
            .count("capture_changed_tiles", tiles.len() as u64);
        if audit && hint.as_ref().is_some_and(|hint| !tiles.is_subset(hint)) {
            // Changed pixels outside the reported damage: an earlier limited
            // comparison may have missed changes too, and a diff base may
            // already contain one the client never received. Unknown history
            // forces full comparisons; a refresh epoch resends full desktops.
            self.telemetry.count("capture_damage_missed", 1);
            self.status.refresh_epoch.fetch_add(1, Ordering::AcqRel);
            return (true, None);
        }
        (!tiles.is_empty(), Some(tiles))
    }

    /// Release capture buffers, clipboard and every held input, then wait for
    /// Niri to process them before the worker closes its socket.
    fn finish(&mut self) {
        // Capture buffers are released first, in the order the capture loop
        // previously dropped them, so their destroy requests precede the sync.
        self.gpu_buffers.clear();
        self.gpu_allocator = None;
        self.shm = None;
        if let Some(clipboard) = self.clipboard.take() {
            clipboard.clear();
        }
        self.input.release();
        self.input.keyboard.destroy();
        self.input.text_keyboard.destroy();
        self.input.pointer.destroy();
        // A successful flush only hands bytes to the socket. Await wl_display.sync
        // so Niri processes releases/destruction before this client socket closes;
        // otherwise a readable+hangup wake may discard final injected releases.
        self.s.synced = false;
        self.c.display().sync(&self.h, ());
        let cleanup_deadline = Instant::now();
        while !self.s.synced && cleanup_deadline.elapsed() < Duration::from_millis(500) {
            if pump(&self.c, &mut self.q, &mut self.s).is_err() {
                break;
            }
        }
    }
}
