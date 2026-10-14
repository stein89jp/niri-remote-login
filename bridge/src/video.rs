//! Optional host NVENC encoding; IronRDP owns graphics negotiation and framing.
use crate::server_error;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_egfx::{
    pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet},
    server::{GraphicsPipelineHandler, GraphicsPipelineServer},
};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, PixelFormat, RdpServerDisplay, RdpServerDisplayUpdates, ServerEvent,
    ServerEventSender, ServerResult,
};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

unsafe extern "C" {
    fn niri_video_free(encoder: *mut c_void);
    fn niri_video_new_config(
        width: c_int,
        height: c_int,
        software: c_int,
        fps: c_int,
        bitrate: c_int,
        preset: c_int,
        spatial_aq: c_int,
        avc444: c_int,
        error: *mut c_char,
        length: usize,
    ) -> *mut c_void;
    fn niri_video_set_rate(encoder: *mut c_void, bitrate: c_int, fps: c_int) -> c_int;
    fn niri_video_encode_pair(
        encoder: *mut c_void,
        pixels: *const u8,
        stride: c_int,
        force_idr: c_int,
        main: *mut *const u8,
        main_size: *mut c_int,
        aux: *mut *const u8,
        aux_size: *mut c_int,
        error: *mut c_char,
        length: usize,
    ) -> c_int;
    fn niri_video_encode_dmabuf(
        encoder: *mut c_void,
        fd: c_int,
        fourcc: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
        device: u64,
        force_idr: c_int,
        main: *mut *const u8,
        main_size: *mut c_int,
        aux: *mut *const u8,
        aux_size: *mut c_int,
        error: *mut c_char,
        length: usize,
    ) -> c_int;
    fn niri_video_timings(encoder: *mut c_void, convert_ms: *mut f64, encode_ms: *mut f64);
    #[cfg(test)]
    fn niri_video_set_bitrate(encoder: *mut c_void, bitrate: c_int) -> c_int;
    fn niri_video_encode(
        encoder: *mut c_void,
        pixels: *const u8,
        stride: c_int,
        force_idr: c_int,
        data: *mut *const u8,
        error: *mut c_char,
        length: usize,
    ) -> c_int;
}

pub struct Encoder {
    ptr: *mut c_void,
    width: u16,
    height: u16,
    bitrate: u32,
    fps: u32,
    avc444: bool,
    software: bool,
    gpu_active: bool,
}
#[derive(Debug)]
struct EncodedFrame {
    main: Vec<u8>,
    auxiliary: Option<Vec<u8>>,
    conversion: Duration,
    encode: Duration,
    captured_at: Option<Instant>,
    received_at: Instant,
}
impl From<Vec<u8>> for EncodedFrame {
    fn from(main: Vec<u8>) -> Self {
        Self {
            main,
            auxiliary: None,
            conversion: Duration::ZERO,
            encode: Duration::ZERO,
            captured_at: None,
            received_at: Instant::now(),
        }
    }
}
// The FFmpeg context has a single owner; access from encoding tasks is serialized.
unsafe impl Send for Encoder {}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { niri_video_free(self.ptr) };
    }
}
impl Encoder {
    fn set_rate(&mut self, bitrate: u32, fps: u32) -> anyhow::Result<bool> {
        if (self.bitrate, self.fps) == (bitrate, fps) {
            return Ok(false);
        }
        anyhow::ensure!(
            (1_000_000..=100_000_000).contains(&bitrate) && (1..=60).contains(&fps),
            "invalid adaptive H.264 rate"
        );
        if fps != self.fps {
            *self = Self::new_rate(
                self.width,
                self.height,
                self.software,
                self.avc444,
                fps,
                bitrate,
            )?;
            return Ok(true);
        }
        anyhow::ensure!(
            unsafe { niri_video_set_rate(self.ptr, bitrate as c_int, fps as c_int) } == 0,
            "native bitrate reconfiguration failed"
        );
        self.bitrate = bitrate;
        self.fps = fps;
        Ok(false)
    }
    #[cfg(test)]
    fn set_bitrate(&mut self, bitrate: u32) -> anyhow::Result<()> {
        if bitrate == self.bitrate {
            return Ok(());
        }
        anyhow::ensure!(
            (1_000_000..=100_000_000).contains(&bitrate)
                && unsafe { niri_video_set_bitrate(self.ptr, bitrate as c_int) } == 0,
            "invalid adaptive H.264 bitrate"
        );
        eprintln!(
            "RDP graphics adaptive H.264 target: {} -> {} kbps",
            self.bitrate / 1000,
            bitrate / 1000
        );
        self.bitrate = bitrate;
        Ok(())
    }
    pub fn nvenc(width: u16, height: u16) -> anyhow::Result<Self> {
        Self::new(width, height, false)
    }
    fn new(width: u16, height: u16, software: bool) -> anyhow::Result<Self> {
        Self::new_mode(width, height, software, false)
    }
    fn new_mode(width: u16, height: u16, software: bool, avc444: bool) -> anyhow::Result<Self> {
        Self::new_rate(
            width,
            height,
            software,
            avc444,
            crate::performance::frame_rate()?,
            crate::performance::bitrate()?,
        )
    }
    fn new_rate(
        width: u16,
        height: u16,
        software: bool,
        avc444: bool,
        fps: u32,
        bitrate: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            width >= 1 && height >= 1 && width <= 4096 && height <= 4096,
            "H.264 dimensions must be within 1..4096"
        );
        let (preset, spatial_aq) = crate::performance::quality_profile()?;
        let mut error = [0 as c_char; 256];
        let ptr = unsafe {
            niri_video_new_config(
                width.into(),
                height.into(),
                software.into(),
                fps as c_int,
                bitrate as c_int,
                preset as c_int,
                spatial_aq.into(),
                avc444.into(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(Self {
            ptr,
            width,
            height,
            bitrate,
            fps,
            avc444,
            software,
            gpu_active: false,
        })
    }
    fn timings(&self) -> (Duration, Duration) {
        let (mut conversion, mut encode) = (0.0, 0.0);
        unsafe { niri_video_timings(self.ptr, &mut conversion, &mut encode) };
        (
            Duration::from_secs_f64(conversion.max(0.0) / 1000.0),
            Duration::from_secs_f64(encode.max(0.0) / 1000.0),
        )
    }
    fn encoded(
        &self,
        main: *const u8,
        main_size: c_int,
        aux: *const u8,
        aux_size: c_int,
    ) -> anyhow::Result<EncodedFrame> {
        anyhow::ensure!(
            main_size > 0 && !main.is_null() && aux_size >= 0 && (aux_size == 0 || !aux.is_null()),
            "invalid native H.264 packets"
        );
        let (conversion, encode) = self.timings();
        Ok(EncodedFrame {
            main: unsafe { std::slice::from_raw_parts(main, main_size as usize) }.to_vec(),
            auxiliary: if aux_size > 0 {
                Some(unsafe { std::slice::from_raw_parts(aux, aux_size as usize) }.to_vec())
            } else {
                None
            },
            conversion,
            encode,
            captured_at: None,
            received_at: Instant::now(),
        })
    }
    fn encode_views(
        &mut self,
        image: &BitmapUpdate,
        force_idr: bool,
    ) -> anyhow::Result<EncodedFrame> {
        if !self.avc444 {
            let main = self.encode(image, force_idr)?;
            let (conversion, encode) = self.timings();
            return Ok(EncodedFrame {
                main,
                auxiliary: None,
                conversion,
                encode,
                captured_at: None,
                received_at: Instant::now(),
            });
        }
        anyhow::ensure!(
            image.x == 0
                && image.y == 0
                && image.width.get() == self.width
                && image.height.get() == self.height
                && matches!(image.format, PixelFormat::BgrA32 | PixelFormat::BgrX32),
            "AVC444 requires full BGR desktop"
        );
        let stride = image.stride.get();
        anyhow::ensure!(
            stride >= usize::from(self.width) * 4
                && stride <= 4096 * 4
                && image.data.len() >= stride * usize::from(self.height),
            "invalid AVC444 input"
        );
        let mut error = [0 as c_char; 256];
        let (mut main, mut aux) = (std::ptr::null(), std::ptr::null());
        let (mut main_size, mut aux_size) = (0, 0);
        let result = unsafe {
            niri_video_encode_pair(
                self.ptr,
                image.data.as_ptr(),
                stride as c_int,
                force_idr.into(),
                &mut main,
                &mut main_size,
                &mut aux,
                &mut aux_size,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            result == 0,
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        self.encoded(main, main_size, aux, aux_size)
    }
    fn encode_gpu(
        &mut self,
        image: &crate::wayland::GpuFrame,
        force_idr: bool,
    ) -> anyhow::Result<EncodedFrame> {
        anyhow::ensure!(
            image.width == u32::from(self.width) && image.height == u32::from(self.height),
            "GPU frame geometry changed"
        );
        anyhow::ensure!(!image.inverted, "inverted GPU capture uses SHM fallback");
        let mut error = [0 as c_char; 256];
        let (mut main, mut aux) = (std::ptr::null(), std::ptr::null());
        let (mut main_size, mut aux_size) = (0, 0);
        let result = unsafe {
            niri_video_encode_dmabuf(
                self.ptr,
                image.fd.as_raw_fd(),
                image.fourcc,
                image.stride,
                image.offset,
                image.modifier,
                image.device,
                force_idr.into(),
                &mut main,
                &mut main_size,
                &mut aux,
                &mut aux_size,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            result == 0,
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        let result = self.encoded(main, main_size, aux, aux_size)?;
        if !self.gpu_active {
            eprintln!(
                "RDP GPU capture using EGL/CUDA DMA-BUF NVENC ({}x{})",
                self.width, self.height
            );
        }
        self.gpu_active = true;
        Ok(result)
    }
    #[cfg(test)]
    fn encode_frame(
        encoder: &mut Option<Self>,
        image: &BitmapUpdate,
        force_idr: bool,
        create: impl FnOnce(u16, u16) -> anyhow::Result<Self>,
    ) -> anyhow::Result<Vec<u8>> {
        let (width, height) = (image.width.get(), image.height.get());
        let changed = encoder
            .as_ref()
            .is_none_or(|encoder| (encoder.width, encoder.height) != (width, height));
        if changed {
            *encoder = Some(create(width, height)?);
        }
        encoder
            .as_mut()
            .unwrap()
            .encode(image, force_idr || changed)
    }
    pub fn encode(&mut self, image: &BitmapUpdate, force_idr: bool) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            image.x == 0
                && image.y == 0
                && image.width.get() == self.width
                && image.height.get() == self.height
                && matches!(image.format, PixelFormat::BgrA32 | PixelFormat::BgrX32),
            "H.264 requires a full BGR desktop frame"
        );
        let stride = image.stride.get();
        anyhow::ensure!(
            stride >= usize::from(self.width) * 4
                && stride <= 4096 * 4
                && image.data.len() >= stride * usize::from(self.height),
            "invalid encoder buffer"
        );
        let mut error = [0 as c_char; 256];
        let mut data = std::ptr::null();
        let length = unsafe {
            niri_video_encode(
                self.ptr,
                image.data.as_ptr(),
                stride as c_int,
                force_idr.into(),
                &mut data,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            length > 0 && !data.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        // Packet storage belongs to the native encoder and is valid until its next call.
        Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
    }
}

#[derive(Default)]
struct Shared {
    handle: Option<GfxServerHandle>,
    sender: Option<UnboundedSender<ServerEvent>>,
    generation: Arc<AtomicU64>,
    suppressed: Arc<AtomicBool>,
    rate: Arc<Mutex<crate::performance::AdaptiveBitrate>>,
    audio_generation: u64,
    refresh: Arc<AtomicU64>,
    encode_gate: Arc<EncodeGate>,
}
struct EncodeGate(Arc<tokio::sync::Semaphore>);
impl Default for EncodeGate {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(1)))
    }
}
#[derive(Clone, Default)]
pub struct Video(Arc<Mutex<Shared>>);
type DesktopProvider = Arc<dyn Fn() -> Option<crate::wayland::Desktop> + Send + Sync>;

fn primary_monitor(width: u16, height: u16) -> Vec<Monitor> {
    vec![Monitor {
        left: 0,
        top: 0,
        right: i32::from(width) - 1,
        bottom: i32::from(height) - 1,
        flags: MonitorFlags::PRIMARY,
    }]
}

/// Sound feedback follows the active graphics context, including when sound
/// negotiation happens first. Older connections cannot reduce a newer one's rate.
#[derive(Clone)]
pub struct AudioFeedback {
    video: Video,
    generation: u64,
}
impl std::fmt::Debug for AudioFeedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioFeedback")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
impl AudioFeedback {
    pub fn report_congestion(&self, now: Instant) {
        let state = self.video.0.lock().unwrap();
        if state.audio_generation == self.generation {
            state.rate.lock().unwrap().audio_congested(now);
        }
    }
    pub fn clear(&self) {
        let state = self.video.0.lock().unwrap();
        if state.audio_generation == self.generation {
            state.rate.lock().unwrap().clear_audio_congestion();
        }
    }
}
impl Video {
    pub fn request_refresh(&self) {
        self.0
            .lock()
            .unwrap()
            .refresh
            .fetch_add(1, Ordering::SeqCst);
    }
    /// Create once per sound backend, then clone into its capture worker.
    pub fn audio_feedback(&self) -> AudioFeedback {
        let mut state = self.0.lock().unwrap();
        state.audio_generation = state.audio_generation.wrapping_add(1);
        state.rate.lock().unwrap().clear_audio_congestion();
        AudioFeedback {
            video: self.clone(),
            generation: state.audio_generation,
        }
    }
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        // Reject invalid capture settings before accepting client connections.
        crate::performance::frame_rate()?;
        crate::performance::bitrate()?;
        crate::performance::quality_profile()?;
        crate::performance::avc444_requested()?;
        crate::performance::gpu_capture_requested()?;
        crate::performance::joint_adaptation_requested()?;
        match std::env::var("NIRI_RDP_ENCODER")
            .as_deref()
            .unwrap_or("bitmap")
        {
            "bitmap" => Ok(None),
            "nvenc" => {
                // Explicit hardware requests fail at startup rather than quietly using the CPU.
                let mut encoder = Encoder::nvenc(1920, 1080)?;
                let image = crate::bitmap(
                    &vec![0; 1920 * 1080 * 4],
                    1920,
                    1080,
                    1920 * 4,
                    false,
                    DesktopSize {
                        width: 1920,
                        height: 1080,
                    },
                    false,
                )?;
                encoder.encode(&image, true)?;
                eprintln!("NVENC H.264 preflight passed; AVC420-capable clients use host encoding");
                let video = Self::default();
                *video.0.lock().unwrap().rate.lock().unwrap() =
                    crate::performance::AdaptiveBitrate::new(
                        crate::performance::bitrate()?,
                        Instant::now(),
                    );
                Ok(Some(video))
            }
            _ => anyhow::bail!("NIRI_RDP_ENCODER must be bitmap or nvenc"),
        }
    }
    pub fn display(&self, inner: crate::wayland::Desktop) -> Box<dyn RdpServerDisplay> {
        Box::new(VideoDisplay {
            inner,
            video: self.clone(),
        })
    }
    pub(crate) fn updates_send(
        &self,
        desktop: &crate::wayland::Desktop,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates + Send>> {
        self.wrap_updates_with_desktop(desktop.updates_full_send(), Some(desktop.clone()))
    }
    #[cfg(test)]
    pub(crate) fn wrap_updates(
        &self,
        inner: Box<dyn RdpServerDisplayUpdates + Send>,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates + Send>> {
        self.wrap_updates_with_desktop(inner, None)
    }
    pub(crate) fn wrap_updates_with_desktop_provider(
        &self,
        inner: Box<dyn RdpServerDisplayUpdates + Send>,
        provider: DesktopProvider,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates + Send>> {
        let mut stream = self.make_updates(inner, None)?;
        stream.desktop_provider = Some(provider);
        Ok(Box::new(stream))
    }
    fn wrap_updates_with_desktop(
        &self,
        inner: Box<dyn RdpServerDisplayUpdates + Send>,
        desktop: Option<crate::wayland::Desktop>,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates + Send>> {
        Ok(Box::new(self.make_updates(inner, desktop)?))
    }
    fn make_updates(
        &self,
        inner: Box<dyn RdpServerDisplayUpdates + Send>,
        desktop: Option<crate::wayland::Desktop>,
    ) -> ServerResult<VideoUpdates> {
        let state = self.0.lock().unwrap();
        Ok(VideoUpdates {
            inner,
            handle: state
                .handle
                .clone()
                .ok_or_else(|| server_error("missing graphics context"))?,
            sender: state
                .sender
                .clone()
                .ok_or_else(|| server_error("missing graphics sender"))?,
            encoder: Arc::new(Mutex::new(None)),
            pending: None,
            surface: None,
            start: Instant::now(),
            force_idr: true,
            last_image: None,
            ended: false,
            generation: state.generation.clone(),
            known_generation: state.generation.load(Ordering::SeqCst),
            suppressed: state.suppressed.clone(),
            rate: state.rate.clone(),
            desktop,
            desktop_provider: None,
            pending_gpu: false,
            gpu_failed: false,
            last_gpu: None,
            refinement: None,
            next_frame: Instant::now(),
            diagnostics: crate::performance::Telemetry::from_env(),
            credit_wait_started: None,
            fallback_previous: None,
            fallback_partial: Default::default(),
            fallback_resume: false,
            refresh: state.refresh.clone(),
            known_refresh: state.refresh.load(Ordering::SeqCst),
            pending_refresh: 0,
            encode_gate: state.encode_gate.clone(),
        })
    }
    pub fn set_suppressed(&self, suppressed: Arc<AtomicBool>) {
        self.0.lock().unwrap().suppressed = suppressed;
    }
}
struct Handler(
    Arc<AtomicU64>,
    Arc<Mutex<crate::performance::AdaptiveBitrate>>,
);
impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, _: &CapabilitiesAdvertisePdu) {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.lock().unwrap().reset_frames();
    }
    fn on_ready(&mut self, _: &CapabilitySet) {}
    fn on_close(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.lock().unwrap().reset_frames();
    }
    fn on_frame_ack(&mut self, frame_id: u32, queue_depth: u32, _: u32) {
        self.1
            .lock()
            .unwrap()
            .acknowledged(frame_id, queue_depth, Instant::now());
    }
    fn max_frames_in_flight(&self) -> u32 {
        self.1.lock().unwrap().max_frames_in_flight()
    }
}
impl ServerEventSender for Video {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.0.lock().unwrap().sender = Some(sender);
    }
}
impl GfxServerFactory for Video {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler(Arc::new(AtomicU64::new(0)), Arc::default()))
    }
    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        // A new context per connection prevents surfaces/acks crossing authenticated sessions.
        let generation = Arc::new(AtomicU64::new(0));
        let mut state = self.0.lock().unwrap();
        let now = Instant::now();
        let rate = {
            let previous = state.rate.lock().unwrap();
            let mut rate = crate::performance::AdaptiveBitrate::new(previous.ceiling(), now);
            rate.set_frame_rate_ceiling(crate::performance::frame_rate().unwrap_or(60));
            rate.enable_joint_adaptation(
                crate::performance::joint_adaptation_requested().unwrap_or(false),
            );
            rate.inherit_audio_congestion(&previous, now);
            Arc::new(Mutex::new(rate))
        };
        let handle = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(Handler(
            generation.clone(),
            rate.clone(),
        )))));
        state.handle = Some(handle.clone());
        state.generation = generation;
        state.rate = rate;
        Some((GfxDvcBridge::new(handle.clone()), handle))
    }
}
struct VideoDisplay {
    inner: crate::wayland::Desktop,
    video: Video,
}
#[async_trait::async_trait]
impl RdpServerDisplay for VideoDisplay {
    async fn size(&mut self) -> DesktopSize {
        self.inner.size().await
    }
    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        self.inner.request_initial_size(client_size).await
    }
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        self.inner.request_layout(layout);
    }
    fn pointer_capabilities(&mut self, max_size: u16) {
        self.inner.pointer_capabilities(max_size);
    }
    fn request_refresh(&mut self) {
        self.video.request_refresh();
        self.inner.request_refresh();
    }
    fn set_output_suppressed(&mut self, suppressed: bool) {
        if !suppressed {
            self.video.request_refresh();
        }
        self.inner.set_output_suppressed(suppressed);
    }
    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(self.video.updates_send(&self.inner)?)
    }
}
type EncodingTask = JoinHandle<anyhow::Result<EncodedFrame>>;
struct VideoUpdates {
    inner: Box<dyn RdpServerDisplayUpdates + Send>,
    handle: GfxServerHandle,
    sender: UnboundedSender<ServerEvent>,
    encoder: Arc<Mutex<Option<Encoder>>>,
    pending: Option<(EncodingTask, u16, u16, u64)>,
    surface: Option<u16>,
    start: Instant,
    force_idr: bool,
    // Bytes is reference counted: retaining one input adds no full-frame copy.
    last_image: Option<BitmapUpdate>,
    ended: bool,
    generation: Arc<AtomicU64>,
    known_generation: u64,
    suppressed: Arc<AtomicBool>,
    rate: Arc<Mutex<crate::performance::AdaptiveBitrate>>,
    desktop: Option<crate::wayland::Desktop>,
    desktop_provider: Option<DesktopProvider>,
    pending_gpu: bool,
    gpu_failed: bool,
    last_gpu: Option<Arc<crate::wayland::GpuFrame>>,
    refinement: Option<(Instant, Instant)>,
    next_frame: Instant,
    diagnostics: crate::performance::Telemetry,
    credit_wait_started: Option<Instant>,
    fallback_previous: Option<BitmapUpdate>,
    fallback_partial: std::collections::VecDeque<BitmapUpdate>,
    fallback_resume: bool,
    refresh: Arc<AtomicU64>,
    known_refresh: u64,
    pending_refresh: u64,
    encode_gate: Arc<EncodeGate>,
}
impl VideoUpdates {
    fn cadence_due(&self, now: Instant) -> bool {
        // Authenticated capture already uses the selected FPS. Ready-time
        // jitter must not lose frames to a second, independently phased clock.
        // A login provider can exist before it returns an actual Desktop.
        self.desktop.is_some() || self.force_idr || now >= self.next_frame
    }
    fn attach_authenticated_desktop(&mut self) {
        if self.desktop.is_none() {
            self.desktop = self
                .desktop_provider
                .as_ref()
                .and_then(|provider| provider());
        }
    }
    fn unchanged(&self, image: &BitmapUpdate) -> bool {
        !self.force_idr
            && self.last_image.as_ref().is_some_and(|previous| {
                previous.width == image.width
                    && previous.height == image.height
                    && previous.format == image.format
                    && previous.stride == image.stride
                    && previous.data == image.data
            })
    }
    #[cfg(test)]
    fn submit(&mut self, data: &[u8], width: u16, height: u16) -> ServerResult<()> {
        self.submit_frame(&EncodedFrame::from(data.to_vec()), width, height)
    }
    fn submit_frame(&mut self, data: &EncodedFrame, width: u16, height: u16) -> ServerResult<()> {
        let mut gfx = self.handle.lock().unwrap();
        gfx.set_max_frames_in_flight(self.rate.lock().unwrap().max_frames_in_flight());
        if !gfx.is_ready() || !gfx.supports_avc420() {
            return Err(server_error("H.264 graphics channel closed"));
        }
        if let Some(id) = self.surface {
            match gfx.get_surface(id) {
                Some(surface) if (surface.width, surface.height) == (width, height) => (),
                _ => self.surface = None,
            }
        }
        let surface = match self.surface {
            Some(id) => id,
            None => {
                // Reactivation can replace this update stream while its old
                // surfaces remain in the connection's graphics context.
                if gfx.surface_ids().next().is_some() {
                    let mut rate = self.rate.lock().unwrap();
                    rate.reset_frames();
                    gfx.set_max_frames_in_flight(rate.max_frames_in_flight());
                    self.credit_wait_started = None;
                }
                // The pinned library's automatic reset uses zero monitors,
                // which conforming clients reject. A private Niri desktop is
                // one primary monitor; ResetGraphics uses inclusive bounds.
                gfx.resize_with_monitors(width, height, primary_monitor(width, height));
                gfx.set_output_dimensions(width, height);
                let id = gfx
                    .create_surface(width, height)
                    .ok_or_else(|| server_error("graphics surface failed"))?;
                gfx.map_surface_to_output(id, 0, 0);
                self.surface = Some(id);
                eprintln!(
                    "RDP graphics using NVENC H.264 {} ({width}x{height})",
                    if data.auxiliary.is_some() {
                        "AVC444"
                    } else {
                        "AVC420"
                    }
                );
                id
            }
        };
        // Region QP/quality is informational (MS-RDPEGFX 2.2.4.4.1), not a
        // decoder knob. Leave the valid conservative estimate in place; actual
        // rate-controlled QP comes from H.264, not this metadata.
        let regions = [Avc420Region::full_frame(width, height, 22)];
        let frame = if let Some(auxiliary) = &data.auxiliary {
            gfx.send_avc444_frame(
                surface,
                &data.main,
                &regions,
                Some(auxiliary),
                Some(&regions),
                self.start.elapsed().as_millis() as u32,
            )
        } else {
            gfx.send_avc420_frame(
                surface,
                &data.main,
                &regions,
                self.start.elapsed().as_millis() as u32,
            )
        };
        // If a reference frame is dropped, the next accepted frame must be an IDR.
        self.force_idr = frame.is_none();
        if let Some(id) = frame {
            self.rate.lock().unwrap().sent(id, Instant::now());
            self.diagnostics.count("delivered_frames", 1);
            self.diagnostics.count(
                "h264_payload_bytes",
                (data.main.len() + data.auxiliary.as_ref().map_or(0, Vec::len)) as u64,
            );
            self.diagnostics.sample("conversion", data.conversion);
            self.diagnostics.sample("encode", data.encode);
            let now = Instant::now();
            self.diagnostics.sample(
                "worker_receipt_to_submit",
                now.saturating_duration_since(data.received_at),
            );
            if let Some(captured_at) = data.captured_at {
                self.diagnostics.sample(
                    "capture_ready_to_submit",
                    now.saturating_duration_since(captured_at),
                );
            } else {
                self.diagnostics.count("unknown_capture_age", 1);
            }
        }
        let channel = gfx
            .channel_id()
            .ok_or_else(|| server_error("graphics channel missing"))?;
        let messages = ironrdp_dvc::encode_dvc_messages(
            channel,
            gfx.drain_output(),
            ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
        )
        .map_err(|e| server_error(format!("graphics framing failed: {e}")))?;
        if !messages.is_empty() {
            if self.diagnostics.enabled() {
                self.diagnostics.count(
                    "graphics_dvc_bytes",
                    messages
                        .iter()
                        .map(|message| {
                            message
                                .encode_unframed_pdu()
                                .map_or(0, |bytes| bytes.len() as u64)
                        })
                        .sum(),
                );
            }
            self.diagnostics.report("video", Instant::now());
            self.sender
                .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                    messages,
                }))
                .map_err(|_| server_error("graphics sender closed"))?;
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl RdpServerDisplayUpdates for VideoUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        if self.ended {
            return Ok(None);
        }
        loop {
            self.attach_authenticated_desktop();
            let refresh = self.refresh.load(Ordering::SeqCst);
            if refresh != self.known_refresh {
                self.known_refresh = refresh;
                self.force_idr = true;
                self.refinement = None;
                self.fallback_previous = None;
                self.fallback_partial.clear();
            }
            if !self.fallback_partial.is_empty() {
                let gfx = self.handle.lock().unwrap();
                if gfx.is_ready() && gfx.supports_avc420() {
                    self.fallback_partial.clear();
                } else {
                    return Ok(self.fallback_partial.pop_front().map(DisplayUpdate::Bitmap));
                }
            }
            if let Some((task, width, height, generation)) = &mut self.pending {
                // Keep the handle in self: cancellation of this future cannot lose an encoded reference frame.
                let result = task.await;
                let (width, height) = (*width, *height);
                let generation = *generation;
                self.pending = None;
                let data = match result {
                    Ok(Ok(data)) => data,
                    Ok(Err(error)) if self.pending_gpu => {
                        eprintln!("RDP GPU capture unavailable; restoring SHM capture: {error}");
                        self.gpu_failed = true;
                        self.pending_gpu = false;
                        self.last_gpu = None;
                        self.encoder.lock().unwrap().take();
                        self.force_idr = true;
                        self.refinement = None;
                        if let Some(desktop) = &self.desktop {
                            desktop.set_gpu_capture(false);
                        }
                        continue;
                    }
                    other => {
                        self.ended = true;
                        return Err(server_error(format!("NVENC encoding failed: {other:?}")));
                    }
                };
                if self.generation.load(Ordering::SeqCst) != generation
                    || self.refresh.load(Ordering::SeqCst) != self.pending_refresh
                    || (self.surface.is_some() && self.suppressed.load(Ordering::Relaxed))
                {
                    // Decoder recovery/minimize can invalidate an in-flight reference frame.
                    self.force_idr = true;
                    continue;
                }
                if let Err(error) = self.submit_frame(&data, width, height) {
                    self.ended = true;
                    return Err(error);
                }
            }
            let timer = self.refinement.map(|(due, _)| due);
            let update = if let Some(due) = timer {
                tokio::select! {
                    biased;
                    update=self.inner.next_update()=>update?,
                    _=tokio::time::sleep_until(due.into())=> {
                        let now=Instant::now();
                        let (_,expires)=self.refinement.unwrap();
                        if now>=expires {self.refinement=None;continue;}
                        if !self.rate.lock().unwrap().permits_refinement(now) || self.suppressed.load(Ordering::Relaxed) {
                            self.refinement=Some((now+Duration::from_millis(250),expires));continue;
                        }
                        self.last_image.clone().map(DisplayUpdate::Bitmap)
                    }
                }
            } else {
                self.inner.next_update().await?
            };
            let Some(update) = update else {
                return Ok(None);
            };
            let image = match update {
                DisplayUpdate::Bitmap(image) => image,
                DisplayUpdate::Resize(size) => {
                    self.encoder.lock().unwrap().take();
                    self.last_image = None;
                    self.fallback_previous = None;
                    self.fallback_partial.clear();
                    self.last_gpu = None;
                    self.refinement = None;
                    if let Some(desktop) = &self.desktop {
                        desktop.set_gpu_capture(false);
                    }
                    self.surface = None;
                    self.force_idr = true;
                    self.rate.lock().unwrap().reset_frames();
                    let mut gfx = self.handle.lock().unwrap();
                    if gfx.is_ready() && gfx.supports_avc420() {
                        gfx.resize_with_monitors(
                            size.width,
                            size.height,
                            primary_monitor(size.width, size.height),
                        );
                    }
                    return Ok(Some(DisplayUpdate::Resize(size)));
                }
                other => return Ok(Some(other)),
            };
            let generation = self.generation.load(Ordering::SeqCst);
            self.attach_authenticated_desktop();
            let refresh = self.refresh.load(Ordering::SeqCst);
            if refresh != self.known_refresh {
                self.known_refresh = refresh;
                self.force_idr = true;
                self.refinement = None;
                self.fallback_previous = None;
                self.fallback_partial.clear();
            }
            if generation != self.known_generation {
                self.surface = None;
                self.force_idr = true;
                self.known_generation = generation;
                self.refinement = None;
                self.last_gpu = None;
                self.encoder.lock().unwrap().take();
                self.fallback_previous = None;
                self.fallback_partial.clear();
                if let Some(desktop) = &self.desktop {
                    desktop.set_gpu_capture(false);
                }
            }
            // mstsc can suppress output during connection setup before it has
            // a surface. Deliver the first graphics frame so initialization
            // cannot deadlock on the client's later un-suppress request.
            if self.surface.is_some() && self.suppressed.load(Ordering::Relaxed) {
                self.force_idr = true;
                self.refinement = None;
                if let Some(desktop) = &self.desktop {
                    desktop.set_gpu_capture(false);
                }
                continue;
            }
            let avc444;
            {
                let mut gfx = self.handle.lock().unwrap();
                let (fps, window) = {
                    let mut rate = self.rate.lock().unwrap();
                    rate.check_stall(Instant::now());
                    (rate.frame_rate(), rate.max_frames_in_flight())
                };
                gfx.set_max_frames_in_flight(window);
                if let Some(desktop) = &self.desktop {
                    desktop.set_video_frame_rate(fps);
                }
                if !gfx.is_ready() || !gfx.supports_avc420() {
                    if self.surface.is_some() {
                        self.ended = true;
                        return Err(server_error("H.264 graphics channel closed"));
                    }
                    if let Some(desktop) = &self.desktop {
                        desktop.set_gpu_capture(false);
                        if desktop.is_gpu_bitmap(&image) {
                            continue;
                        }
                    }
                    if self.suppressed.load(Ordering::Relaxed) && self.fallback_previous.is_some() {
                        self.fallback_resume = true;
                        continue;
                    }
                    if self.fallback_resume {
                        self.fallback_resume = false;
                        self.fallback_previous = None;
                    }
                    // Keep older clients usable, including those with AVC disabled.
                    self.fallback_partial.extend(crate::bitmap_damage(
                        self.fallback_previous.as_ref(),
                        &image,
                    ));
                    self.fallback_previous = Some(image);
                    if let Some(image) = self.fallback_partial.pop_front() {
                        return Ok(Some(DisplayUpdate::Bitmap(image)));
                    }
                    self.diagnostics
                        .count("unchanged_bitmap_fallback_frames", 1);
                    continue;
                }
                if gfx.should_backpressure() {
                    self.diagnostics.count("credit_wait_frames", 1);
                    self.credit_wait_started.get_or_insert(Instant::now());
                    continue;
                }
                if let Some(started) = self.credit_wait_started.take() {
                    self.diagnostics.sample(
                        "graphics_credit_wait",
                        Instant::now().saturating_duration_since(started),
                    );
                }
                avc444 = gfx.supports_avc444()
                    && crate::performance::avc444_requested()
                        .map_err(|e| server_error(e.to_string()))?;
            }
            let now = Instant::now();
            let (bitrate, fps) = {
                let rate = self.rate.lock().unwrap();
                (rate.current(), rate.frame_rate())
            };
            if !self.cadence_due(now) {
                self.diagnostics.count("cadence_skipped_frames", 1);
                continue;
            }
            let gpu = if let Some(desktop) = &self.desktop {
                if !self.gpu_failed
                    && crate::performance::gpu_capture_requested()
                        .map_err(|e| server_error(e.to_string()))?
                {
                    desktop.set_gpu_capture(true);
                }
                if desktop.is_gpu_bitmap(&image) {
                    match desktop.gpu_frame_for(&image).or_else(|| {
                        self.last_gpu.clone().filter(|_| {
                            self.last_image
                                .as_ref()
                                .is_some_and(|last| last.data == image.data)
                        })
                    }) {
                        Some(frame) => Some(frame),
                        None => {
                            self.diagnostics.count("overwritten_gpu_frames", 1);
                            continue;
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            };
            // Continue consuming capture heartbeats so capability recovery,
            // minimize/resume and backpressure still wake on a static desktop.
            // Avoid conversion, GPU upload and wire traffic for identical pixels.
            if self.unchanged(&image) {
                let eligible = self
                    .refinement
                    .is_some_and(|(due, expires)| now >= due && now < expires)
                    && self.rate.lock().unwrap().permits_refinement(now);
                if !eligible {
                    self.diagnostics.count("unchanged_frames", 1);
                    continue;
                }
                self.refinement = None;
                self.diagnostics.count("refinement_frames", 1);
            } else {
                self.refinement = Some((
                    now + Duration::from_millis(750),
                    now + Duration::from_secs(6),
                ));
            }
            let (width, height) = (image.width.get(), image.height.get());
            // Acquire asynchronously before submitting blocking work. Dropped
            // streams may retain an already-running native call; replacements
            // cannot create another context or enqueue more blocking tasks.
            let waiting = Instant::now();
            let permit = self
                .encode_gate
                .0
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| server_error("graphics encoder gate closed"))?;
            if waiting.elapsed() > Duration::from_millis(150) {
                self.diagnostics.count("superseded_encoder_wait_frames", 1);
                continue;
            }
            self.last_image = Some(image.clone());
            self.last_gpu = gpu.clone();
            self.next_frame = now + Duration::from_secs_f64(1.0 / f64::from(fps));
            let encoder = self.encoder.clone();
            let force_idr = self.force_idr;
            self.pending_gpu = gpu.is_some();
            self.pending_refresh = self.known_refresh;
            let captured_at = gpu.as_ref().map(|frame| frame.captured_at).or_else(|| {
                self.desktop
                    .as_ref()
                    .and_then(|desktop| desktop.capture_time_for(&image))
            });
            let received_at = now;
            // Encoder initialization and GPU submission run off the input/network thread.
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut encoder = encoder.lock().unwrap();
                let changed = encoder.as_ref().is_none_or(|encoder| {
                    (encoder.width, encoder.height, encoder.avc444) != (width, height, avc444)
                        || (encoder.gpu_active && gpu.is_none())
                });
                if changed {
                    let software = encoder.as_ref().is_some_and(|encoder| encoder.software);
                    *encoder = Some(Encoder::new_mode(width, height, software, avc444)?);
                }
                let encoder = encoder.as_mut().unwrap();
                let recreated = encoder.set_rate(bitrate, fps)?;
                let mut encoded = if let Some(gpu) = gpu {
                    encoder.encode_gpu(&gpu, force_idr || changed || recreated)
                } else {
                    encoder.encode_views(&image, force_idr || changed || recreated)
                }?;
                encoded.captured_at = captured_at;
                encoded.received_at = received_at;
                Ok(encoded)
            });
            self.pending = Some((task, width, height, generation));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_dvc::DvcProcessor;
    use ironrdp_egfx::pdu::{CapabilitiesV10Flags, GfxPdu};
    use std::{collections::VecDeque, io::Write, process::Command, time::Duration};

    struct Sequence(VecDeque<DisplayUpdate>);
    #[async_trait::async_trait]
    impl RdpServerDisplayUpdates for Sequence {
        async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
            Ok(self.0.pop_front())
        }
    }
    fn image() -> DisplayUpdate {
        sized_image(64, 64)
    }
    fn sized_image(width: u16, height: u16) -> DisplayUpdate {
        DisplayUpdate::Bitmap(
            crate::bitmap(
                &vec![127; usize::from(width) * usize::from(height) * 4],
                width.into(),
                height.into(),
                u32::from(width) * 4,
                false,
                DesktopSize { width, height },
                false,
            )
            .unwrap(),
        )
    }
    fn advertise(handle: &GfxServerHandle, avc: bool) {
        let flags = if avc {
            CapabilitiesV10Flags::empty()
        } else {
            CapabilitiesV10Flags::AVC_DISABLED
        };
        let pdu = GfxPdu::CapabilitiesAdvertise(CapabilitiesAdvertisePdu::from_typed(&[
            CapabilitySet::V10 { flags },
        ]));
        let bytes = ironrdp_core::encode_vec(&pdu).unwrap();
        let mut gfx = handle.lock().unwrap();
        gfx.process(7, &bytes).unwrap();
        assert!(gfx.is_ready());
        assert_eq!(gfx.supports_avc420(), avc);
    }
    fn graphics_pdus(event: ServerEvent) -> Vec<GfxPdu> {
        use ironrdp_dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
        let ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }) = event else {
            panic!("expected graphics messages");
        };
        messages
            .into_iter()
            .map(|message| {
                let bytes = message.encode_unframed_pdu().unwrap();
                let DrdynvcServerPdu::Data(DrdynvcDataPdu::Data(data)) =
                    ironrdp_core::decode(&bytes).unwrap()
                else {
                    panic!("small graphics test PDU must fit one DVC message");
                };
                let payload = data.data();
                assert_eq!(&payload[..2], &[0xe0, 0x04], "uncompressed ZGFX segment");
                ironrdp_core::decode(&payload[2..]).unwrap()
            })
            .collect()
    }
    fn updates(
        frames: Vec<DisplayUpdate>,
    ) -> (
        VideoUpdates,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        let video = Video::default();
        let (_, handle) = video.build_server_with_handle().unwrap();
        handle.lock().unwrap().start(7).unwrap();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let state = video.0.lock().unwrap();
        (
            VideoUpdates {
                inner: Box::new(Sequence(frames.into())),
                handle,
                sender,
                encoder: Arc::new(Mutex::new(None)),
                pending: None,
                surface: None,
                start: Instant::now(),
                force_idr: true,
                last_image: None,
                ended: false,
                generation: state.generation.clone(),
                known_generation: 0,
                suppressed: state.suppressed.clone(),
                rate: state.rate.clone(),
                desktop: None,
                desktop_provider: None,
                pending_gpu: false,
                gpu_failed: false,
                last_gpu: None,
                refinement: None,
                next_frame: Instant::now(),
                diagnostics: crate::performance::Telemetry::from_env(),
                credit_wait_started: None,
                fallback_previous: None,
                fallback_partial: Default::default(),
                fallback_resume: false,
                refresh: state.refresh.clone(),
                known_refresh: state.refresh.load(Ordering::SeqCst),
                pending_refresh: 0,
                encode_gate: state.encode_gate.clone(),
            },
            receiver,
        )
    }
    #[test]
    fn authenticated_capture_cadence_survives_readback_jitter() {
        let (mut stream, _) = updates(vec![]);
        let (desktop, _) = crate::wayland::lifecycle_tests::fake_desktop(DesktopSize {
            width: 64,
            height: 64,
        });
        stream.desktop = Some(desktop);
        stream.force_idr = false;
        let start = Instant::now();
        let period = Duration::from_millis(50);
        stream.next_frame = start;
        let mut delivered = 0;
        // Capture starts at exactly 20 FPS. Alternating readback latency is
        // legal: Ready notifications need not be spaced exactly 50 ms apart.
        for frame in 0..200 {
            let readback = Duration::from_millis(2 + frame % 2);
            let ready = start + period * frame as u32 + readback;
            if stream.cadence_due(ready) {
                delivered += 1;
                stream.next_frame = ready + period;
            }
        }
        assert_eq!(
            delivered, 200,
            "a second receipt-time clock must not halve source-paced capture"
        );
    }
    #[test]
    fn unauthenticated_desktop_provider_keeps_send_cadence() {
        let (mut stream, _) = updates(vec![]);
        stream.force_idr = false;
        let authenticated = Arc::new(Mutex::new(None));
        let current = authenticated.clone();
        stream.desktop_provider = Some(Arc::new(move || current.lock().unwrap().clone()));
        let now = Instant::now();
        stream.next_frame = now + Duration::from_millis(50);
        stream.attach_authenticated_desktop();
        assert!(stream.desktop.is_none());
        assert!(!stream.cadence_due(now));
        assert!(stream.cadence_due(stream.next_frame));
        stream.force_idr = true;
        assert!(
            stream.cadence_due(now),
            "explicit refresh remains immediate"
        );
        stream.force_idr = false;
        let (desktop, _) = crate::wayland::lifecycle_tests::fake_desktop(DesktopSize {
            width: 64,
            height: 64,
        });
        *authenticated.lock().unwrap() = Some(desktop);
        stream.attach_authenticated_desktop();
        assert!(stream.desktop.is_some());
        assert!(stream.cadence_due(now));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn failed_or_panicked_encoding_task_ends_without_repolling() {
        for panics in [false, true] {
            let (mut stream, _) = updates(vec![]);
            let task = tokio::spawn(async move {
                assert!(!panics, "simulated encoding task panic");
                Err(anyhow::anyhow!("simulated encoder failure"))
            });
            stream.pending = Some((task, 64, 64, 0));
            assert!(stream.next_update().await.is_err());
            assert!(stream.pending.is_none());
            assert!(stream.ended);
            assert!(stream.next_update().await.unwrap().is_none());
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_update_future_retains_pending_encoding_task() {
        let (mut stream, _) = updates(vec![]);
        let (release, gate) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            gate.await.unwrap();
            Err(anyhow::anyhow!("simulated encoder failure"))
        });
        stream.pending = Some((task, 64, 64, 0));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), stream.next_update())
                .await
                .is_err()
        );
        assert!(stream.pending.is_some());
        release.send(()).unwrap();
        assert!(stream.next_update().await.is_err());
        assert!(stream.next_update().await.unwrap().is_none());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn login_stream_can_use_graphics_wrapper_without_a_wayland_desktop() {
        let mut video = Video::default();
        let (_, handle) = video.build_server_with_handle().unwrap();
        handle.lock().unwrap().start(7).unwrap();
        advertise(&handle, false);
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        video.set_sender(sender);
        let mut stream = video
            .wrap_updates(Box::new(Sequence(vec![image()].into())))
            .unwrap();
        assert!(matches!(
            stream.next_update().await.unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clients_without_avc_receive_bitmaps_without_encoder_initialization() {
        let (mut stream, mut events) = updates(vec![image()]);
        advertise(&stream.handle, false);
        assert!(matches!(
            stream.next_update().await.unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        assert!(stream.encoder.lock().unwrap().is_none());
        assert!(stream.pending.is_none());
        assert!(events.try_recv().is_err());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn nvenc_bitmap_fallback_suppresses_idle_and_preserves_sparse_changes_and_refresh() {
        let DisplayUpdate::Bitmap(initial) = sized_image(256, 256) else {
            unreachable!()
        };
        let mut changed = initial.clone();
        let mut pixels = changed.data.to_vec();
        pixels[4 * (130 * 256 + 130)] = 128;
        changed.data = pixels.into();
        let (mut stream, mut events) = updates(vec![
            DisplayUpdate::Bitmap(initial.clone()),
            DisplayUpdate::Bitmap(initial.clone()),
            DisplayUpdate::Bitmap(changed.clone()),
            DisplayUpdate::Bitmap(changed.clone()),
        ]);
        advertise(&stream.handle, false);
        let Some(DisplayUpdate::Bitmap(first)) = stream.next_update().await.unwrap() else {
            panic!("initial full bitmap")
        };
        assert_eq!(first.width.get(), 256);
        let Some(DisplayUpdate::Bitmap(damage)) = stream.next_update().await.unwrap() else {
            panic!("changed partial bitmap")
        };
        assert_eq!(
            (damage.x, damage.y, damage.width.get(), damage.height.get()),
            (128, 128, 64, 64)
        );
        assert!(
            stream.next_update().await.unwrap().is_none(),
            "duplicate heartbeats must produce no additional bitmaps"
        );
        assert!(stream.encoder.lock().unwrap().is_none());
        assert!(events.try_recv().is_err());
        stream.refresh.fetch_add(1, Ordering::SeqCst);
        stream.inner = Box::new(Sequence(vec![DisplayUpdate::Bitmap(changed)].into()));
        let Some(DisplayUpdate::Bitmap(full)) = stream.next_update().await.unwrap() else {
            panic!("explicit refresh must replay full bitmap")
        };
        assert_eq!(
            (full.x, full.y, full.width.get(), full.height.get()),
            (0, 0, 256, 256)
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn explicit_refresh_discards_pending_reference_and_sends_idr_without_surface_recreation()
    {
        use ironrdp_egfx::pdu::Avc420BitmapStream;
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        let DisplayUpdate::Bitmap(image) = image() else {
            unreachable!()
        };
        let fps = stream.rate.lock().unwrap().frame_rate();
        let mut encoder = Encoder::new_rate(64, 64, true, false, fps, 12_000_000).unwrap();
        let first = encoder.encode_views(&image, true).unwrap();
        let pointer = encoder.ptr;
        stream.submit_frame(&first, 64, 64).unwrap();
        let surface = stream.surface;
        events.try_recv().unwrap();
        *stream.encoder.lock().unwrap() = Some(encoder);
        stream.last_image = Some(image.clone());
        let (release, wait) = tokio::sync::oneshot::channel();
        stream.pending = Some((
            tokio::spawn(async move {
                wait.await.unwrap();
                Ok(vec![0, 0, 0, 1, 0x41].into())
            }),
            64,
            64,
            stream.known_generation,
        ));
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        stream.inner = Box::new(Channel(receiver));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), stream.next_update())
                .await
                .is_err()
        );
        stream.refresh.fetch_add(1, Ordering::SeqCst);
        release.send(()).unwrap();
        sender.send(DisplayUpdate::Bitmap(image)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next_update())
                .await
                .is_err()
        );
        assert_eq!(stream.surface, surface);
        assert_eq!(
            stream.encoder.lock().unwrap().as_ref().unwrap().ptr,
            pointer
        );
        let pdus = graphics_pdus(events.try_recv().unwrap());
        assert!(!pdus.iter().any(|pdu| matches!(
            pdu,
            GfxPdu::CreateSurface(_) | GfxPdu::DeleteSurface(_) | GfxPdu::ResetGraphics(_)
        )));
        let stream = pdus
            .iter()
            .find_map(|pdu| {
                if let GfxPdu::WireToSurface1(wire) = pdu {
                    Some(ironrdp_core::decode::<Avc420BitmapStream<'_>>(&wire.bitmap_data).unwrap())
                } else {
                    None
                }
            })
            .unwrap();
        assert!(
            stream
                .data
                .windows(4)
                .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5),
            "refresh must deliver an IDR instead of the skipped pending P frame"
        );
        assert!(events.try_recv().is_err());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_an_old_update_stream_cannot_start_concurrent_native_work() {
        let (mut old, _) = updates(vec![]);
        let gate = old.encode_gate.clone();
        let permit = gate.0.clone().acquire_owned().await.unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        old.pending = Some((
            tokio::spawn(async move {
                let _permit = permit;
                wait.await.unwrap();
                Err(anyhow::anyhow!("old native call completed"))
            }),
            64,
            64,
            0,
        ));
        drop(old);
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        stream.encode_gate = gate.clone();
        let fps = stream.rate.lock().unwrap().frame_rate();
        *stream.encoder.lock().unwrap() =
            Some(Encoder::new_rate(64, 64, true, false, fps, 12_000_000).unwrap());
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        stream.inner = Box::new(Channel(receiver));
        sender.send(image()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stream.next_update())
                .await
                .is_err()
        );
        assert!(
            stream.pending.is_none(),
            "replacement must wait asynchronously before submitting native work"
        );
        assert!(events.try_recv().is_err());
        assert_eq!(gate.0.available_permits(), 0);
        release.send(()).unwrap();
        tokio::task::yield_now().await;
        sender.send(image()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next_update())
                .await
                .is_err()
        );
        assert!(events.try_recv().is_ok());
        assert_eq!(gate.0.available_permits(), 1);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn returning_from_gpu_capture_recreates_shm_encoder_and_sends_an_idr() {
        use ironrdp_egfx::pdu::Avc420BitmapStream;
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        let fps = stream.rate.lock().unwrap().frame_rate();
        let mut encoder = Encoder::new_rate(64, 64, true, false, fps, 12_000_000).unwrap();
        let DisplayUpdate::Bitmap(frame) = image() else {
            unreachable!()
        };
        let first = encoder.encode_views(&frame, true).unwrap();
        stream.submit_frame(&first, 64, 64).unwrap();
        events.try_recv().unwrap();
        // The opt-in C fixture validates that a real CUDA context rejects CPU
        // input. Here exercise the wrapper's transition with a software context
        // so the regression remains runnable without NVIDIA hardware.
        encoder.gpu_active = true;
        *stream.encoder.lock().unwrap() = Some(encoder);
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        stream.inner = Box::new(Channel(receiver));
        let mut changed = frame;
        changed.data = vec![128; changed.data.len()].into();
        sender.send(DisplayUpdate::Bitmap(changed)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next_update())
                .await
                .is_err()
        );
        assert!(!stream.encoder.lock().unwrap().as_ref().unwrap().gpu_active);
        let pdus = graphics_pdus(events.try_recv().unwrap());
        let stream = pdus
            .iter()
            .find_map(|pdu| {
                if let GfxPdu::WireToSurface1(wire) = pdu {
                    Some(ironrdp_core::decode::<Avc420BitmapStream<'_>>(&wire.bitmap_data).unwrap())
                } else {
                    None
                }
            })
            .unwrap();
        assert!(
            stream
                .data
                .windows(4)
                .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5)
        );
    }
    #[test]
    fn unchanged_desktop_skips_encoding_but_recovery_and_resize_require_a_frame() {
        let (mut stream, _) = updates(vec![]);
        let DisplayUpdate::Bitmap(image) = image() else {
            unreachable!()
        };
        stream.last_image = Some(image.clone());
        assert!(
            !stream.unchanged(&image),
            "initial/recovery IDR must not be skipped"
        );
        stream.force_idr = false;
        assert!(stream.unchanged(&image));
        let mut changed = image.clone();
        changed.data = vec![128; changed.data.len()].into();
        assert!(!stream.unchanged(&changed));
        let DisplayUpdate::Bitmap(resized) = sized_image(32, 128) else {
            unreachable!()
        };
        assert!(!stream.unchanged(&resized));
        stream.force_idr = true;
        assert!(
            !stream.unchanged(&image),
            "dropped reference or resume needs a fresh IDR"
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn resize_releases_encoder_and_old_graphics_surfaces() {
        let size = DesktopSize {
            width: 63,
            height: 61,
        };
        let (mut stream, mut events) = updates(vec![DisplayUpdate::Resize(size)]);
        advertise(&stream.handle, true);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let old_surface = stream.surface.unwrap();
        events.try_recv().unwrap();
        *stream.encoder.lock().unwrap() = Some(Encoder::new(64, 64, true).unwrap());
        assert!(
            matches!(stream.next_update().await.unwrap(), Some(DisplayUpdate::Resize(actual))
            if actual.width == size.width && actual.height == size.height)
        );
        assert!(stream.encoder.lock().unwrap().is_none());
        assert!(stream.surface.is_none());
        assert!(stream.force_idr);
        assert!(
            stream
                .handle
                .lock()
                .unwrap()
                .get_surface(old_surface)
                .is_none()
        );
        stream
            .submit(&[0, 0, 0, 1, 0x65], size.width, size.height)
            .unwrap();
        let gfx = stream.handle.lock().unwrap();
        let surface = gfx.get_surface(stream.surface.unwrap()).unwrap();
        assert_eq!((surface.width, surface.height), (size.width, size.height));
        assert!(surface.is_mapped);
        assert!(events.try_recv().is_ok());
    }
    #[test]
    fn changed_dimensions_and_replaced_stream_recreate_graphics_surface() {
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let old_surface = stream.surface.unwrap();
        events.try_recv().unwrap();
        for replaced_stream in [false, true] {
            if replaced_stream {
                stream.surface = None;
            }
            stream.submit(&[0, 0, 0, 1, 0x65], 63, 61).unwrap();
            let gfx = stream.handle.lock().unwrap();
            assert!(gfx.get_surface(old_surface).is_none());
            assert_eq!(gfx.surface_ids().count(), 1);
            let surface = gfx.get_surface(stream.surface.unwrap()).unwrap();
            assert_eq!((surface.width, surface.height), (63, 61));
            assert!(surface.is_mapped);
            let pdus = graphics_pdus(events.try_recv().unwrap());
            let deleted = pdus
                .iter()
                .position(|pdu| matches!(pdu, GfxPdu::DeleteSurface(_)))
                .unwrap();
            let reset = pdus
                .iter()
                .position(|pdu| {
                    matches!(pdu,
                GfxPdu::ResetGraphics(reset) if (reset.width, reset.height) == (63, 61))
                })
                .unwrap();
            let created = pdus
                .iter()
                .position(|pdu| {
                    matches!(pdu,
                GfxPdu::CreateSurface(surface) if (surface.width, surface.height) == (63, 61))
                })
                .unwrap();
            let mapped = pdus
                .iter()
                .position(|pdu| matches!(pdu, GfxPdu::MapSurfaceToOutput(_)))
                .unwrap();
            assert!(deleted < reset && reset < created && created < mapped);
        }
    }
    #[test]
    fn replacing_an_update_stream_discards_old_ack_samples_and_retains_audio_pressure() {
        use ironrdp_egfx::pdu::{FrameAcknowledgePdu, QueueDepth};
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        let now = Instant::now();
        *stream.rate.lock().unwrap() =
            crate::performance::AdaptiveBitrate::new(12_000_000, now - Duration::from_secs(10));
        stream
            .rate
            .lock()
            .unwrap()
            .audio_congested(now - Duration::from_secs(1));
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let old_id = graphics_pdus(events.try_recv().unwrap())
            .into_iter()
            .find_map(|pdu| {
                if let GfxPdu::StartFrame(frame) = pdu {
                    Some(frame.frame_id)
                } else {
                    None
                }
            })
            .unwrap();
        stream
            .rate
            .lock()
            .unwrap()
            .sent(old_id, now - Duration::from_secs(5));
        stream.surface = None;
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
            frame_id: old_id,
            queue_depth: QueueDepth::AvailableBytes(1_000_000),
            total_frames_decoded: 1,
        });
        stream
            .handle
            .lock()
            .unwrap()
            .process(7, &ironrdp_core::encode_vec(&ack).unwrap())
            .unwrap();
        let mut rate = stream.rate.lock().unwrap();
        rate.check_stall(now + Duration::from_millis(200));
        assert_eq!(
            rate.current(),
            9_000_000,
            "old-surface ACK and age cannot penalize the replacement"
        );
        let mut inherited = crate::performance::AdaptiveBitrate::new(12_000_000, now);
        inherited.inherit_audio_congestion(&rate, now);
        assert_eq!(
            inherited.current(),
            9_000_000,
            "graphics reset must retain recent audio pressure"
        );
    }
    #[test]
    fn dimension_changes_recreate_encoder_and_odd_sizes_are_padded() {
        let mut encoder = None;
        for (width, height) in [(64, 64), (63, 61), (80, 48), (1, 1), (1, 17), (17, 1)] {
            let DisplayUpdate::Bitmap(image) = sized_image(width, height) else {
                unreachable!()
            };
            let encoded =
                Encoder::encode_frame(&mut encoder, &image, false, |w, h| Encoder::new(w, h, true))
                    .unwrap();
            let context = encoder.as_ref().unwrap();
            assert_eq!((context.width, context.height), (width, height));
            assert!(
                encoded
                    .windows(4)
                    .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5),
                "each new size must begin with an IDR"
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("resized.h264");
            std::fs::write(&path, encoded).unwrap();
            let decoded = Command::new("ffmpeg")
                .args(["-v", "error", "-threads", "1", "-i"])
                .arg(path)
                .args([
                    "-f", "rawvideo", "-pix_fmt", "bgra", "-threads", "1", "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            assert_eq!(
                decoded.stdout.len(),
                usize::from(width.div_ceil(16) * 16) * usize::from(height.div_ceil(16) * 16) * 4
            );
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn capability_readvertise_discards_inflight_reference_and_recreates_surface() {
        let (mut stream, mut events) = updates(vec![image()]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let previous_surface = stream.surface.unwrap();
        assert!(!stream.force_idr);
        assert!(events.try_recv().is_ok());
        let pending_generation = stream.known_generation;
        stream.pending = Some((
            tokio::spawn(async { Ok(vec![0, 0, 0, 1, 0x41].into()) }),
            64,
            64,
            pending_generation,
        ));
        // Recovery can renegotiate codecs as well as discard client surfaces.
        advertise(&stream.handle, false);
        assert!(
            stream
                .handle
                .lock()
                .unwrap()
                .get_surface(previous_surface)
                .is_none()
        );
        assert!(matches!(
            stream.next_update().await.unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        assert!(stream.pending.is_none());
        assert!(stream.surface.is_none());
        assert!(stream.force_idr, "decoder recovery must require a new IDR");
        assert!(
            events.try_recv().is_err(),
            "stale P frame must never reach recovered client"
        );
        // An IDR from the recovered encoder can create and map a fresh surface.
        advertise(&stream.handle, true);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        assert!(
            stream
                .handle
                .lock()
                .unwrap()
                .get_surface(stream.surface.unwrap())
                .unwrap()
                .is_mapped
        );
        assert!(events.try_recv().is_ok());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn handshake_suppression_does_not_drop_first_graphics_surface() {
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        stream.suppressed.store(true, Ordering::Relaxed);
        stream.pending = Some((
            tokio::spawn(async { Ok(vec![0, 0, 0, 1, 0x65].into()) }),
            64,
            64,
            stream.known_generation,
        ));
        assert!(stream.next_update().await.unwrap().is_none());
        assert!(stream.surface.is_some());
        assert!(events.try_recv().is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn minimized_output_drops_pending_reference_and_requires_idr_on_resume() {
        let (mut stream, mut events) = updates(vec![image()]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let surface = stream.surface;
        assert!(events.try_recv().is_ok());
        assert!(!stream.force_idr);
        stream.pending = Some((
            tokio::spawn(async { Ok(vec![0, 0, 0, 1, 0x41].into()) }),
            64,
            64,
            stream.known_generation,
        ));
        stream.suppressed.store(true, Ordering::Relaxed);
        assert!(stream.next_update().await.unwrap().is_none());
        assert!(stream.pending.is_none());
        assert!(stream.encoder.lock().unwrap().is_none());
        assert!(stream.force_idr);
        assert_eq!(stream.surface, surface);
        assert!(events.try_recv().is_err());
        stream.suppressed.store(false, Ordering::Relaxed);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        assert!(events.try_recv().is_ok());
    }
    #[test]
    fn invalid_video_dimensions_fail_before_ffi() {
        for (w, h) in [(0, 2), (2, 0), (2, 4097), (4098, 2)] {
            assert!(Encoder::new(w, h, true).is_err());
        }
    }
    #[test]
    fn avc444_sends_both_views_in_order_with_logical_region_masks() {
        use ironrdp_egfx::pdu::{Avc444BitmapStream, Codec1Type, Encoding};
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        assert!(stream.handle.lock().unwrap().supports_avc444());
        let mut encoder = Encoder::new_mode(63, 61, true, true).unwrap();
        let DisplayUpdate::Bitmap(image) = sized_image(63, 61) else {
            unreachable!()
        };
        let encoded = encoder.encode_views(&image, true).unwrap();
        assert!(encoded.auxiliary.is_some());
        stream.submit_frame(&encoded, 63, 61).unwrap();
        let wires: Vec<_> = graphics_pdus(events.try_recv().unwrap())
            .into_iter()
            .filter_map(|pdu| {
                if let GfxPdu::WireToSurface1(wire) = pdu {
                    Some(wire)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(wires.len(), 1);
        assert_eq!(wires[0].codec_id, Codec1Type::Avc444);
        let views: Avc444BitmapStream<'_> = ironrdp_core::decode(&wires[0].bitmap_data).unwrap();
        assert_eq!(views.encoding, Encoding::LUMA_AND_CHROMA);
        assert_eq!(views.stream1.data, &encoded.main);
        let second = views.stream2.unwrap();
        assert_eq!(second.data, encoded.auxiliary.as_ref().unwrap());
        for view in [views.stream1, second] {
            assert_eq!(view.rectangles.len(), 1);
            let region = &view.rectangles[0];
            assert_eq!(
                (region.left, region.top, region.right, region.bottom),
                (0, 0, 63, 61)
            );
        }
    }
    #[test]
    fn cadence_changes_recreate_encoder_with_an_idr_and_matching_frame_budget() {
        let mut encoder = Encoder::new_rate(64, 64, true, false, 60, 12_000_000).unwrap();
        let DisplayUpdate::Bitmap(image) = image() else {
            unreachable!()
        };
        encoder.encode_views(&image, true).unwrap();
        assert!(encoder.set_rate(3_000_000, 15).unwrap());
        assert_eq!((encoder.fps, encoder.bitrate), (15, 3_000_000));
        let encoded = encoder.encode_views(&image, true).unwrap();
        assert!(
            encoded
                .main
                .windows(4)
                .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5)
        );
        assert!(
            !encoder.set_rate(2_000_000, 15).unwrap(),
            "bitrate-only changes retain reference context"
        );
    }
    struct Channel(tokio::sync::mpsc::Receiver<DisplayUpdate>);
    #[async_trait::async_trait]
    impl RdpServerDisplayUpdates for Channel {
        async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
            Ok(self.0.recv().await)
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn a_settled_image_refines_once_without_recreating_encoder_or_repeating_idrs() {
        use ironrdp_egfx::pdu::{FrameAcknowledgePdu, QueueDepth};
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        let DisplayUpdate::Bitmap(image) = image() else {
            unreachable!()
        };
        let fps = stream.rate.lock().unwrap().frame_rate();
        let mut encoder = Encoder::new_rate(64, 64, true, false, fps, 12_000_000).unwrap();
        let first = encoder.encode_views(&image, true).unwrap();
        let ptr = encoder.ptr;
        *stream.encoder.lock().unwrap() = Some(encoder);
        stream.submit_frame(&first, 64, 64).unwrap();
        let ack = |stream: &mut VideoUpdates, event| {
            let id = graphics_pdus(event)
                .into_iter()
                .find_map(|pdu| {
                    if let GfxPdu::StartFrame(frame) = pdu {
                        Some(frame.frame_id)
                    } else {
                        None
                    }
                })
                .unwrap();
            let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
                frame_id: id,
                queue_depth: QueueDepth::AvailableBytes(0),
                total_frames_decoded: id + 1,
            });
            stream
                .handle
                .lock()
                .unwrap()
                .process(7, &ironrdp_core::encode_vec(&ack).unwrap())
                .unwrap();
        };
        ack(&mut stream, events.try_recv().unwrap());
        stream.last_image = Some(image.clone());
        stream.refinement = Some((
            Instant::now() + Duration::from_millis(10),
            Instant::now() + Duration::from_secs(2),
        ));
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        stream.inner = Box::new(Channel(receiver));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next_update())
                .await
                .is_err()
        );
        let refined = events
            .try_recv()
            .expect("exactly one refinement reaches the client");
        assert!(events.try_recv().is_err());
        assert!(stream.refinement.is_none());
        assert_eq!(stream.encoder.lock().unwrap().as_ref().unwrap().ptr, ptr);
        ack(&mut stream, refined);
        for _ in 0..8 {
            sender
                .send(DisplayUpdate::Bitmap(image.clone()))
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next_update())
                .await
                .is_err()
        );
        assert!(
            events.try_recv().is_err(),
            "ordinary idle heartbeats cannot repeatedly refine"
        );
        assert_eq!(stream.encoder.lock().unwrap().as_ref().unwrap().ptr, ptr);
    }
    #[test]
    fn nonaligned_desktop_encodes_edge_padding_without_stretching() {
        let mut encoder = Encoder::new(62, 62, true).unwrap();
        let mut pixels = vec![0; 62 * 62 * 4];
        for y in 0..62 {
            for x in 0..62 {
                let offset = (y * 62 + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(&[
                    if y < 32 { 20 } else { 220 },
                    40,
                    if x < 32 { 20 } else { 220 },
                    255,
                ]);
            }
        }
        let image = crate::bitmap(
            &pixels,
            62,
            62,
            248,
            false,
            DesktopSize {
                width: 62,
                height: 62,
            },
            false,
        )
        .unwrap();
        let encoded = encoder.encode(&image, true).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("padded.h264");
        std::fs::write(&path, encoded).unwrap();
        let decoded = Command::new("ffmpeg")
            .args(["-v", "error", "-threads", "1", "-i"])
            .arg(path)
            .args([
                "-f", "rawvideo", "-pix_fmt", "bgra", "-threads", "1", "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert_eq!(
            decoded.stdout.len(),
            64 * 64 * 4,
            "AVC420 coded dimensions must be aligned to 16 without SPS cropping"
        );
        let pixel = |x: usize, y: usize| &decoded.stdout[(y * 64 + x) * 4..][..4];
        assert!(
            pixel(32, 32)[0] > 180 && pixel(32, 32)[2] > 180,
            "padding must preserve the source transition at pixel 32, not stretch it"
        );
        for (x, y) in [(62, 48), (63, 48), (48, 62), (48, 63), (63, 63)] {
            let padded = pixel(x, y);
            let source_edge = pixel(x.min(61), y.min(61));
            for channel in 0..3 {
                assert!(
                    padded[channel].abs_diff(source_edge[channel]) <= 8,
                    "padded edges must repeat the final source row/column"
                );
            }
        }
    }
    #[test]
    fn avc420_signals_and_encodes_full_range_bt709() {
        let mut encoder = Encoder::new(64, 64, true).unwrap();
        let mut pixels = vec![0; 64 * 64 * 4];
        for y in 0..64 {
            for x in 0..64 {
                let color = match (x < 32, y < 32) {
                    (true, true) => [0, 0, 255, 255],
                    (false, true) => [0, 255, 0, 255],
                    (true, false) => [0, 0, 0, 255],
                    (false, false) => [255, 255, 255, 255],
                };
                let offset = (y * 64 + x) * 4;
                pixels[offset..offset + 4].copy_from_slice(&color);
            }
        }
        let image = crate::bitmap(
            &pixels,
            64,
            64,
            256,
            false,
            DesktopSize {
                width: 64,
                height: 64,
            },
            false,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full-range-bt709.h264");
        std::fs::write(&path, encoder.encode(&image, true).unwrap()).unwrap();
        let metadata = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=color_range,color_space",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            metadata.status.success(),
            "{}",
            String::from_utf8_lossy(&metadata.stderr)
        );
        let metadata = String::from_utf8(metadata.stdout).unwrap();
        assert!(metadata.contains("\"color_range\": \"pc\""), "{metadata}");
        assert!(
            metadata.contains("\"color_space\": \"bt709\""),
            "{metadata}"
        );

        // Pin both input and output range to full so FFmpeg cannot hide a
        // limited-range encoding mistake by expanding decoded samples.
        let yuv = Command::new("ffmpeg")
            .args(["-v", "error", "-threads", "1", "-i"])
            .arg(&path)
            .args([
                "-vf",
                "scale=in_range=full:out_range=full",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "-threads",
                "1",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            yuv.status.success(),
            "{}",
            String::from_utf8_lossy(&yuv.stderr)
        );
        assert_eq!(yuv.stdout.len(), 64 * 64 * 3 / 2);
        let luma = |x: usize, y: usize| yuv.stdout[y * 64 + x];
        assert!(luma(16, 48) <= 4, "black must reach full-range zero");
        assert!(luma(48, 48) >= 251, "white must reach full-range 255");
        assert!(
            luma(16, 16).abs_diff(54) <= 4,
            "BT709 red luma must be about 54"
        );
        assert!(
            luma(48, 16).abs_diff(182) <= 4,
            "BT709 green luma must be about 182"
        );
        let red_chroma = (16 / 2) * 32 + 16 / 2;
        assert!(yuv.stdout[64 * 64 + red_chroma].abs_diff(99) <= 4);
        assert!(yuv.stdout[64 * 64 + 32 * 32 + red_chroma] >= 251);

        let rgb = Command::new("ffmpeg")
            .args(["-v", "error", "-threads", "1", "-i"])
            .arg(&path)
            .args([
                "-f", "rawvideo", "-pix_fmt", "bgra", "-threads", "1", "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            rgb.status.success(),
            "{}",
            String::from_utf8_lossy(&rgb.stderr)
        );
        assert_eq!(rgb.stdout.len(), pixels.len());
        for (x, y) in [(16, 16), (48, 16), (16, 48), (48, 48)] {
            let offset = (y * 64 + x) * 4;
            for channel in 0..3 {
                assert!(
                    rgb.stdout[offset + channel].abs_diff(pixels[offset + channel]) <= 8,
                    "saturated colors and grayscale endpoints must survive decoding"
                );
            }
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn graphics_metadata_crops_padding_to_logical_desktop_bounds() {
        use ironrdp_dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
        use ironrdp_egfx::pdu::Avc420BitmapStream;
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        let encoded = [0, 0, 0, 1, 0x65];
        stream.submit(&encoded, 62, 62).unwrap();
        let event = events.try_recv().unwrap();
        let ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }) = event else {
            panic!("expected graphics messages")
        };
        let mut checked = 0;
        for message in messages {
            let bytes = message.encode_unframed_pdu().unwrap();
            let pdu: DrdynvcServerPdu = ironrdp_core::decode(&bytes).unwrap();
            let DrdynvcServerPdu::Data(DrdynvcDataPdu::Data(pdu)) = pdu else {
                panic!("small test graphics PDU must fit in one DVC message")
            };
            // The factory uses the upstream uncompressed ZGFX mode.
            assert_eq!(&pdu.data()[..2], &[0xe0, 0x04]);
            let gfx: GfxPdu = ironrdp_core::decode(&pdu.data()[2..]).unwrap();
            match gfx {
                GfxPdu::ResetGraphics(pdu) => {
                    assert_eq!((pdu.width, pdu.height), (62, 62));
                    assert_eq!(pdu.monitors, primary_monitor(62, 62));
                    checked += 1;
                }
                GfxPdu::CreateSurface(pdu) => {
                    assert_eq!((pdu.width, pdu.height), (62, 62));
                    checked += 1;
                }
                GfxPdu::WireToSurface1(pdu) => {
                    let rect = pdu.destination_rectangle;
                    assert_eq!(
                        (rect.left, rect.top, rect.right, rect.bottom),
                        (0, 0, 62, 62)
                    );
                    let bitmap: Avc420BitmapStream<'_> =
                        ironrdp_core::decode(&pdu.bitmap_data).unwrap();
                    assert_eq!(bitmap.rectangles.len(), 1);
                    let rect = &bitmap.rectangles[0];
                    assert_eq!(
                        (rect.left, rect.top, rect.right, rect.bottom),
                        (0, 0, 62, 62)
                    );
                    assert_eq!(bitmap.data, &encoded);
                    checked += 1;
                }
                _ => {}
            }
        }
        assert_eq!(checked, 3);
    }
    #[test]
    fn graphics_ack_feedback_changes_rate_and_new_connections_reset_it() {
        use ironrdp_egfx::pdu::{FrameAcknowledgePdu, QueueDepth};
        let (mut stream, mut events) = updates(vec![]);
        advertise(&stream.handle, true);
        let now = Instant::now();
        *stream.rate.lock().unwrap() =
            crate::performance::AdaptiveBitrate::new(12_000_000, now - Duration::from_secs(2));
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let id = graphics_pdus(events.try_recv().unwrap())
            .into_iter()
            .find_map(|pdu| {
                if let GfxPdu::StartFrame(frame) = pdu {
                    Some(frame.frame_id)
                } else {
                    None
                }
            })
            .unwrap();
        stream
            .rate
            .lock()
            .unwrap()
            .sent(id, now - Duration::from_secs(1));
        let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
            queue_depth: QueueDepth::AvailableBytes(1_000_000),
            frame_id: id,
            total_frames_decoded: 1,
        });
        stream
            .handle
            .lock()
            .unwrap()
            .process(7, &ironrdp_core::encode_vec(&ack).unwrap())
            .unwrap();
        assert_eq!(stream.rate.lock().unwrap().current(), 9_000_000);
        let video = Video::default();
        video.0.lock().unwrap().rate = stream.rate.clone();
        video.build_server_with_handle().unwrap();
        let new_rate = video.0.lock().unwrap().rate.clone();
        assert!(!Arc::ptr_eq(&new_rate, &stream.rate));
        assert_eq!(new_rate.lock().unwrap().current(), 12_000_000);
    }
    #[test]
    fn sound_pressure_survives_graphics_initialization_and_follows_the_active_context() {
        let video = Video::default();
        let now = Instant::now();
        *video.0.lock().unwrap().rate.lock().unwrap() =
            crate::performance::AdaptiveBitrate::new(12_000_000, now - Duration::from_secs(2));
        let sound = video.audio_feedback();
        sound.report_congestion(now);
        let previous = video.0.lock().unwrap().rate.clone();
        assert_eq!(previous.lock().unwrap().current(), 9_000_000);
        video.build_server_with_handle().unwrap();
        let current = video.0.lock().unwrap().rate.clone();
        assert!(!Arc::ptr_eq(&previous, &current));
        assert_eq!(current.lock().unwrap().current(), 9_000_000);
        sound
            .clone()
            .report_congestion(now + Duration::from_millis(500));
        assert_eq!(current.lock().unwrap().current(), 6_750_000);
        assert_eq!(previous.lock().unwrap().current(), 9_000_000);
    }
    #[test]
    fn old_sound_handles_cannot_report_or_clear_pressure_after_reconnect() {
        let video = Video::default();
        let now = Instant::now();
        *video.0.lock().unwrap().rate.lock().unwrap() =
            crate::performance::AdaptiveBitrate::new(12_000_000, now - Duration::from_secs(2));
        let old = video.audio_feedback();
        old.report_congestion(now);
        assert_eq!(
            video.0.lock().unwrap().rate.lock().unwrap().current(),
            9_000_000
        );
        let current = video.audio_feedback();
        video.build_server_with_handle().unwrap();
        let rate = video.0.lock().unwrap().rate.clone();
        assert_eq!(rate.lock().unwrap().current(), 12_000_000);
        old.report_congestion(now + Duration::from_secs(1));
        assert_eq!(rate.lock().unwrap().current(), 12_000_000);
        current.report_congestion(now + Duration::from_secs(2));
        assert_eq!(rate.lock().unwrap().current(), 9_000_000);
        old.clear();
        video.build_server_with_handle().unwrap();
        assert_eq!(
            video.0.lock().unwrap().rate.lock().unwrap().current(),
            9_000_000,
            "stale cleanup must leave current sound pressure intact"
        );
        current.clear();
        current.clear();
        video.build_server_with_handle().unwrap();
        assert_eq!(
            video.0.lock().unwrap().rate.lock().unwrap().current(),
            12_000_000
        );
    }
    #[test]
    fn rate_changes_keep_a_decodable_stream() {
        let mut encoder = Encoder::new(64, 64, true).unwrap();
        let DisplayUpdate::Bitmap(mut image) = image() else {
            unreachable!()
        };
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("adaptive.h264");
        let mut file = std::fs::File::create(&stream).unwrap();
        for (index, rate) in [12_000_000, 6_000_000, 1_000_000, 3_000_000, 12_000_000]
            .into_iter()
            .enumerate()
        {
            encoder.set_bitrate(rate).unwrap();
            image.data = vec![30 + index as u8 * 40; 64 * 64 * 4].into();
            file.write_all(&encoder.encode(&image, index == 0).unwrap())
                .unwrap();
        }
        assert!(encoder.set_bitrate(999_999).is_err());
        assert!(encoder.set_bitrate(100_000_001).is_err());
        drop(file);
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-threads", "1", "-i"])
            .arg(stream)
            .args([
                "-f", "rawvideo", "-pix_fmt", "bgra", "-threads", "1", "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "decoder reported reference corruption"
        );
        assert_eq!(output.stdout.len(), 64 * 64 * 4 * 5);
    }
    #[test]
    fn encoded_frames_decode_and_idr_survives_a_dropped_reference() {
        let mut encoder = Encoder::new(64, 64, true).unwrap();
        let mut make_frame = |red: u8, idr: bool| {
            let data: Vec<u8> = [20, 40, red, 255]
                .into_iter()
                .cycle()
                .take(64 * 64 * 4)
                .collect();
            let image = crate::bitmap(
                &data,
                64,
                64,
                256,
                false,
                DesktopSize {
                    width: 64,
                    height: 64,
                },
                false,
            )
            .unwrap();
            encoder.encode(&image, idr).unwrap()
        };
        let first = make_frame(220, true);
        let _dropped = make_frame(120, false);
        let recovered = make_frame(240, true);
        let dir = tempfile::tempdir().unwrap();
        let stream = dir.path().join("frames.h264");
        let mut file = std::fs::File::create(&stream).unwrap();
        file.write_all(&first).unwrap();
        file.write_all(&recovered).unwrap();
        drop(file);
        let output = Command::new("ffmpeg")
            .args(["-v", "error", "-threads", "1", "-i"])
            .arg(stream)
            .args([
                "-f", "rawvideo", "-pix_fmt", "bgra", "-threads", "1", "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout.len(), 64 * 64 * 4 * 2);
        for frame in output.stdout.as_chunks::<{ 64 * 64 * 4 }>().0 {
            assert!(
                frame[2] > 180 && frame[0] < 60 && frame[1] < 80,
                "H.264 decoder must preserve red/blue ordering: {:?}",
                &frame[..4]
            );
        }
    }
}
