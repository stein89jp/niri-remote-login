use super::encoder::{EncodedFrame, Encoder, NATIVE_CONTEXTS, native_error};
use super::gfx::primary_monitor;
use super::updates::VideoUpdates;
use super::*;
use crate::performance::RateApplier;
use ironrdp_dvc::DvcProcessor;
use ironrdp_egfx::pdu::{CapabilitiesAdvertisePdu, CapabilitiesV10Flags, CapabilitySet, GfxPdu};
use ironrdp_server::{
    DesktopSize, DisplayUpdate, EgfxServerMessage, GfxServerFactory, GfxServerHandle,
    RdpServerDisplayUpdates, ServerEvent, ServerEventSender, ServerResult,
};
use std::{
    collections::VecDeque,
    ffi::c_char,
    io::Write,
    process::Command,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

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
            rate_applier: RateApplier::new(state.config.rate_increase_defer()),
            config: state.config,
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
async fn explicit_refresh_discards_pending_reference_and_sends_idr_without_surface_recreation() {
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
        7_300_000,
        "old-surface ACK and age cannot penalize the replacement"
    );
    let mut inherited = crate::performance::AdaptiveBitrate::new(12_000_000, now);
    inherited.inherit_audio_congestion(&rate, now);
    assert_eq!(
        inherited.current(),
        7_300_000,
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
fn cadence_change_releases_the_old_native_context_before_creating_its_replacement() {
    NATIVE_CONTEXTS.with(|contexts| contexts.set((0, 0)));
    let mut encoder = Encoder::new_rate(64, 64, true, false, 60, 12_000_000).unwrap();
    assert_eq!(NATIVE_CONTEXTS.with(|contexts| contexts.get()), (1, 1));
    assert!(encoder.set_rate(3_000_000, 15).unwrap());
    assert_eq!(
        NATIVE_CONTEXTS.with(|contexts| contexts.get()),
        (1, 1),
        "two encoder sessions must never overlap"
    );
    assert!(encoder.set_rate(3_000_000, 61).is_err());
    drop(encoder);
    assert_eq!(NATIVE_CONTEXTS.with(|contexts| contexts.get()).0, 0);
}
#[test]
fn native_errors_are_descriptive_even_without_native_text() {
    let mut error = [0 as c_char; 16];
    assert_eq!(
        native_error(&error, "H.264 encoding"),
        "H.264 encoding failed without native detail"
    );
    for (slot, byte) in error.iter_mut().zip(b"open H.264\0") {
        *slot = *byte as c_char;
    }
    assert_eq!(native_error(&error, "H.264 encoding"), "open H.264");
    let unterminated = [b'x' as c_char; 4];
    assert_eq!(
        native_error(&unterminated, "AVC444 encoding"),
        "AVC444 encoding failed without native detail"
    );
    let error = Encoder::new_rate(64, 64, true, false, 60, 999_999)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("bitrate"), "{error}");
    let mut encoder = Encoder::new_rate(64, 64, true, false, 60, 12_000_000).unwrap();
    let DisplayUpdate::Bitmap(image) = image() else {
        unreachable!()
    };
    // A SHM AVC420 context rejects the paired path with native text.
    encoder.avc444 = true;
    let error = encoder.encode_views(&image, true).unwrap_err().to_string();
    assert_eq!(error, "invalid AVC444 input");
}
#[test]
fn idr_detection_counts_only_instantaneous_decoder_refresh_slices() {
    assert!(h264_has_idr(&[0, 0, 0, 1, 0x67, 0, 0, 1, 0x65, 0x88]));
    assert!(!h264_has_idr(&[0, 0, 0, 1, 0x41, 0x9a]));
    assert!(!h264_has_idr(&[0x65, 0, 0, 1]));
    assert!(EncodedFrame::from(vec![0, 0, 1, 0x25]).idr);
    let mut encoder = Encoder::new_rate(64, 64, true, false, 60, 12_000_000).unwrap();
    let DisplayUpdate::Bitmap(image) = image() else {
        unreachable!()
    };
    assert!(encoder.encode_views(&image, true).unwrap().idr);
    assert!(!encoder.encode_views(&image, false).unwrap().idr);
}
#[test]
fn delivered_idrs_let_a_held_rate_increase_wait_for_the_next_one() {
    let (mut stream, mut events) = updates(vec![]);
    advertise(&stream.handle, true);
    let now = Instant::now();
    assert_eq!(
        stream.rate_applier.select(7_300_000, 60, true, now),
        7_300_000
    );
    stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
    events.try_recv().unwrap();
    assert_eq!(
        stream
            .rate_applier
            .select(12_000_000, 60, false, Instant::now()),
        7_300_000,
        "a delivered IDR starts the increase hold"
    );
    assert_eq!(
        stream
            .rate_applier
            .select(12_000_000, 60, true, Instant::now()),
        12_000_000
    );
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
    assert_eq!(stream.rate.lock().unwrap().current(), 7_300_000);
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
    assert_eq!(previous.lock().unwrap().current(), 7_300_000);
    video.build_server_with_handle().unwrap();
    let current = video.0.lock().unwrap().rate.clone();
    assert!(!Arc::ptr_eq(&previous, &current));
    assert_eq!(current.lock().unwrap().current(), 7_300_000);
    sound
        .clone()
        .report_congestion(now + Duration::from_secs(1));
    assert_eq!(current.lock().unwrap().current(), 4_400_000);
    assert_eq!(previous.lock().unwrap().current(), 7_300_000);
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
        7_300_000
    );
    let current = video.audio_feedback();
    video.build_server_with_handle().unwrap();
    let rate = video.0.lock().unwrap().rate.clone();
    assert_eq!(rate.lock().unwrap().current(), 12_000_000);
    old.report_congestion(now + Duration::from_secs(1));
    assert_eq!(rate.lock().unwrap().current(), 12_000_000);
    current.report_congestion(now + Duration::from_secs(2));
    assert_eq!(rate.lock().unwrap().current(), 7_300_000);
    old.clear();
    video.build_server_with_handle().unwrap();
    assert_eq!(
        video.0.lock().unwrap().rate.lock().unwrap().current(),
        7_300_000,
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
#[test]
fn client_frame_ack_ceiling_bounds_the_adaptive_window_through_one_gate() {
    use ironrdp_egfx::pdu::{FrameAcknowledgePdu, QueueDepth};
    use std::num::NonZeroU32;
    let (mut stream, mut events) = updates(vec![]);
    advertise(&stream.handle, true);
    // The protocol server applies the core Frame Acknowledge capability at
    // activation; the bridge's adaptive window stays the configured value.
    stream
        .handle
        .lock()
        .unwrap()
        .set_client_frame_ack_limit(NonZeroU32::new(1));
    assert_eq!(stream.rate.lock().unwrap().max_frames_in_flight(), 2);
    let frame_id = |events: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>| {
        graphics_pdus(events.try_recv().unwrap())
            .into_iter()
            .find_map(|pdu| match pdu {
                GfxPdu::StartFrame(frame) => Some(frame.frame_id),
                _ => None,
            })
            .unwrap()
    };
    stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
    let first = frame_id(&mut events);
    assert!(!stream.force_idr);
    {
        let gfx = stream.handle.lock().unwrap();
        assert_eq!(
            gfx.max_frames_in_flight(),
            1,
            "client ceiling clamps the window"
        );
        assert!(
            gfx.should_backpressure(),
            "one unacknowledged frame fills it"
        );
    }
    // A frame encoded before the window filled is rejected as backpressure,
    // not a channel failure, and the next accepted frame must be an IDR.
    stream.submit(&[0, 0, 0, 1, 0x41], 64, 64).unwrap();
    assert!(
        events.try_recv().is_err(),
        "nothing is queued past the ceiling"
    );
    assert!(stream.force_idr);
    let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
        queue_depth: QueueDepth::AvailableBytes(0),
        frame_id: first,
        total_frames_decoded: 1,
    });
    stream
        .handle
        .lock()
        .unwrap()
        .process(7, &ironrdp_core::encode_vec(&ack).unwrap())
        .unwrap();
    assert!(!stream.handle.lock().unwrap().should_backpressure());
    stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
    assert!(frame_id(&mut events) > first);
    assert!(!stream.force_idr);
    // Surface recreation reapplies the bridge window; a channel reopen keeps
    // the connection's ceiling.
    {
        let mut gfx = stream.handle.lock().unwrap();
        gfx.close(7);
        gfx.start(7).unwrap();
    }
    advertise(&stream.handle, true);
    stream.surface = None;
    stream.submit(&[0, 0, 0, 1, 0x65], 64, 64).unwrap();
    let gfx = stream.handle.lock().unwrap();
    assert_eq!(gfx.max_frames_in_flight(), 1);
    assert!(gfx.should_backpressure());
}
