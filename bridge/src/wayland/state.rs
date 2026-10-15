//! Worker-owned Wayland state and protocol event dispatch, including the
//! separate cursor channel.
// Shapes published here carry cache index 0. The per-activation display
// stream (`Updates` in wayland/mod.rs) assigns real indices through this
// cache, because only it knows which shapes reached the client.
pub(super) mod cursor_cache;
// ext-image-copy-capture cursor sessions feed the separate cursor.
mod cursor_capture;
use super::input::read_keymap;
use super::{CursorSnapshot, OutputRecord};
use cursor_capture::CursorCapture;
use ironrdp_server::DesktopSize;
use std::{collections::BTreeSet, fs::File, sync::Arc, time::Instant};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfigurationResult {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) output_manager: Option<ZwlrOutputManagerV1>,
    pub(super) heads: Vec<(ZwlrOutputHeadV1, String, bool, Option<f64>)>,
    pub(super) output_serial: Option<u32>,
    pub(super) applied_scale: Option<f64>,
    pub(super) applied_size: Option<DesktopSize>,
    pub(super) configuration_result: Option<ConfigurationResult>,
    pub(super) shm: Option<wl_shm::WlShm>,
    pub(super) seat: Option<wl_seat::WlSeat>,
    pub(super) capture: Option<ZwlrScreencopyManagerV1>,
    pub(super) pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    pub(super) keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    pub(super) seat_keyboard: Option<wl_keyboard::WlKeyboard>,
    pub(super) seat_keymap: Option<String>,
    pub(super) keymap_error: Option<String>,
    pub(super) synced: bool,
    pub(super) outputs: Vec<OutputRecord>,
    pub(super) selected_output: Option<u32>,
    pub(super) output_removed: bool,
    pub(super) refresh_requested: bool,
    pub(super) output_sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    pub(super) image_copy: Option<ExtImageCopyCaptureManagerV1>,
    pub(super) cursor: Option<CursorCapture>,
    /// When a stopped cursor session may be created again.
    pub(super) cursor_retry_at: Option<Instant>,
    pub(super) cursor_unavailable_logged: bool,
    pub(super) cursor_max: u16,
    pub(super) cursor_tx: Option<tokio::sync::watch::Sender<CursorSnapshot>>,
    pub(super) cursor_snapshot: CursorSnapshot,
    /// The snapshot last sent to the RDP streams.
    pub(super) cursor_published: CursorSnapshot,
    pub(super) dmabuf: Option<ZwpLinuxDmabufV1>,
    pub(super) dmabuf_xrgb_modifiers: BTreeSet<u64>,
    pub(super) dmabuf_params: Option<wayland_client::backend::ObjectId>,
    pub(super) dmabuf_created: Option<wl_buffer::WlBuffer>,
    pub(super) dmabuf_failed: bool,
    pub(super) gpu_format: Option<(u32, u32, u32)>,
    pub(super) active_frame: Option<wayland_client::backend::ObjectId>,
    pub(super) dimensions: Option<(wl_shm::Format, u32, u32, u32)>,
    pub(super) buffer_done: bool,
    pub(super) ready: bool,
    pub(super) failed: bool,
    pub(super) inverted: bool,
    pub(super) damage: Vec<(u32, u32, u32, u32)>,
    pub(super) full_damage: bool,
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
                "ext_output_image_capture_source_manager_v1" => {
                    s.output_sources = Some(registry.bind(name, 1, q, ()));
                }
                "ext_image_copy_capture_manager_v1" => {
                    s.image_copy = Some(registry.bind(name, 1, q, ()));
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
    pub(super) fn validate_output(&self) -> anyhow::Result<()> {
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
impl State {
    pub(super) fn observed_scale(&self, name: &str) -> Option<f64> {
        self.heads.iter().find(|head| head.1 == name)?.3
    }
    pub(super) fn output_policy_matches(
        &self,
        name: &str,
        target: DesktopSize,
        scale: f64,
    ) -> bool {
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
    pub(super) fn remote_geometry_changed(&self) -> bool {
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
        self.cursor_published = self.cursor_snapshot.clone();
        if let Some(tx) = &self.cursor_tx {
            tx.send_replace(self.cursor_snapshot.clone());
        }
    }
    /// Whether the snapshot differs from the published one in anything the
    /// RDP streams act on. A baseline position alone is not sent: streams
    /// read positions only with a new position generation (a warp).
    fn cursor_changed(&self) -> bool {
        let (next, sent) = (&self.cursor_snapshot, &self.cursor_published);
        (next.separate, next.visible, next.position_generation)
            != (sent.separate, sent.visible, sent.position_generation)
            || match (&next.shape, &sent.shape) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (a, b) => a.is_some() != b.is_some(),
            }
    }
    pub(super) fn set_cursor_max(&mut self, max_size: u16) {
        self.cursor_max = max_size;
        // Reactivation invalidates the client's pointer cache. Start embedded
        // and open a new cursor session so a fresh shape precedes separation.
        self.cursor = None;
        self.cursor_retry_at = None;
        self.cursor_snapshot.separate = false;
        self.cursor_snapshot.visible = false;
        self.publish_cursor();
    }
    /// Remember where the bridge's own virtual pointer put the cursor, so the
    /// position reported when the cursor appears again is not a warp.
    pub(super) fn note_injected(&mut self, point: (u32, u32)) {
        if let Some(capture) = &mut self.cursor {
            capture.tracker.injected(point);
        }
    }
    /// Run after every dispatched batch of events: create the cursor session
    /// when wanted (with a backoff after the compositor stopped one), re-arm a
    /// failed frame, and publish the snapshot once if it changed (R3).
    pub(super) fn service_cursor(&mut self, h: &QueueHandle<State>) {
        let now = Instant::now();
        if self.cursor_max != 0
            && self.cursor.is_none()
            && self.cursor_retry_at.is_none_or(|at| now >= at)
            && let Some(output) = self
                .selected_output
                .and_then(|name| self.outputs.iter().find(|output| output.0 == name))
                .map(|output| output.1.clone())
        {
            self.cursor_retry_at = None;
            match (&self.output_sources, &self.image_copy, &self.seat) {
                (Some(sources), Some(image_copy), Some(seat)) => {
                    self.cursor = Some(CursorCapture::new(sources, image_copy, seat, &output, h));
                }
                _ if !self.cursor_unavailable_logged => {
                    self.cursor_unavailable_logged = true;
                    eprintln!(
                        "compositor offers no ext-image-copy-capture cursor sessions; \
                         drawing the cursor into the video"
                    );
                }
                _ => {}
            }
        }
        if let Some(capture) = &mut self.cursor {
            capture.retry(now, h);
        }
        if self.cursor_changed() {
            self.publish_cursor();
        }
    }
}
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
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
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
}
