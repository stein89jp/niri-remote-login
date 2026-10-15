//! FFI wrapper around the native H.264 (NVENC or software) encoder context.
use crate::performance::VideoConfig;
use ironrdp_server::{BitmapUpdate, PixelFormat};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

unsafe extern "C" {
    fn niri_video_free(encoder: *mut c_void);
    fn niri_video_new_encoder(
        width: c_int,
        height: c_int,
        software: c_int,
        fps: c_int,
        bitrate: c_int,
        preset: c_int,
        spatial_aq: c_int,
        avc444: c_int,
        gop_seconds: c_int,
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
    pub(super) ptr: *mut c_void,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) bitrate: u32,
    pub(super) fps: u32,
    pub(super) avc444: bool,
    pub(super) software: bool,
    pub(super) gpu_active: bool,
    pub(super) config: VideoConfig,
}
/// Whether an Annex B H.264 access unit contains an IDR slice (NAL type 5).
pub fn h264_has_idr(data: &[u8]) -> bool {
    data.windows(4)
        .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5)
}
/// Native calls report through a fixed buffer; never surface an empty error.
pub(super) fn native_error(error: &[c_char], step: &str) -> String {
    let text = if error.contains(&0) {
        unsafe { CStr::from_ptr(error.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    };
    if text.is_empty() {
        format!("{step} failed without native detail")
    } else {
        text
    }
}
#[derive(Debug)]
pub(super) struct EncodedFrame {
    pub(super) main: Vec<u8>,
    pub(super) idr: bool,
    pub(super) auxiliary: Option<Vec<u8>>,
    pub(super) conversion: Duration,
    pub(super) encode: Duration,
    pub(super) captured_at: Option<Instant>,
    pub(super) received_at: Instant,
}
impl From<Vec<u8>> for EncodedFrame {
    fn from(main: Vec<u8>) -> Self {
        Self {
            idr: h264_has_idr(&main),
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
        if !self.ptr.is_null() {
            #[cfg(test)]
            track_native_contexts(-1);
        }
        unsafe { niri_video_free(self.ptr) };
    }
}
#[cfg(test)]
thread_local! {
    /// Live native encoder contexts on this thread and their peak, so tests
    /// can prove a replacement never overlaps the context it replaces.
    pub(super) static NATIVE_CONTEXTS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}
#[cfg(test)]
fn track_native_contexts(delta: isize) {
    NATIVE_CONTEXTS.with(|contexts| {
        let (live, peak) = contexts.get();
        let live = live.saturating_add_signed(delta);
        contexts.set((live, peak.max(live)));
    });
}
impl Encoder {
    pub(super) fn set_rate(&mut self, bitrate: u32, fps: u32) -> anyhow::Result<bool> {
        if (self.bitrate, self.fps) == (bitrate, fps) {
            return Ok(false);
        }
        anyhow::ensure!(
            (1_000_000..=100_000_000).contains(&bitrate) && (1..=60).contains(&fps),
            "invalid adaptive H.264 rate"
        );
        if fps != self.fps {
            // NVENC sessions are a bounded driver resource: release this
            // context before opening its replacement so the two never overlap.
            unsafe { niri_video_free(self.ptr) };
            self.ptr = std::ptr::null_mut();
            #[cfg(test)]
            track_native_contexts(-1);
            *self = Self::create(
                self.width,
                self.height,
                self.software,
                self.avc444,
                fps,
                bitrate,
                &self.config,
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
    pub(super) fn set_bitrate(&mut self, bitrate: u32) -> anyhow::Result<()> {
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
    /// An AVC420 NVENC encoder using the validated environment settings.
    pub fn nvenc(width: u16, height: u16) -> anyhow::Result<Self> {
        let config = VideoConfig::from_env()?;
        Self::create(
            width,
            height,
            false,
            false,
            config.fps,
            config.bitrate,
            &config,
        )
    }
    #[cfg(test)]
    pub(super) fn new(width: u16, height: u16, software: bool) -> anyhow::Result<Self> {
        Self::new_mode(width, height, software, false)
    }
    #[cfg(test)]
    pub(super) fn new_mode(
        width: u16,
        height: u16,
        software: bool,
        avc444: bool,
    ) -> anyhow::Result<Self> {
        let config = VideoConfig::from_env()?;
        Self::create(
            width,
            height,
            software,
            avc444,
            config.fps,
            config.bitrate,
            &config,
        )
    }
    #[cfg(test)]
    pub(super) fn new_rate(
        width: u16,
        height: u16,
        software: bool,
        avc444: bool,
        fps: u32,
        bitrate: u32,
    ) -> anyhow::Result<Self> {
        Self::create(
            width,
            height,
            software,
            avc444,
            fps,
            bitrate,
            &VideoConfig::default(),
        )
    }
    pub(super) fn create(
        width: u16,
        height: u16,
        software: bool,
        avc444: bool,
        fps: u32,
        bitrate: u32,
        config: &VideoConfig,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            width >= 1 && height >= 1 && width <= 4096 && height <= 4096,
            "H.264 dimensions must be within 1..4096"
        );
        let mut error = [0 as c_char; 256];
        let ptr = unsafe {
            niri_video_new_encoder(
                width.into(),
                height.into(),
                software.into(),
                fps as c_int,
                bitrate as c_int,
                config.preset as c_int,
                config.spatial_aq.into(),
                avc444.into(),
                config.gop_seconds as c_int,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            native_error(&error, "H.264 encoder creation")
        );
        #[cfg(test)]
        track_native_contexts(1);
        Ok(Self {
            ptr,
            width,
            height,
            bitrate,
            fps,
            avc444,
            software,
            gpu_active: false,
            config: *config,
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
        let main = unsafe { std::slice::from_raw_parts(main, main_size as usize) }.to_vec();
        Ok(EncodedFrame {
            idr: h264_has_idr(&main),
            main,
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
    pub(super) fn encode_views(
        &mut self,
        image: &BitmapUpdate,
        force_idr: bool,
    ) -> anyhow::Result<EncodedFrame> {
        if !self.avc444 {
            let main = self.encode(image, force_idr)?;
            let (conversion, encode) = self.timings();
            return Ok(EncodedFrame {
                idr: h264_has_idr(&main),
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
        anyhow::ensure!(result == 0, "{}", native_error(&error, "AVC444 encoding"));
        self.encoded(main, main_size, aux, aux_size)
    }
    pub(super) fn encode_gpu(
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
        anyhow::ensure!(result == 0, "{}", native_error(&error, "DMA-BUF encoding"));
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
    pub(super) fn encode_frame(
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
            native_error(&error, "H.264 encoding")
        );
        // Packet storage belongs to the native encoder and is valid until its next call.
        Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
    }
}
