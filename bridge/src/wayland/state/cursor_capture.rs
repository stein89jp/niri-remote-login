//! The separate RDP pointer from ext-image-copy-capture cursor sessions.
//!
//! Per activation the worker opens one cursor session for its output and the
//! seat pointer. The cursor session reports enter/leave, the hotspot and the
//! position of the compositor's pointer in output buffer pixels (RDP desktop
//! pixels); its capture session renders the cursor image into one SHM
//! ARGB8888 buffer, with exactly one frame pending at a time.
//!
//! `CursorTracker` maps those events onto the `CursorSnapshot` without any
//! Wayland objects, so the mapping rules are unit-tested:
//! - R1: the first position of a session is a baseline, never a warp;
//! - R2: the position that comes with a later `enter` is a baseline when it is
//!   where the bridge's own virtual pointer put the cursor (within one pixel of
//!   a point injected since the preceding `leave`, or the last point before
//!   it); otherwise it is a warp;
//! - R3: the worker publishes the snapshot once per dispatched batch (see
//!   `State::service_cursor`), so the compositor's leave+enter re-send of a
//!   repeated warp never reaches RDP as a HidePointer flicker.
//!
//! The compositor's patch 0011 keeps the bridge's own motion from being echoed
//! as positions; that requires the virtual pointer and this session to share
//! one Wayland client, which they do.
use super::super::CursorSnapshot;
use super::super::capture::CaptureBuffer;
use super::State;
use ironrdp_server::DisplayUpdate;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
        ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

/// The compositor offers any cursor size and has no `unsupported` event.
/// Larger cursors are drawn into the video instead.
pub(super) const CURSOR_LIMIT: u32 = 384;
/// Larger shapes need the Large Pointer update.
const RGBA_POINTER_LIMIT: u32 = 96;
/// Re-arm delay after a frame failed without new constraints.
pub(super) const FRAME_RETRY: Duration = Duration::from_millis(50);
/// Re-creation delay after the compositor stopped the session.
pub(super) const SESSION_RETRY: Duration = Duration::from_secs(1);
/// Fractional-scale rounding moves the compositor's position by at most this.
const ECHO_TOLERANCE: i64 = 1;
/// Bound on own points remembered while the cursor is hidden.
const INJECTED_HISTORY: usize = 32;

/// Convert a premultiplied ARGB8888 cursor (little-endian B, G, R, A rows,
/// top-down) to the straight-alpha, bottom-up BGRA of MS-RDPBCGR Color/New
/// Pointer masks. `None` means the image is fully transparent.
pub(super) fn cursor_image(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> anyhow::Result<Option<Vec<u8>>> {
    anyhow::ensure!(
        (1..=CURSOR_LIMIT).contains(&width) && (1..=CURSOR_LIMIT).contains(&height),
        "invalid cursor geometry"
    );
    let row = width as usize * 4;
    anyhow::ensure!(
        stride as usize >= row && stride.is_multiple_of(4),
        "invalid cursor stride"
    );
    anyhow::ensure!(
        bytes.len() == stride as usize * height as usize,
        "cursor image length mismatch"
    );
    let mut bgra = Vec::with_capacity(row * height as usize);
    let mut opaque = false;
    for line in bytes.chunks_exact(stride as usize).rev() {
        for pixel in line[..row].as_chunks::<4>().0 {
            let alpha = u32::from(pixel[3]);
            opaque |= alpha != 0;
            let color = |channel: u8| {
                (u32::from(channel) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255) as u8
            };
            bgra.extend_from_slice(&[color(pixel[0]), color(pixel[1]), color(pixel[2]), pixel[3]]);
        }
    }
    Ok(opaque.then_some(bgra))
}

fn pointer_update(width: u32, height: u32, hotspot: (u16, u16), data: Vec<u8>) -> DisplayUpdate {
    let (width, height) = (width as u16, height as u16);
    let (hot_x, hot_y) = hotspot;
    if u32::from(width) > RGBA_POINTER_LIMIT || u32::from(height) > RGBA_POINTER_LIMIT {
        DisplayUpdate::LargePointer(ironrdp_server::LargePointer {
            cache_index: 0,
            width,
            height,
            hot_x,
            hot_y,
            data,
        })
    } else {
        DisplayUpdate::RGBAPointer(ironrdp_server::RGBAPointer {
            cache_index: 0,
            width,
            height,
            hot_x,
            hot_y,
            data,
        })
    }
}

fn same_shape(a: &DisplayUpdate, b: &DisplayUpdate) -> bool {
    match (a, b) {
        (DisplayUpdate::RGBAPointer(a), DisplayUpdate::RGBAPointer(b)) => {
            (a.width, a.height, a.hot_x, a.hot_y) == (b.width, b.height, b.hot_x, b.hot_y)
                && a.data == b.data
        }
        (DisplayUpdate::LargePointer(a), DisplayUpdate::LargePointer(b)) => {
            (a.width, a.height, a.hot_x, a.hot_y) == (b.width, b.height, b.hot_x, b.hot_y)
                && a.data == b.data
        }
        _ => false,
    }
}

/// Why the next position may be a baseline instead of a warp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Baseline {
    /// R1: the session's first position.
    Session,
    /// R2: the position following a later `enter`.
    Enter,
}

/// The Wayland-free mapping from cursor session events to the snapshot.
#[derive(Debug)]
pub(super) struct CursorTracker {
    entered: bool,
    baseline: Option<Baseline>,
    /// The latest constraints fit the client's pointer limit as ARGB8888.
    fits: bool,
    /// A frame completed for the current constraints.
    has_frame: bool,
    transparent: bool,
    /// The latest hotspot; it belongs to the next completed frame.
    hotspot: (i32, i32),
    /// Points the bridge injected since the last `leave`, plus the last one
    /// before it.
    injected: VecDeque<(u32, u32)>,
}

impl CursorTracker {
    pub(super) fn new() -> Self {
        Self {
            entered: false,
            baseline: Some(Baseline::Session),
            fits: false,
            has_frame: false,
            transparent: false,
            hotspot: (0, 0),
            injected: VecDeque::new(),
        }
    }
    /// Record where the bridge's own virtual pointer moved the cursor.
    pub(super) fn injected(&mut self, point: (u32, u32)) {
        if self.injected.back() != Some(&point) {
            if self.injected.len() == INJECTED_HISTORY {
                self.injected.pop_front();
            }
            self.injected.push_back(point);
        }
    }
    /// New buffer constraints; shapes from older ones no longer count.
    pub(super) fn constraints(&mut self, fits: bool) {
        if !fits {
            self.has_frame = false;
        }
        self.fits = fits;
    }
    pub(super) fn hotspot(&mut self, x: i32, y: i32) {
        self.hotspot = (x, y);
    }
    pub(super) fn enter(&mut self) {
        self.entered = true;
        if self.baseline != Some(Baseline::Session) {
            self.baseline = Some(Baseline::Enter);
        }
    }
    pub(super) fn leave(&mut self) {
        self.entered = false;
        if self.baseline == Some(Baseline::Enter) {
            self.baseline = None;
        }
        // Later own motion is compared from here on.
        while self.injected.len() > 1 {
            self.injected.pop_front();
        }
    }
    /// A compositor position, clamped to the desktop. Only warps advance the
    /// position generation, which moves the client's local pointer.
    pub(super) fn position(
        &mut self,
        x: i32,
        y: i32,
        desktop: (u16, u16),
        snapshot: &mut CursorSnapshot,
    ) {
        let clamp = |value: i32, size: u16| value.clamp(0, i32::from(size.max(1)) - 1) as u16;
        let point = (clamp(x, desktop.0), clamp(y, desktop.1));
        let baseline = match self.baseline.take() {
            Some(Baseline::Session) => true,
            Some(Baseline::Enter) => self.injected.iter().any(|&(ix, iy)| {
                (i64::from(ix) - i64::from(point.0)).abs() <= ECHO_TOLERANCE
                    && (i64::from(iy) - i64::from(point.1)).abs() <= ECHO_TOLERANCE
            }),
            None => false,
        };
        snapshot.position = point;
        if !baseline {
            snapshot.position_generation = snapshot.position_generation.wrapping_add(1);
        }
    }
    /// A completed frame of `width`x`height`; `None` pixels are transparent.
    /// An unchanged shape keeps its `Arc`, so nothing is sent again.
    pub(super) fn frame(
        &mut self,
        pixels: Option<Vec<u8>>,
        (width, height): (u32, u32),
        snapshot: &mut CursorSnapshot,
    ) {
        self.has_frame = true;
        let Some(pixels) = pixels else {
            self.transparent = true;
            return;
        };
        self.transparent = false;
        let hotspot = (
            self.hotspot.0.clamp(0, width as i32 - 1) as u16,
            self.hotspot.1.clamp(0, height as i32 - 1) as u16,
        );
        let shape = pointer_update(width, height, hotspot, pixels);
        if !snapshot
            .shape
            .as_deref()
            .is_some_and(|current| same_shape(current, &shape))
        {
            snapshot.shape = Some(Arc::new(shape));
        }
    }
    /// Derive the snapshot's separation and visibility.
    pub(super) fn apply(&self, snapshot: &mut CursorSnapshot) {
        snapshot.separate = self.fits && self.has_frame;
        snapshot.visible =
            snapshot.separate && self.entered && !self.transparent && snapshot.shape.is_some();
    }
}

/// One activation's cursor session objects and its capture buffer.
pub(in crate::wayland) struct CursorCapture {
    pointer: wl_pointer::WlPointer,
    source: ExtImageCaptureSourceV1,
    cursor: ExtImageCopyCaptureCursorSessionV1,
    session: ExtImageCopyCaptureSessionV1,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    buffer: Option<(CaptureBuffer, (u32, u32))>,
    offered_size: Option<(u32, u32)>,
    offered_argb: bool,
    retry_at: Option<Instant>,
    pub(super) tracker: CursorTracker,
}

impl CursorCapture {
    pub(super) fn new(
        sources: &ExtOutputImageCaptureSourceManagerV1,
        image_copy: &ExtImageCopyCaptureManagerV1,
        seat: &wl_seat::WlSeat,
        output: &wl_output::WlOutput,
        h: &QueueHandle<State>,
    ) -> Self {
        // The bridge has no surfaces, so this pointer receives no events.
        let pointer = seat.get_pointer(h, ());
        let source = sources.create_source(output, h, ());
        let cursor = image_copy.create_pointer_cursor_session(&source, &pointer, h, ());
        let session = cursor.get_capture_session(h, ());
        Self {
            pointer,
            source,
            cursor,
            session,
            frame: None,
            buffer: None,
            offered_size: None,
            offered_argb: false,
            retry_at: None,
            tracker: CursorTracker::new(),
        }
    }
    /// Capture the next cursor image change into the buffer, unless a frame
    /// is already pending or the constraints do not fit.
    fn arm(&mut self, h: &QueueHandle<State>) {
        if self.frame.is_some() || !self.tracker.fits {
            return;
        }
        let Some((buffer, (width, height))) = &self.buffer else {
            return;
        };
        let Some(wl_buffer) = buffer.buffer() else {
            return;
        };
        let frame = self.session.create_frame(h, ());
        frame.attach_buffer(wl_buffer);
        frame.damage_buffer(0, 0, *width as i32, *height as i32);
        frame.capture();
        self.frame = Some(frame);
        self.retry_at = None;
    }
    fn drop_frame(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
    }
    /// A complete set of constraints: (re)allocate and arm, or fall back.
    fn constraints_done(&mut self, shm: Option<&wl_shm::WlShm>, max: u16, h: &QueueHandle<State>) {
        let size = self.offered_size.take();
        let argb = std::mem::take(&mut self.offered_argb);
        let limit = u32::from(max).min(CURSOR_LIMIT);
        let fitting = size.filter(|&(width, height)| {
            argb && (1..=limit).contains(&width) && (1..=limit).contains(&height)
        });
        let Some((width, height)) = fitting else {
            // Embedded in video until the compositor offers a fitting size.
            self.tracker.constraints(false);
            self.drop_frame();
            self.buffer = None;
            return;
        };
        if self
            .buffer
            .as_ref()
            .is_none_or(|(_, current)| *current != (width, height))
        {
            // A frame against the old buffer can no longer complete.
            self.drop_frame();
            self.buffer = None;
            let layout = (wl_shm::Format::Argb8888, width, height, width * 4);
            match shm.map(|shm| CaptureBuffer::new(shm, layout, h)) {
                Some(Ok(buffer)) => self.buffer = Some((buffer, (width, height))),
                Some(Err(error)) => eprintln!("cursor capture buffer unavailable: {error}"),
                None => eprintln!("cursor capture needs wl_shm"),
            }
        }
        self.tracker.constraints(self.buffer.is_some());
        self.arm(h);
    }
    /// The pending frame completed: convert it and wait for the next change.
    fn ready(&mut self, h: &QueueHandle<State>, snapshot: &mut CursorSnapshot) {
        self.drop_frame();
        if let Some((buffer, (width, height))) = &self.buffer {
            match cursor_image(buffer.pixels(), *width, *height, width * 4) {
                Ok(pixels) => self.tracker.frame(pixels, (*width, *height), snapshot),
                Err(error) => eprintln!("invalid cursor capture: {error}"),
            }
        }
        self.arm(h);
    }
    /// The frame failed without stopping the session. New constraints re-arm
    /// at once; otherwise retry after a short delay.
    fn failed(&mut self) {
        self.drop_frame();
        self.retry_at = Some(Instant::now() + FRAME_RETRY);
    }
    pub(super) fn retry(&mut self, now: Instant, h: &QueueHandle<State>) {
        if self.retry_at.is_some_and(|at| now >= at) {
            self.retry_at = None;
            self.arm(h);
        }
    }
    #[cfg(test)]
    pub(super) fn pending_frame(&self) -> Option<&ExtImageCopyCaptureFrameV1> {
        self.frame.as_ref()
    }
    #[cfg(test)]
    pub(super) fn buffer(&self) -> Option<(&CaptureBuffer, (u32, u32))> {
        self.buffer.as_ref().map(|(buffer, size)| (buffer, *size))
    }
    #[cfg(test)]
    pub(super) fn cursor_session(&self) -> &ExtImageCopyCaptureCursorSessionV1 {
        &self.cursor
    }
    #[cfg(test)]
    pub(super) fn capture_session(&self) -> &ExtImageCopyCaptureSessionV1 {
        &self.session
    }
}

impl Drop for CursorCapture {
    /// Frame, capture session, cursor session, source, pointer, then the
    /// buffer. The cursor session is destroyed explicitly: Niri forgets it
    /// only then, not when its capture session goes away.
    fn drop(&mut self) {
        self.drop_frame();
        self.session.destroy();
        self.cursor.destroy();
        self.source.destroy();
        if self.pointer.version() >= 3 {
            self.pointer.release();
        }
        self.buffer = None;
    }
}

impl State {
    /// The selected output's mode, which is the RDP desktop size.
    fn cursor_desktop(&self) -> (u16, u16) {
        self.selected_output
            .and_then(|name| self.outputs.iter().find(|output| output.0 == name))
            .map(|output| output.5)
            .filter(|&(width, height)| width > 0 && height > 0)
            .map_or((u16::MAX, u16::MAX), |(width, height)| {
                (
                    width.min(u32::from(u16::MAX)) as u16,
                    height.min(u32::from(u16::MAX)) as u16,
                )
            })
    }
    /// Tear the session down after the compositor stopped it; the video
    /// carries the cursor until a retry succeeds.
    fn stop_cursor(&mut self) {
        self.cursor = None;
        self.cursor_retry_at = Some(Instant::now() + SESSION_RETRY);
        self.cursor_snapshot.separate = false;
        self.cursor_snapshot.visible = false;
    }
    fn apply_cursor(&mut self) {
        if let Some(capture) = &self.cursor {
            capture.tracker.apply(&mut self.cursor_snapshot);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, ()> for State {
    fn event(
        s: &mut Self,
        cursor: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let desktop = s.cursor_desktop();
        let State {
            cursor: capture,
            cursor_snapshot,
            ..
        } = s;
        let Some(capture) = capture.as_mut().filter(|c| &c.cursor == cursor) else {
            return;
        };
        let tracker = &mut capture.tracker;
        match event {
            ext_image_copy_capture_cursor_session_v1::Event::Enter => tracker.enter(),
            ext_image_copy_capture_cursor_session_v1::Event::Leave => tracker.leave(),
            ext_image_copy_capture_cursor_session_v1::Event::Position { x, y } => {
                tracker.position(x, y, desktop, cursor_snapshot)
            }
            ext_image_copy_capture_cursor_session_v1::Event::Hotspot { x, y } => {
                tracker.hotspot(x, y)
            }
            _ => {}
        }
        s.apply_cursor();
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        s: &mut Self,
        session: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        h: &QueueHandle<Self>,
    ) {
        let max = s.cursor_max;
        let State {
            cursor: capture,
            shm,
            ..
        } = s;
        let Some(capture) = capture.as_mut().filter(|c| &c.session == session) else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                capture.offered_size = Some((width, height))
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                capture.offered_argb |= format == WEnum::Value(wl_shm::Format::Argb8888)
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                capture.constraints_done(shm.as_ref(), max, h)
            }
            ext_image_copy_capture_session_v1::Event::Stopped => s.stop_cursor(),
            _ => {}
        }
        s.apply_cursor();
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        s: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        h: &QueueHandle<Self>,
    ) {
        let State {
            cursor: capture,
            cursor_snapshot,
            ..
        } = s;
        let Some(capture) = capture.as_mut().filter(|c| c.frame.as_ref() == Some(frame)) else {
            return;
        };
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => capture.ready(h, cursor_snapshot),
            ext_image_copy_capture_frame_v1::Event::Failed {
                reason: WEnum::Value(FailureReason::Stopped),
            } => s.stop_cursor(),
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => capture.failed(),
            _ => {}
        }
        s.apply_cursor();
    }
}

delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore wl_pointer::WlPointer);

#[cfg(test)]
mod tests {
    use super::super::cursor_cache::PointerCache;
    use super::*;
    use std::os::unix::net::UnixStream;
    use tokio::sync::watch;
    use wayland_client::EventQueue;

    /// A 24x24 cursor whose top row differs from the rest.
    fn argb(value: u8) -> Vec<u8> {
        let mut pixels = vec![0u8; 24 * 24 * 4];
        for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let alpha = if index < 24 { 255 } else { 128 };
            pixel.copy_from_slice(&[value / 2, value / 2, value / 2, alpha]);
        }
        pixels
    }
    fn shape_of(snapshot: &CursorSnapshot) -> (u16, u16, u16, u16) {
        match snapshot.shape.as_deref() {
            Some(DisplayUpdate::RGBAPointer(p)) => (p.width, p.height, p.hot_x, p.hot_y),
            Some(DisplayUpdate::LargePointer(p)) => (p.width, p.height, p.hot_x, p.hot_y),
            _ => panic!("no pointer shape"),
        }
    }
    /// A tracker that entered with a baseline and completed one 24x24 frame.
    fn shown(snapshot: &mut CursorSnapshot) -> CursorTracker {
        let mut tracker = CursorTracker::new();
        tracker.constraints(true);
        tracker.hotspot(2, 3);
        tracker.enter();
        tracker.position(100, 100, (640, 480), snapshot);
        let pixels = cursor_image(&argb(10), 24, 24, 96).unwrap();
        tracker.frame(pixels, (24, 24), snapshot);
        tracker.apply(snapshot);
        assert!(snapshot.separate && snapshot.visible);
        tracker
    }

    #[test]
    fn cursor_image_converts_premultiplied_argb8888_to_straight_bottom_up_bgra() {
        // Two rows of one pixel: B, G, R, A premultiplied, top-down.
        let image = cursor_image(&[10, 20, 30, 255, 5, 10, 20, 128], 1, 2, 4).unwrap();
        assert_eq!(image, Some(vec![10, 20, 40, 128, 10, 20, 30, 255]));
        // Row padding past the width is skipped.
        let image = cursor_image(&[1, 2, 3, 255, 9, 9, 9, 9], 1, 1, 8).unwrap();
        assert_eq!(image, Some(vec![1, 2, 3, 255]));
        let bytes = vec![255u8; 385 * 4];
        for (width, height, stride, length) in [
            (0, 1, 4, 0),
            (385, 1, 385 * 4, 385 * 4),
            (1, 385, 4, 385 * 4),
            (2, 1, 4, 4),
            (1, 1, 6, 6),
            (1, 2, 4, 4),
            (1, 1, 4, 8),
        ] {
            assert!(
                cursor_image(&bytes[..length], width, height, stride).is_err(),
                "{width}x{height} stride {stride} length {length}"
            );
        }
    }
    #[test]
    fn transparent_cursor_frame_hides_pointer() {
        assert_eq!(cursor_image(&[0; 16], 2, 2, 8).unwrap(), None);
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        // A client set a null cursor: the session keeps a transparent image.
        tracker.frame(None, (24, 24), &mut snapshot);
        tracker.apply(&mut snapshot);
        assert!(snapshot.separate && !snapshot.visible);
        let pixels = cursor_image(&argb(10), 24, 24, 96).unwrap();
        tracker.frame(pixels, (24, 24), &mut snapshot);
        tracker.apply(&mut snapshot);
        assert!(snapshot.visible);
    }
    #[test]
    fn first_position_of_a_session_is_a_baseline() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = CursorTracker::new();
        tracker.injected((5, 5));
        tracker.enter();
        tracker.position(-7, 900, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position, (0, 479));
        assert_eq!(snapshot.position_generation, 0);
        // Later positions while entered are compositor warps.
        tracker.position(700, 10, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position, (639, 10));
        assert_eq!(snapshot.position_generation, 1);
        // A new session (reconnect) starts with a baseline again.
        let mut tracker = CursorTracker::new();
        tracker.enter();
        tracker.position(30, 40, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position_generation, 1);
    }
    #[test]
    fn enter_position_at_injected_point_is_not_a_warp() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        // hide-when-typing, then the user moves: the first own motion shows
        // the cursor again while later moves are already on the way.
        tracker.leave();
        for point in [(50, 60), (55, 65), (60, 70)] {
            tracker.injected(point);
        }
        tracker.enter();
        tracker.position(56, 64, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position_generation, 0);
        // Showing again at the last point from before the leave.
        tracker.leave();
        tracker.enter();
        tracker.position(60, 71, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position_generation, 0);
    }
    #[test]
    fn enter_position_elsewhere_is_a_warp() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        tracker.injected((10, 10));
        tracker.injected((300, 200));
        tracker.injected((20, 20));
        // Points from before the leave other than the last do not count.
        tracker.leave();
        tracker.enter();
        tracker.position(300, 200, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position, (300, 200));
        assert_eq!(snapshot.position_generation, 1);
        // Only the position right after an enter may be a baseline.
        tracker.injected((400, 300));
        tracker.position(400, 300, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position_generation, 2);
    }
    #[test]
    fn repeated_warp_to_same_point_bumps_generation() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        tracker.position(320, 240, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position_generation, 1);
        // Local moves, then Niri warps to the same point again. It re-sends
        // the position after a leave and an enter (patch 0011).
        tracker.injected((20, 20));
        tracker.leave();
        tracker.enter();
        tracker.hotspot(2, 3);
        tracker.position(320, 240, (640, 480), &mut snapshot);
        assert_eq!(snapshot.position, (320, 240));
        assert_eq!(snapshot.position_generation, 2);
    }
    #[test]
    fn leave_hides_and_enter_restores_shape() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        let shape = snapshot.shape.clone().unwrap();
        let mut cache = PointerCache::new(4);
        assert!(matches!(
            cache.resolve(&shape),
            DisplayUpdate::RGBAPointer(ref p) if p.cache_index == 0
        ));
        tracker.leave();
        tracker.apply(&mut snapshot);
        assert!(snapshot.separate && !snapshot.visible);
        tracker.enter();
        tracker.position(100, 100, (640, 480), &mut snapshot);
        tracker.apply(&mut snapshot);
        assert!(snapshot.separate && snapshot.visible);
        // The same shape object returns, so the stream sends a Cached Pointer.
        let restored = snapshot.shape.clone().unwrap();
        assert!(Arc::ptr_eq(&shape, &restored));
        assert!(matches!(
            cache.resolve(&restored),
            DisplayUpdate::CachedPointer(0)
        ));
    }
    #[test]
    fn hotspot_applies_to_next_ready_frame() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        assert_eq!(shape_of(&snapshot), (24, 24, 2, 3));
        tracker.hotspot(5, 7);
        tracker.apply(&mut snapshot);
        assert_eq!(shape_of(&snapshot), (24, 24, 2, 3));
        let pixels = cursor_image(&argb(10), 24, 24, 96).unwrap();
        tracker.frame(pixels.clone(), (24, 24), &mut snapshot);
        assert_eq!(shape_of(&snapshot), (24, 24, 5, 7));
        // Hotspots are clamped into the image.
        tracker.hotspot(100, -4);
        tracker.frame(pixels, (24, 24), &mut snapshot);
        assert_eq!(shape_of(&snapshot), (24, 24, 23, 0));
        // Large shapes use the Large Pointer update.
        let large = cursor_image(&[255; 128 * 128 * 4], 128, 128, 512).unwrap();
        tracker.frame(large, (128, 128), &mut snapshot);
        assert!(matches!(
            snapshot.shape.as_deref(),
            Some(DisplayUpdate::LargePointer(_))
        ));
    }
    #[test]
    fn unchanged_frame_is_not_republished() {
        let mut snapshot = CursorSnapshot::default();
        let mut tracker = shown(&mut snapshot);
        let shape = snapshot.shape.clone().unwrap();
        // An animated cursor step or a redraw with identical pixels.
        let pixels = cursor_image(&argb(10), 24, 24, 96).unwrap();
        tracker.frame(pixels, (24, 24), &mut snapshot);
        assert!(Arc::ptr_eq(&shape, snapshot.shape.as_ref().unwrap()));
        let pixels = cursor_image(&argb(20), 24, 24, 96).unwrap();
        tracker.frame(pixels, (24, 24), &mut snapshot);
        assert!(!Arc::ptr_eq(&shape, snapshot.shape.as_ref().unwrap()));
    }

    /// A worker state wired to an unanswered socket. Events are delivered by
    /// calling the dispatch handlers; `batch` ends one dispatched batch.
    struct Harness {
        _compositor: UnixStream,
        connection: Connection,
        _events: EventQueue<State>,
        handle: QueueHandle<State>,
        state: State,
        published: watch::Receiver<CursorSnapshot>,
    }
    impl Harness {
        fn new(max: u16, globals: bool) -> Self {
            let (socket, compositor) = UnixStream::pair().unwrap();
            let connection = Connection::from_socket(socket).unwrap();
            let events = connection.new_event_queue::<State>();
            let handle = events.handle();
            let registry = connection.display().get_registry(&handle, ());
            let output: wl_output::WlOutput = registry.bind(3, 4, &handle, ());
            let (tx, published) = watch::channel(CursorSnapshot::default());
            let mut state = State {
                seat: Some(registry.bind(1, 7, &handle, ())),
                shm: Some(registry.bind(2, 1, &handle, ())),
                outputs: vec![(3, output, "HEADLESS-1".into(), 1, true, (640, 480))],
                selected_output: Some(3),
                output_sources: globals.then(|| registry.bind(4, 1, &handle, ())),
                image_copy: globals.then(|| registry.bind(5, 1, &handle, ())),
                cursor_tx: Some(tx),
                ..State::default()
            };
            state.set_cursor_max(max);
            state.service_cursor(&handle);
            Self {
                _compositor: compositor,
                connection,
                _events: events,
                handle,
                state,
                published,
            }
        }
        fn capture(&self) -> &CursorCapture {
            self.state.cursor.as_ref().expect("cursor session")
        }
        fn session(&mut self, event: ext_image_copy_capture_session_v1::Event) {
            let session = self.capture().capture_session().clone();
            <State as Dispatch<ExtImageCopyCaptureSessionV1, ()>>::event(
                &mut self.state,
                &session,
                event,
                &(),
                &self.connection,
                &self.handle,
            );
        }
        fn constraints(&mut self, width: u32, height: u32, format: wl_shm::Format) {
            self.session(ext_image_copy_capture_session_v1::Event::BufferSize { width, height });
            self.session(ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(format),
            });
            self.session(ext_image_copy_capture_session_v1::Event::Done);
        }
        fn cursor(&mut self, event: ext_image_copy_capture_cursor_session_v1::Event) {
            let cursor = self.capture().cursor_session().clone();
            <State as Dispatch<ExtImageCopyCaptureCursorSessionV1, ()>>::event(
                &mut self.state,
                &cursor,
                event,
                &(),
                &self.connection,
                &self.handle,
            );
        }
        fn frame(&mut self, event: ext_image_copy_capture_frame_v1::Event) {
            let frame = self
                .capture()
                .pending_frame()
                .expect("pending frame")
                .clone();
            <State as Dispatch<ExtImageCopyCaptureFrameV1, ()>>::event(
                &mut self.state,
                &frame,
                event,
                &(),
                &self.connection,
                &self.handle,
            );
        }
        /// The compositor renders `pixels` into the pending frame's buffer.
        fn ready(&mut self, pixels: &[u8]) {
            self.capture()
                .buffer()
                .unwrap()
                .0
                .write_as_compositor(pixels);
            self.frame(ext_image_copy_capture_frame_v1::Event::Ready);
        }
        /// End of one dispatched batch; returns how many snapshots were sent.
        fn batch(&mut self) -> u64 {
            let before = self.state.cursor_snapshot.generation;
            self.state.service_cursor(&self.handle);
            assert_eq!(
                self.published.borrow_and_update().generation,
                self.state.cursor_snapshot.generation
            );
            self.state.cursor_snapshot.generation - before
        }
        fn sent(&self) -> CursorSnapshot {
            self.published.borrow().clone()
        }
        /// Constraints, entry, hotspot and one shape, as at session start.
        fn show(&mut self) {
            self.constraints(24, 24, wl_shm::Format::Argb8888);
            self.cursor(ext_image_copy_capture_cursor_session_v1::Event::Enter);
            self.cursor(ext_image_copy_capture_cursor_session_v1::Event::Hotspot { x: 2, y: 3 });
            self.cursor(ext_image_copy_capture_cursor_session_v1::Event::Position {
                x: 100,
                y: 100,
            });
            self.ready(&argb(10));
            assert_eq!(self.batch(), 1);
            let sent = self.sent();
            assert!(sent.separate && sent.visible);
            assert_eq!(sent.position_generation, 0, "no startup warp");
        }
    }
    use ext_image_copy_capture_cursor_session_v1::Event as CursorEvent;
    use ext_image_copy_capture_frame_v1::Event as FrameEvent;

    #[test]
    fn leave_enter_in_one_dispatch_publishes_once() {
        let mut h = Harness::new(96, true);
        h.show();
        h.state.note_injected((20, 20));
        assert_eq!(h.batch(), 0);
        // A warp back to the last reported position: Niri re-sends it.
        h.cursor(CursorEvent::Leave);
        h.cursor(CursorEvent::Enter);
        h.cursor(CursorEvent::Hotspot { x: 2, y: 3 });
        h.cursor(CursorEvent::Position { x: 100, y: 100 });
        assert_eq!(h.batch(), 1);
        let sent = h.sent();
        assert!(sent.separate && sent.visible, "no HidePointer flicker");
        assert_eq!((sent.position, sent.position_generation), ((100, 100), 1));
        // A pair that only re-shows the cursor at the bridge's own point
        // changes nothing the client acts on.
        h.cursor(CursorEvent::Leave);
        h.cursor(CursorEvent::Enter);
        h.cursor(CursorEvent::Position { x: 20, y: 21 });
        assert_eq!(h.batch(), 0);
        // Across two batches the hide is visible.
        h.cursor(CursorEvent::Leave);
        assert_eq!(h.batch(), 1);
        assert!(!h.sent().visible);
        h.cursor(CursorEvent::Enter);
        h.cursor(CursorEvent::Position { x: 20, y: 20 });
        assert_eq!(h.batch(), 1);
        assert!(h.sent().visible);
        assert_eq!(h.sent().position_generation, 1);
    }
    #[test]
    fn oversized_constraints_fall_back_to_embedded_and_recover() {
        let mut h = Harness::new(96, true);
        h.show();
        // 64 px at 200 %: larger than the client's pointer limit.
        h.constraints(128, 128, wl_shm::Format::Argb8888);
        assert!(h.capture().pending_frame().is_none());
        assert!(h.capture().buffer().is_none());
        assert_eq!(h.batch(), 1);
        assert!(!h.sent().separate);
        // Without ARGB8888 the cursor stays embedded too.
        h.constraints(24, 24, wl_shm::Format::Xrgb8888);
        assert!(h.capture().pending_frame().is_none());
        // A fitting size arms a frame, and separation waits for its shape.
        h.constraints(32, 32, wl_shm::Format::Argb8888);
        assert_eq!(h.capture().buffer().unwrap().1, (32, 32));
        assert!(h.capture().pending_frame().is_some());
        assert_eq!(h.batch(), 0);
        h.ready(&[255; 32 * 32 * 4]);
        assert_eq!(h.batch(), 1);
        assert!(h.sent().separate && h.sent().visible);
        assert_eq!(shape_of(&h.sent()), (32, 32, 2, 3));
        // A client that accepts large pointers gets them up to 384 px.
        let mut h = Harness::new(384, true);
        h.constraints(128, 128, wl_shm::Format::Argb8888);
        h.cursor(CursorEvent::Enter);
        h.cursor(CursorEvent::Position { x: 1, y: 1 });
        h.ready(&[255; 128 * 128 * 4]);
        assert_eq!(h.batch(), 1);
        assert!(matches!(
            h.sent().shape.as_deref(),
            Some(DisplayUpdate::LargePointer(_))
        ));
        h.constraints(385, 385, wl_shm::Format::Argb8888);
        assert_eq!(h.batch(), 1);
        assert!(!h.sent().separate);
    }
    #[test]
    fn buffer_constraints_failure_reallocates_before_next_capture() {
        let mut h = Harness::new(96, true);
        h.show();
        let first = h.capture().pending_frame().unwrap().clone();
        // A theme size change: Niri fails the frame, then sends the new size.
        h.frame(FrameEvent::Failed {
            reason: WEnum::Value(FailureReason::BufferConstraints),
        });
        assert!(h.capture().pending_frame().is_none());
        h.constraints(48, 48, wl_shm::Format::Argb8888);
        assert_eq!(h.capture().buffer().unwrap().1, (48, 48));
        let next = h.capture().pending_frame().unwrap().clone();
        assert_ne!(first, next);
        // The old shape stays separate until the new one is ready.
        assert_eq!(h.batch(), 0);
        h.ready(&[255; 48 * 48 * 4]);
        assert_eq!(h.batch(), 1);
        assert_eq!(shape_of(&h.sent()), (48, 48, 2, 3));
        // Without new constraints the frame is re-armed after a delay.
        h.frame(FrameEvent::Failed {
            reason: WEnum::Value(FailureReason::Unknown),
        });
        assert!(h.capture().pending_frame().is_none());
        let handle = h.handle.clone();
        let capture = h.state.cursor.as_mut().unwrap();
        capture.retry(Instant::now(), &handle);
        assert!(capture.pending_frame().is_none());
        capture.retry(Instant::now() + FRAME_RETRY, &handle);
        assert!(capture.pending_frame().is_some());
        assert!(h.sent().separate && h.sent().visible);
    }
    #[test]
    fn stopped_session_falls_back_and_retries() {
        for stop_frame in [false, true] {
            let mut h = Harness::new(96, true);
            h.show();
            if stop_frame {
                h.frame(FrameEvent::Failed {
                    reason: WEnum::Value(FailureReason::Stopped),
                });
            } else {
                h.session(ext_image_copy_capture_session_v1::Event::Stopped);
            }
            assert!(h.state.cursor.is_none());
            assert_eq!(h.batch(), 1);
            assert!(!h.sent().separate && !h.sent().visible);
            // Not before the backoff.
            assert_eq!(h.batch(), 0);
            assert!(h.state.cursor.is_none());
            h.state.cursor_retry_at = Some(Instant::now());
            h.batch();
            assert!(h.state.cursor.is_some());
            // The new session's first position is again a baseline.
            h.show();
        }
    }
    #[test]
    fn missing_globals_keep_the_embedded_cursor() {
        let mut h = Harness::new(96, false);
        assert!(h.state.cursor.is_none());
        assert!(h.state.cursor_unavailable_logged);
        assert_eq!(h.batch(), 0);
        assert!(!h.sent().separate);
        // A client without pointer capability gets no session either.
        let mut h = Harness::new(0, true);
        assert!(h.state.cursor.is_none());
        assert_eq!(h.batch(), 0);
    }
    #[test]
    fn reactivation_replaces_the_session_and_starts_embedded() {
        let mut h = Harness::new(96, true);
        h.show();
        let old = h.capture().cursor_session().clone();
        h.state.set_cursor_max(96);
        assert!(h.state.cursor.is_none());
        assert!(!h.sent().separate);
        h.batch();
        assert_ne!(h.capture().cursor_session(), &old);
        // Events for the destroyed session are ignored.
        <State as Dispatch<ExtImageCopyCaptureCursorSessionV1, ()>>::event(
            &mut h.state,
            &old,
            CursorEvent::Position { x: 9, y: 9 },
            &(),
            &h.connection,
            &h.handle,
        );
        assert_eq!(h.batch(), 0);
        h.show();
    }
}
