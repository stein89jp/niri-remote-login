//! The `VideoUpdates` state machine: capture cadence, reference invalidation,
//! bitmap fallback and asynchronous encoding.
use super::encoder::{EncodedFrame, Encoder};
use super::gfx::{EncodeGate, primary_monitor};
use crate::performance::{RateApplier, VideoConfig};
use crate::server_error;
use ironrdp_egfx::{pdu::Avc420Region, server::FrameSubmissionError};
use ironrdp_server::{
    BitmapUpdate, DisplayUpdate, EgfxServerMessage, GfxServerHandle, RdpServerDisplayUpdates,
    ServerEvent, ServerResult,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

pub(super) type DesktopProvider = Arc<dyn Fn() -> Option<crate::wayland::Desktop> + Send + Sync>;
/// What an invalidated client reference also discards. Every invalidation
/// makes the next encoded frame an IDR and cancels a pending refinement.
#[derive(Clone, Copy, Default)]
struct Invalidation {
    /// The bitmap-fallback diff base and its queued tiles.
    fallback: bool,
    /// The native encoder context and the retained GPU frame.
    encoder: bool,
    /// The graphics surface, recreated before the next frame.
    surface: bool,
    /// GPU capture, requested again when the next frame is due.
    gpu_capture: bool,
}
pub(super) type EncodingTask = JoinHandle<anyhow::Result<EncodedFrame>>;
pub(super) struct VideoUpdates {
    pub(super) inner: Box<dyn RdpServerDisplayUpdates + Send>,
    pub(super) handle: GfxServerHandle,
    pub(super) sender: UnboundedSender<ServerEvent>,
    pub(super) encoder: Arc<Mutex<Option<Encoder>>>,
    pub(super) pending: Option<(EncodingTask, u16, u16, u64)>,
    pub(super) surface: Option<u16>,
    pub(super) start: Instant,
    pub(super) force_idr: bool,
    // Bytes is reference counted: retaining one input adds no full-frame copy.
    pub(super) last_image: Option<BitmapUpdate>,
    pub(super) ended: bool,
    pub(super) generation: Arc<AtomicU64>,
    pub(super) known_generation: u64,
    pub(super) suppressed: Arc<AtomicBool>,
    pub(super) rate: Arc<Mutex<crate::performance::AdaptiveBitrate>>,
    pub(super) desktop: Option<crate::wayland::Desktop>,
    pub(super) desktop_provider: Option<DesktopProvider>,
    pub(super) pending_gpu: bool,
    pub(super) gpu_failed: bool,
    pub(super) last_gpu: Option<Arc<crate::wayland::GpuFrame>>,
    pub(super) refinement: Option<(Instant, Instant)>,
    pub(super) next_frame: Instant,
    pub(super) diagnostics: crate::performance::Telemetry,
    pub(super) credit_wait_started: Option<Instant>,
    pub(super) fallback_previous: Option<BitmapUpdate>,
    pub(super) fallback_partial: std::collections::VecDeque<BitmapUpdate>,
    pub(super) fallback_resume: bool,
    pub(super) refresh: Arc<AtomicU64>,
    pub(super) known_refresh: u64,
    pub(super) pending_refresh: u64,
    pub(super) encode_gate: Arc<EncodeGate>,
    pub(super) rate_applier: RateApplier,
    pub(super) config: VideoConfig,
}
impl VideoUpdates {
    /// Invalidate the client's reference: refresh, generation change, resize,
    /// output suppression and GPU capture failure share this recovery.
    fn invalidate_reference(&mut self, scope: Invalidation) {
        self.force_idr = true;
        self.refinement = None;
        if scope.fallback {
            self.fallback_previous = None;
            self.fallback_partial.clear();
        }
        if scope.encoder {
            self.last_gpu = None;
            self.encoder.lock().unwrap().take();
        }
        if scope.surface {
            self.surface = None;
        }
        if scope.gpu_capture
            && let Some(desktop) = &self.desktop
        {
            desktop.set_gpu_capture(false);
        }
    }
    /// A client refresh invalidates every reference the client may lack.
    fn observe_refresh(&mut self) {
        let refresh = self.refresh.load(Ordering::SeqCst);
        if refresh != self.known_refresh {
            self.known_refresh = refresh;
            self.invalidate_reference(Invalidation {
                fallback: true,
                ..Invalidation::default()
            });
        }
    }
    pub(super) fn cadence_due(&self, now: Instant) -> bool {
        // Authenticated capture already uses the selected FPS. Ready-time
        // jitter must not lose frames to a second, independently phased clock.
        // A login provider can exist before it returns an actual Desktop.
        self.desktop.is_some() || self.force_idr || now >= self.next_frame
    }
    pub(super) fn attach_authenticated_desktop(&mut self) {
        if self.desktop.is_none() {
            self.desktop = self
                .desktop_provider
                .as_ref()
                .and_then(|provider| provider());
        }
    }
    pub(super) fn unchanged(&self, image: &BitmapUpdate) -> bool {
        !self.force_idr
            && self.last_image.as_ref().is_some_and(|previous| {
                // Shared buffers and capture-log tiles avoid reading complete
                // desktops on the runtime thread.
                match &self.desktop {
                    Some(desktop) => desktop.same_capture(previous, image),
                    None => crate::same_pixels(previous, image, None),
                }
            })
    }
    #[cfg(test)]
    pub(super) fn submit(&mut self, data: &[u8], width: u16, height: u16) -> ServerResult<()> {
        self.submit_frame(&EncodedFrame::from(data.to_vec()), width, height)
    }
    pub(super) fn submit_frame(
        &mut self,
        data: &EncodedFrame,
        width: u16,
        height: u16,
    ) -> ServerResult<()> {
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
            gfx.try_send_avc444_frame(
                surface,
                &data.main,
                &regions,
                Some(auxiliary),
                Some(&regions),
                self.start.elapsed().as_millis() as u32,
            )
        } else {
            gfx.try_send_avc420_frame(
                surface,
                &data.main,
                &regions,
                self.start.elapsed().as_millis() as u32,
            )
        };
        // The protocol server's frame tracker is the only credit gate: it
        // applies the client's core Frame Acknowledge ceiling to this
        // stream's adaptive window. A full window is ordinary flow control,
        // but the encoder has already referenced the dropped frame, so any
        // rejection still requires the next accepted frame to be an IDR.
        match &frame {
            Err(FrameSubmissionError::Backpressured { .. }) => {
                self.diagnostics.count("backpressured_submissions", 1)
            }
            Err(_) => self.diagnostics.count("rejected_submissions", 1),
            Ok(_) => (),
        }
        self.force_idr = frame.is_err();
        if let Ok(id) = frame {
            self.rate.lock().unwrap().sent(id, Instant::now());
            self.diagnostics.count("delivered_frames", 1);
            if data.idr {
                self.rate_applier.idr_sent(Instant::now());
                self.diagnostics.count("idr_frames", 1);
            }
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
            self.observe_refresh();
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
                        self.invalidate_reference(Invalidation {
                            encoder: true,
                            gpu_capture: true,
                            ..Invalidation::default()
                        });
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
                    self.last_image = None;
                    self.invalidate_reference(Invalidation {
                        fallback: true,
                        encoder: true,
                        surface: true,
                        gpu_capture: true,
                    });
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
            self.observe_refresh();
            if generation != self.known_generation {
                self.known_generation = generation;
                self.invalidate_reference(Invalidation {
                    fallback: true,
                    encoder: true,
                    surface: true,
                    gpu_capture: true,
                });
            }
            // mstsc can suppress output during connection setup before it has
            // a surface. Deliver the first graphics frame so initialization
            // cannot deadlock on the client's later un-suppress request.
            if self.surface.is_some() && self.suppressed.load(Ordering::Relaxed) {
                self.invalidate_reference(Invalidation {
                    gpu_capture: true,
                    ..Invalidation::default()
                });
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
                    self.fallback_partial.extend(match &self.desktop {
                        Some(desktop) => {
                            desktop.capture_damage(self.fallback_previous.as_ref(), &image)
                        }
                        None => crate::bitmap_damage(self.fallback_previous.as_ref(), &image),
                    });
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
                avc444 = gfx.supports_avc444() && self.config.avc444;
            }
            let now = Instant::now();
            let (target, fps) = {
                let rate = self.rate.lock().unwrap();
                (rate.current(), rate.frame_rate())
            };
            if !self.cadence_due(now) {
                self.diagnostics.count("cadence_skipped_frames", 1);
                continue;
            }
            let gpu = if let Some(desktop) = &self.desktop {
                if !self.gpu_failed && self.config.gpu_capture {
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
            let bitrate = self.rate_applier.select(target, fps, force_idr, now);
            if bitrate != target {
                self.diagnostics.count("deferred_rate_increase_frames", 1);
            }
            let config = self.config;
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
                    (encoder.width, encoder.height, encoder.avc444, encoder.fps)
                        != (width, height, avc444, fps)
                        || (encoder.gpu_active && gpu.is_none())
                });
                if changed {
                    let software = encoder.as_ref().is_some_and(|encoder| encoder.software);
                    // Release the old native session before opening its
                    // replacement; NVENC session counts are limited.
                    encoder.take();
                    *encoder = Some(Encoder::create(
                        width, height, software, avc444, fps, bitrate, &config,
                    )?);
                }
                let encoder = encoder.as_mut().unwrap();
                // NVENC rate reconfiguration emits an IDR by itself.
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
