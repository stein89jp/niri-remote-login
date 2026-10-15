//! Graphics pipeline factory, per-connection handler state and the display
//! wrapper that routes desktop updates through H.264.
use super::encoder::Encoder;
use super::updates::{DesktopProvider, VideoUpdates};
use crate::performance::{RateApplier, VideoConfig};
use crate::server_error;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_egfx::{
    pdu::{CapabilitiesAdvertisePdu, CapabilitySet},
    server::{GraphicsPipelineHandler, GraphicsPipelineServer},
};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_server::{
    DesktopSize, GfxDvcBridge, GfxServerFactory, GfxServerHandle, RdpServerDisplay,
    RdpServerDisplayUpdates, ServerEvent, ServerEventSender, ServerResult,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Default)]
pub(super) struct Shared {
    pub(super) handle: Option<GfxServerHandle>,
    pub(super) sender: Option<UnboundedSender<ServerEvent>>,
    pub(super) generation: Arc<AtomicU64>,
    pub(super) suppressed: Arc<AtomicBool>,
    pub(super) rate: Arc<Mutex<crate::performance::AdaptiveBitrate>>,
    pub(super) audio_generation: u64,
    pub(super) refresh: Arc<AtomicU64>,
    pub(super) encode_gate: Arc<EncodeGate>,
    pub(super) config: VideoConfig,
}
pub(super) struct EncodeGate(pub(super) Arc<tokio::sync::Semaphore>);
impl Default for EncodeGate {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(1)))
    }
}
#[derive(Clone, Default)]
pub struct Video(pub(super) Arc<Mutex<Shared>>);

pub(super) fn primary_monitor(width: u16, height: u16) -> Vec<Monitor> {
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
        // Reject invalid capture settings before accepting client connections;
        // the parsed settings are reused instead of re-reading the environment.
        let config = VideoConfig::from_env()?;
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
                {
                    let mut state = video.0.lock().unwrap();
                    state.config = config;
                    *state.rate.lock().unwrap() =
                        crate::performance::AdaptiveBitrate::new(config.bitrate, Instant::now());
                }
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
            rate_applier: RateApplier::new(state.config.rate_increase_defer()),
            config: state.config,
        })
    }
    pub fn set_suppressed(&self, suppressed: Arc<AtomicBool>) {
        self.0.lock().unwrap().suppressed = suppressed;
    }
    /// For the disconnect summary only: whether the latest graphics context
    /// would encode AVC444 (negotiated and enabled), as frame submission does.
    pub fn avc444_negotiated(&self) -> bool {
        let (handle, enabled) = {
            let state = self.0.lock().unwrap();
            (state.handle.clone(), state.config.avc444)
        };
        enabled && handle.is_some_and(|handle| handle.lock().unwrap().supports_avc444())
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
            rate.set_frame_rate_ceiling(state.config.fps);
            rate.enable_joint_adaptation(state.config.joint_adaptation);
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
    fn pointer_cache_capabilities(&mut self, max_size: u16, cache_size: u16) {
        self.inner.pointer_cache_capabilities(max_size, cache_size);
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
