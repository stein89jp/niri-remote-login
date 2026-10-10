//! Optional host NVENC encoding; IronRDP owns graphics negotiation and framing.
use crate::server_error;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_egfx::{
    pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet},
    server::{GraphicsPipelineHandler, GraphicsPipelineServer},
};
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, PixelFormat, RdpServerDisplay, RdpServerDisplayUpdates, ServerEvent,
    ServerEventSender, ServerResult,
};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};

unsafe extern "C" {
    fn niri_video_new(
        width: c_int,
        height: c_int,
        software: c_int,
        fps: c_int,
        bitrate: c_int,
        error: *mut c_char,
        length: usize,
    ) -> *mut c_void;
    fn niri_video_free(encoder: *mut c_void);
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
}
// The FFmpeg context has a single owner; access from encoding tasks is serialized.
unsafe impl Send for Encoder {}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { niri_video_free(self.ptr) };
    }
}
impl Encoder {
    pub fn nvenc(width: u16, height: u16) -> anyhow::Result<Self> {
        Self::new(width, height, false)
    }
    fn new(width: u16, height: u16, software: bool) -> anyhow::Result<Self> {
        anyhow::ensure!(
            width >= 1 && height >= 1 && width <= 4096 && height <= 4096,
            "H.264 dimensions must be within 1..4096"
        );
        let mut error = [0 as c_char; 256];
        let ptr = unsafe {
            niri_video_new(
                width.into(),
                height.into(),
                software.into(),
                crate::performance::frame_rate()? as c_int,
                crate::performance::bitrate()? as c_int,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(Self { ptr, width, height })
    }
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
}
#[derive(Clone, Default)]
pub struct Video(Arc<Mutex<Shared>>);
impl Video {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        // Reject invalid capture settings before accepting client connections.
        crate::performance::frame_rate()?;
        crate::performance::bitrate()?;
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
                Ok(Some(Self::default()))
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
        let state = self.0.lock().unwrap();
        Ok(Box::new(VideoUpdates {
            inner: desktop.updates_send(),
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
        }))
    }
    pub fn set_suppressed(&self, suppressed: Arc<AtomicBool>) {
        self.0.lock().unwrap().suppressed = suppressed;
    }
}
struct Handler(Arc<AtomicU64>);
impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, _: &CapabilitiesAdvertisePdu) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn on_ready(&mut self, _: &CapabilitySet) {}
    fn on_close(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn max_frames_in_flight(&self) -> u32 {
        2
    }
}
impl ServerEventSender for Video {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.0.lock().unwrap().sender = Some(sender);
    }
}
impl GfxServerFactory for Video {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler(Arc::new(AtomicU64::new(0))))
    }
    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        // A new context per connection prevents surfaces/acks crossing authenticated sessions.
        let generation = Arc::new(AtomicU64::new(0));
        let handle = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(Handler(
            generation.clone(),
        )))));
        let mut state = self.0.lock().unwrap();
        state.handle = Some(handle.clone());
        state.generation = generation;
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
    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(self.video.updates_send(&self.inner)?)
    }
}
type EncodingTask = JoinHandle<anyhow::Result<Vec<u8>>>;
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
}
impl VideoUpdates {
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
    fn submit(&mut self, data: &[u8], width: u16, height: u16) -> ServerResult<()> {
        let mut gfx = self.handle.lock().unwrap();
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
                    gfx.resize(width, height);
                }
                gfx.set_output_dimensions(width, height);
                let id = gfx
                    .create_surface(width, height)
                    .ok_or_else(|| server_error("graphics surface failed"))?;
                gfx.map_surface_to_output(id, 0, 0);
                self.surface = Some(id);
                eprintln!("RDP graphics using NVENC H.264 AVC420 ({width}x{height})");
                id
            }
        };
        let frame = gfx.send_avc420_frame(
            surface,
            data,
            &[Avc420Region::full_frame(width, height, 22)],
            self.start.elapsed().as_millis() as u32,
        );
        // If a reference frame is dropped, the next accepted frame must be an IDR.
        self.force_idr = frame.is_none();
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
            if let Some((task, width, height, generation)) = &mut self.pending {
                // Keep the handle in self: cancellation of this future cannot lose an encoded reference frame.
                let result = task.await;
                let (width, height) = (*width, *height);
                let generation = *generation;
                self.pending = None;
                let data = match result {
                    Ok(Ok(data)) => data,
                    other => {
                        self.ended = true;
                        return Err(server_error(format!("NVENC encoding failed: {other:?}")));
                    }
                };
                if self.generation.load(Ordering::SeqCst) != generation
                    || self.suppressed.load(Ordering::Relaxed)
                {
                    // Decoder recovery/minimize can invalidate an in-flight reference frame.
                    self.force_idr = true;
                    continue;
                }
                if let Err(error) = self.submit(&data, width, height) {
                    self.ended = true;
                    return Err(error);
                }
            }
            let Some(update) = self.inner.next_update().await? else {
                return Ok(None);
            };
            let image = match update {
                DisplayUpdate::Bitmap(image) => image,
                DisplayUpdate::Resize(size) => {
                    self.encoder.lock().unwrap().take();
                    self.last_image = None;
                    self.surface = None;
                    self.force_idr = true;
                    let mut gfx = self.handle.lock().unwrap();
                    if gfx.is_ready() && gfx.supports_avc420() {
                        gfx.resize(size.width, size.height);
                    }
                    return Ok(Some(DisplayUpdate::Resize(size)));
                }
                other => return Ok(Some(other)),
            };
            let generation = self.generation.load(Ordering::SeqCst);
            if generation != self.known_generation {
                self.surface = None;
                self.force_idr = true;
                self.known_generation = generation;
            }
            if self.suppressed.load(Ordering::Relaxed) {
                self.force_idr = true;
                continue;
            }
            {
                let gfx = self.handle.lock().unwrap();
                if !gfx.is_ready() || !gfx.supports_avc420() {
                    if self.surface.is_some() {
                        self.ended = true;
                        return Err(server_error("H.264 graphics channel closed"));
                    }
                    // Keep older clients usable, including those with AVC disabled.
                    return Ok(Some(DisplayUpdate::Bitmap(image)));
                }
                if gfx.should_backpressure() {
                    continue;
                }
            }
            // Continue consuming capture heartbeats so capability recovery,
            // minimize/resume and backpressure still wake on a static desktop.
            // Avoid conversion, GPU upload and wire traffic for identical pixels.
            if self.unchanged(&image) {
                continue;
            }
            let (width, height) = (image.width.get(), image.height.get());
            self.last_image = Some(image.clone());
            let encoder = self.encoder.clone();
            let force_idr = self.force_idr;
            // Encoder initialization and GPU submission run off the input/network thread.
            let task = tokio::task::spawn_blocking(move || {
                let mut encoder = encoder.lock().unwrap();
                Encoder::encode_frame(&mut encoder, &image, force_idr, Encoder::nvenc)
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
            },
            receiver,
        )
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
            tokio::spawn(async { Ok(vec![0, 0, 0, 1, 0x41]) }),
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
    async fn minimized_output_drops_pending_reference_and_requires_idr_on_resume() {
        let (mut stream, mut events) = updates(vec![image()]);
        advertise(&stream.handle, true);
        stream.known_generation = stream.generation.load(Ordering::SeqCst);
        stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
        let surface = stream.surface;
        assert!(events.try_recv().is_ok());
        assert!(!stream.force_idr);
        stream.pending = Some((
            tokio::spawn(async { Ok(vec![0, 0, 0, 1, 0x41]) }),
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
