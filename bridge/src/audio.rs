//! Session audio through the TLS-protected RDP sound channel.
use ironrdp_rdpsnd::{
    pdu::{AudioFormat, WaveFormat},
    server::{NegotiatedFormat, RdpsndError},
};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use std::{
    collections::VecDeque,
    ffi::{CStr, c_char, c_void},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;

pub const BLOCK_BYTES: usize = 48000 * 2 * 2 / 50; // 20 ms, stereo S16LE
/// One 20 ms block at 44.1 kHz: 960 * 147 / 160 frames, exactly.
const PCM44_FRAMES: usize = 882;
const ADPCM_SAMPLES: usize = 1012;
const ADPCM_BLOCK_BYTES: usize = 1024;
// IronRDP keeps at most four waves per event batch. Stay below that limit
// so every queued wave receives a block number and can be acknowledged.
const MAX_PENDING: usize = 4;
const MAX_CAPTURE_BLOCKS: usize = 3; // At most 64 ms of capture batching/jitter
const PLAYBACK_BUDGET: Duration = Duration::from_millis(300);
const CAPTURE_BUDGET: Duration = Duration::from_millis(60);
const SILENCE_TAIL_BLOCKS: usize = 5; // 100 ms flushes bounded AAC lookahead.
// Earned clock-drift catch-up for PCM (a skip user-approved
// 2026-10-18; the blind 30 s skip replaced by this on the user's request
// 2026-10-20). mstsc confirms waves on arrival, so its playback queue is
// invisible here. A 20 ms block is dropped only when the estimated surplus in
// that queue (see `ArrivalModel`) exceeds a jitter reserve by one block.
const BLOCK_DURATION: Duration = Duration::from_millis(20);
// Never drop the last 60 ms of estimated surplus: it absorbs network jitter.
const DRIFT_SKIP_RESERVE: Duration = Duration::from_millis(60);
// The surplus estimate saturates here, bounding catch-up after a long stall.
const DRIFT_SKIP_EARNED_CAP: Duration = Duration::from_secs(1);
// Default unobservable client-clock drift allowance: 20 ms per 200 s of
// sound. `NIRI_RDP_AUDIO_DRIFT_PPM` overrides it within 0..=MAX_DRIFT_PPM.
const DRIFT_ALLOWANCE_PPM: u32 = 100;
const MAX_DRIFT_PPM: u32 = 2000;
// The model running dry this long is a likely client underrun. mstsc may then
// rebuild a larger queue that receipts cannot show (the pre-2026-10-18 growth
// that followed credit waits), so each such starvation earns one block...
const STARVATION_MIN: Duration = Duration::from_millis(100);
const STARVATION_CREDIT: Duration = BLOCK_DURATION;
// ...at most once per 30 s: never faster than the former blind 667 ppm.
const STARVATION_CREDIT_SPACING: Duration = Duration::from_secs(30);
// The estimate is the lowest surplus over the last one to two such windows.
const ARRIVAL_LOW_WATER_WINDOW: Duration = Duration::from_secs(1);
// Minimum time between skips, and from the start or resume of a sound run.
const DRIFT_SKIP_MIN_SPACING: Duration = Duration::from_secs(10);
// While armed, wait at most this long for a quiet block, then drop the next one.
const DRIFT_SKIP_MAX_WAIT: Duration = Duration::from_secs(5);
// A block is quiet when its mean absolute sample is at most about -40 dBFS...
const DRIFT_SKIP_QUIET_LEVEL: u32 = 328;
// ...or at most a quarter (-12 dB) of the recent audible average.
const DRIFT_SKIP_QUIET_FRACTION: f32 = 0.25;
// Weight of each audible block in that average: about a 640 ms time constant.
const DRIFT_SKIP_AVERAGE_WEIGHT: f32 = 1.0 / 32.0;
// The block after a skip fades in from the dropped block's first 5 ms.
const DRIFT_SKIP_FADE_FRAMES: usize = 48000 * 5 / 1000;
// Diagnostics: individual Wave Confirm lines per stream, then 5 s summaries.
const CONFIRM_DETAIL_LINES: u32 = 32;
const CONFIRM_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const CAPTURE_MAGIC: &[u8; 4] = b"NRA1";
const CAPTURE_HEADER_BYTES: usize = 32;

/// Linux CLOCK_MONOTONIC is shared by the helper and frontend, unlike Instant's
/// private representation. This age starts at the helper's first monitor read;
/// browser/PipeWire buffering before that observable point is not measured.
fn monotonic_ns() -> u64 {
    let mut value: libc::timespec = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) },
        0
    );
    value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
}

/// Time source for capture, pacing, expiry and confirmation handling.
/// Production always uses `SystemClock`; tests substitute a manual clock so
/// timing assertions do not depend on scheduler latency.
trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    /// CLOCK_MONOTONIC nanoseconds, comparable with the capture helper's.
    fn monotonic_ns(&self) -> u64;
    /// poll(2) timeout in milliseconds that expires at `deadline`.
    fn poll_timeout(&self, deadline: Instant) -> i32;
}

struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn monotonic_ns(&self) -> u64 {
        monotonic_ns()
    }
    fn poll_timeout(&self, deadline: Instant) -> i32 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Round upward: sub-millisecond deadlines must not spin at zero.
        remaining
            .as_millis()
            .saturating_add(u128::from(
                !remaining.subsec_nanos().is_multiple_of(1_000_000),
            ))
            .min(i32::MAX as u128) as i32
    }
}

#[derive(Clone)]
struct AudioClock(Arc<dyn Clock>);
impl Default for AudioClock {
    fn default() -> Self {
        Self(Arc::new(SystemClock))
    }
}
impl std::ops::Deref for AudioClock {
    type Target = dyn Clock;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// Test-only record of the worker blocking with no pending input.
#[cfg(test)]
#[derive(Default)]
struct Quiescence {
    requested: u64,
    /// Settle request observed, and poll timeout, of the current idle wait.
    idle: Option<(u64, i32)>,
}

struct Wake {
    fd: OwnedFd,
    #[cfg(test)]
    quiescence: (Mutex<Quiescence>, std::sync::Condvar),
}
impl Default for Wake {
    fn default() -> Self {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(fd >= 0, "audio wake descriptor unavailable");
        Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            #[cfg(test)]
            quiescence: Default::default(),
        }
    }
}
#[cfg(test)]
impl Wake {
    /// Wait until the worker has processed every input and wake supplied
    /// before this call and blocks again. Returns that wait's poll timeout.
    fn settle(&self) -> i32 {
        let (state, changed) = &self.quiescence;
        let mut state = state.lock().unwrap();
        state.requested += 1;
        let token = state.requested;
        // Notify under the lock: a worker that observes this token also
        // observes the wake, so it cannot report idle before consuming it.
        self.notify();
        let (state, wait) = changed
            .wait_timeout_while(state, Duration::from_secs(20), |state| {
                !state.idle.is_some_and(|(seen, _)| seen >= token)
            })
            .unwrap();
        assert!(!wait.timed_out(), "audio worker did not become idle");
        state.idle.unwrap().1
    }
}
impl Wake {
    fn notify(&self) {
        let value = 1u64;
        // Saturation means a wake is already pending. No application data enters
        // this descriptor, and its owner remains live throughout the syscall.
        unsafe { libc::write(self.fd.as_raw_fd(), (&value as *const u64).cast(), 8) };
    }
    fn wait(
        &self,
        socket: Option<&UnixStream>,
        deadline: Option<Instant>,
        clock: &dyn Clock,
    ) -> bool {
        let mut descriptors = [
            libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket.map_or(-1, AsRawFd::as_raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout = deadline.map_or(-1, |at| clock.poll_timeout(at));
        #[cfg(test)]
        if timeout != 0 {
            let (state, changed) = &self.quiescence;
            let mut state = state.lock().unwrap();
            let mut ready = descriptors;
            if unsafe { libc::poll(ready.as_mut_ptr(), 2, 0) } == 0 {
                state.idle = Some((state.requested, timeout));
                changed.notify_all();
            }
        }
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
        #[cfg(test)]
        {
            self.quiescence.0.lock().unwrap().idle = None;
        }
        if result < 0 {
            return std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
        }
        if descriptors[0].revents != 0 {
            let mut value = 0u64;
            unsafe { libc::read(self.fd.as_raw_fd(), (&mut value as *mut u64).cast(), 8) };
        }
        true
    }
}

struct CaptureSocket {
    stream: UnixStream,
    framed: bool,
}

/// Decode only the explicitly selected private protocol. A wrong version or
/// truncated frame ends audio, so metadata can never become audible PCM.
struct CaptureReader {
    socket: CaptureSocket,
    header: [u8; CAPTURE_HEADER_BYTES],
    header_filled: usize,
    payload: [u8; BLOCK_BYTES],
    payload_filled: usize,
    payload_offset: usize,
    source_ns: u64,
    expected_sequence: Option<u64>,
    discontinuity: bool,
    relay_age: Duration,
    socket_age: Duration,
    clock: AudioClock,
}
impl CaptureReader {
    fn new(socket: CaptureSocket, clock: AudioClock) -> std::io::Result<Self> {
        let mut this = Self {
            socket,
            header: [0; CAPTURE_HEADER_BYTES],
            header_filled: 0,
            payload: [0; BLOCK_BYTES],
            payload_filled: 0,
            payload_offset: 0,
            source_ns: 0,
            expected_sequence: None,
            discontinuity: false,
            relay_age: Duration::ZERO,
            socket_age: Duration::ZERO,
            clock,
        };
        this.socket.stream.write_all(if this.socket.framed {
            CAPTURE_MAGIC
        } else {
            b"A"
        })?;
        this.socket.stream.set_nonblocking(true)?;
        Ok(this)
    }
    fn read(&mut self, destination: &mut [u8]) -> std::io::Result<(usize, Instant)> {
        if !self.socket.framed {
            return self
                .socket
                .stream
                .read(destination)
                .map(|length| (length, self.clock.now()));
        }
        while self.header_filled < CAPTURE_HEADER_BYTES {
            let n = self
                .socket
                .stream
                .read(&mut self.header[self.header_filled..])?;
            if n == 0 {
                return Ok((0, self.clock.now()));
            }
            self.header_filled += n;
            if self.header_filled == CAPTURE_HEADER_BYTES {
                let sequence = u64::from_be_bytes(self.header[8..16].try_into().unwrap());
                self.source_ns = u64::from_be_bytes(self.header[16..24].try_into().unwrap());
                let relay_ns = u64::from_be_bytes(self.header[24..32].try_into().unwrap());
                if &self.header[..4] != CAPTURE_MAGIC
                    || u32::from_be_bytes(self.header[4..8].try_into().unwrap()) as usize
                        != BLOCK_BYTES
                    || self.source_ns > relay_ns
                    || relay_ns > self.clock.monotonic_ns()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid private PCM frame",
                    ));
                }
                self.discontinuity = self
                    .expected_sequence
                    .is_some_and(|expected| expected != sequence);
                self.expected_sequence = Some(sequence.wrapping_add(1));
                self.relay_age = Duration::from_nanos(relay_ns - self.source_ns);
                self.socket_age =
                    Duration::from_nanos(self.clock.monotonic_ns().saturating_sub(relay_ns));
            }
        }
        while self.payload_filled < BLOCK_BYTES {
            let n = self
                .socket
                .stream
                .read(&mut self.payload[self.payload_filled..])?;
            if n == 0 {
                return Ok((0, self.clock.now()));
            }
            self.payload_filled += n;
        }
        let length = destination.len().min(BLOCK_BYTES - self.payload_offset);
        destination[..length]
            .copy_from_slice(&self.payload[self.payload_offset..self.payload_offset + length]);
        self.payload_offset += length;
        let now = self.clock.now();
        let age = Duration::from_nanos(self.clock.monotonic_ns().saturating_sub(self.source_ns));
        let captured_at = now.checked_sub(age).unwrap_or(now - CAPTURE_BUDGET * 2);
        if self.payload_offset == BLOCK_BYTES {
            self.header_filled = 0;
            self.payload_filled = 0;
            self.payload_offset = 0;
        }
        Ok((length, captured_at))
    }
}

unsafe extern "C" {
    fn niri_audio_new(error: *mut c_char, length: usize) -> *mut c_void;
    fn niri_audio_free(encoder: *mut c_void);
    fn niri_audio_extra(encoder: *mut c_void, data: *mut u8, capacity: usize) -> i32;
    fn niri_audio_encode(
        encoder: *mut c_void,
        pcm: *const u8,
        length: usize,
        data: *mut *const u8,
        error: *mut c_char,
        error_length: usize,
    ) -> i32;
    fn niri_audio_resampler_new(channels: i32, error: *mut c_char, length: usize) -> *mut c_void;
    fn niri_audio_resampler_free(resampler: *mut c_void);
    fn niri_audio_resampler_reset(resampler: *mut c_void, error: *mut c_char, length: usize)
    -> i32;
    fn niri_audio_resample(
        resampler: *mut c_void,
        pcm: *const u8,
        length: usize,
        output: *mut u8,
        capacity: usize,
        error: *mut c_char,
        error_length: usize,
    ) -> i32;
    fn niri_audio_aac_new(error: *mut c_char, length: usize) -> *mut c_void;
    fn niri_audio_aac_free(encoder: *mut c_void);
    fn niri_audio_aac_encode(
        encoder: *mut c_void,
        pcm: *const u8,
        length: usize,
        data: *mut *const u8,
        error: *mut c_char,
        error_length: usize,
    ) -> i32;
}

struct Aac {
    ptr: *mut c_void,
}
// Like ADPCM, the encoder and resampler have a single capture-worker owner.
unsafe impl Send for Aac {}
impl Drop for Aac {
    fn drop(&mut self) {
        unsafe { niri_audio_aac_free(self.ptr) };
    }
}
impl Aac {
    fn new() -> anyhow::Result<Self> {
        let mut error = [0 as c_char; 256];
        let ptr = unsafe { niri_audio_aac_new(error.as_mut_ptr(), error.len()) };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(Self { ptr })
    }
    fn encode(&mut self, pcm: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let mut error = [0 as c_char; 256];
        let mut data = std::ptr::null();
        let length = unsafe {
            niri_audio_aac_encode(
                self.ptr,
                pcm.as_ptr(),
                pcm.len(),
                &mut data,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            length >= 0 && (length == 0 || !data.is_null()),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        if length == 0 {
            return Ok(None);
        }
        Ok(Some(
            unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec(),
        ))
    }
}

/// FFmpeg libswresample conversion of exact 20 ms 48 kHz S16LE blocks to
/// exactly [`PCM44_FRAMES`] frames at 44.1 kHz (see `audio.c`). Its filter
/// history makes consecutive blocks one continuous signal, so it is reset only
/// where continuity already ended: actual sample loss (codec recovery) and
/// silence suppression. An ordinary credit wait keeps it.
struct Resampler {
    ptr: *mut c_void,
    channels: usize,
}
// Like the encoders, a single capture-worker owner after negotiation.
unsafe impl Send for Resampler {}
impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { niri_audio_resampler_free(self.ptr) };
    }
}
impl Resampler {
    fn new(channels: u16) -> anyhow::Result<Self> {
        let mut error = [0 as c_char; 256];
        let ptr = unsafe {
            niri_audio_resampler_new(i32::from(channels), error.as_mut_ptr(), error.len())
        };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(Self {
            ptr,
            channels: usize::from(channels),
        })
    }
    fn reset(&mut self) -> anyhow::Result<()> {
        let mut error = [0 as c_char; 256];
        let result =
            unsafe { niri_audio_resampler_reset(self.ptr, error.as_mut_ptr(), error.len()) };
        anyhow::ensure!(
            result == 0,
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(())
    }
    /// One 48 kHz block of this resampler's channel count in, one 44.1 kHz
    /// block of the same duration out.
    fn convert(&mut self, pcm: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut output = vec![0; PCM44_FRAMES * self.channels * 2];
        let mut error = [0 as c_char; 256];
        let length = unsafe {
            niri_audio_resample(
                self.ptr,
                pcm.as_ptr(),
                pcm.len(),
                output.as_mut_ptr(),
                output.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            usize::try_from(length).ok() == Some(output.len()),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(output)
    }
}

fn aac_format() -> AudioFormat {
    // GNOME's AAC_MS offer uses raw LC packets and no HEAACWAVEINFO blob.
    AudioFormat {
        format: WaveFormat::AAC_MS,
        n_channels: 2,
        n_samples_per_sec: 44100,
        n_avg_bytes_per_sec: 12000,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }
}

struct Adpcm {
    ptr: *mut c_void,
    format: AudioFormat,
}
// The FFmpeg context has one owner, moved to the capture worker after negotiation.
unsafe impl Send for Adpcm {}
impl Drop for Adpcm {
    fn drop(&mut self) {
        unsafe { niri_audio_free(self.ptr) };
    }
}
impl Adpcm {
    fn new() -> anyhow::Result<Self> {
        let mut error = [0 as c_char; 256];
        let ptr = unsafe { niri_audio_new(error.as_mut_ptr(), error.len()) };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        let mut encoder = Self {
            ptr,
            format: AudioFormat {
                format: WaveFormat::ADPCM,
                n_channels: 2,
                n_samples_per_sec: 48000,
                n_avg_bytes_per_sec: (48000 * ADPCM_BLOCK_BYTES / ADPCM_SAMPLES) as u32,
                n_block_align: ADPCM_BLOCK_BYTES as u16,
                bits_per_sample: 4,
                data: None,
            },
        };
        let mut extra = vec![0; 32];
        anyhow::ensure!(
            unsafe { niri_audio_extra(ptr, extra.as_mut_ptr(), extra.len()) } == 32,
            "invalid MS ADPCM format data"
        );
        encoder.format.data = Some(extra);
        Ok(encoder)
    }
    fn encode(&mut self, pcm: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut error = [0 as c_char; 256];
        let mut data = std::ptr::null();
        let length = unsafe {
            niri_audio_encode(
                self.ptr,
                pcm.as_ptr(),
                pcm.len(),
                &mut data,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            length == ADPCM_BLOCK_BYTES as i32 && !data.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
    }
}

enum Codec {
    /// Uncompressed S16LE from the 48 kHz stereo capture. `mono` downmixes;
    /// `resampler` converts to 44.1 kHz. Order: the worker has already applied
    /// any drift skip and fade to the 48 kHz stereo block; then downmix, then
    /// resample. Without a resampler, 48 kHz samples pass through unchanged.
    Pcm {
        mono: bool,
        resampler: Option<Resampler>,
    },
    Aac(Aac),
    Adpcm(Adpcm),
}
impl Codec {
    fn encode(&mut self, pcm: Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Self::Pcm { mono, resampler } => {
                let pcm = if *mono { downmix_to_mono(&pcm) } else { pcm };
                match resampler {
                    Some(resampler) => resampler.convert(&pcm).map(Some),
                    None => Ok(Some(pcm)),
                }
            }
            Self::Aac(encoder) => encoder.encode(&pcm),
            Self::Adpcm(encoder) => encoder.encode(&pcm).map(Some),
        }
    }
    /// After actual sample loss: discard state that spans the gap.
    fn reset(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Aac(encoder) => *encoder = Aac::new()?,
            Self::Pcm {
                resampler: Some(resampler),
                ..
            } => resampler.reset()?,
            _ => {}
        }
        Ok(())
    }
    /// Silence suppression began: the next sound is a new run after an
    /// unknown gap, so it starts from a freshly primed resampler. The silence
    /// tail already flushed the filter history; the only difference from
    /// continuing is that 0.75 ms of filter lookahead before the resumed
    /// onset stays silent. AAC keeps its encoder across suppression, as before.
    fn suppressed(&mut self) -> anyhow::Result<()> {
        if let Self::Pcm {
            resampler: Some(resampler),
            ..
        } = self
        {
            resampler.reset()?;
        }
        Ok(())
    }
}

/// S16LE PCM at `rate` with `channels` interleaved channels.
fn pcm_format_with(rate: u32, channels: u16) -> AudioFormat {
    let block_align = channels * 2;
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: channels,
        n_samples_per_sec: rate,
        n_avg_bytes_per_sec: rate * u32::from(block_align),
        n_block_align: block_align,
        bits_per_sample: 16,
        data: None,
    }
}

/// 48 kHz mono: opt-in with `NIRI_RDP_AUDIO_RATE=48000` and mono only.
fn pcm_mono_format() -> AudioFormat {
    pcm_format_with(48000, 1)
}

/// The 2026-10-12 stereo format (1.54 Mbit/s): the fallback after 44.1 kHz.
fn pcm_format() -> AudioFormat {
    pcm_format_with(48000, 2)
}

/// Preferred since 2026-10-20 (user-approved): 44.1 kHz stereo, 1.41 Mbit/s,
/// resampled from the 48 kHz capture. Windows 10 mstsc played 48 kHz PCM at
/// about 44.1 kHz: 25 s behind video after 5 min, and mono pitched up.
fn pcm44_format() -> AudioFormat {
    pcm_format_with(44100, 2)
}

/// 44.1 kHz mono (705.6 kbit/s): first only with `NIRI_RDP_AUDIO_CHANNELS=mono`.
fn pcm44_mono_format() -> AudioFormat {
    pcm_format_with(44100, 1)
}

/// The PCM part of the offer, most preferred first. Compressed fallbacks
/// follow it. With the default 44.1 kHz rate, 48 kHz stereo stays as the
/// next PCM fallback; `NIRI_RDP_AUDIO_RATE=48000` restores the former offer
/// exactly. Mono, when opted in, comes first at the selected rate.
fn pcm_offer(tuning: AudioTuning) -> Vec<AudioFormat> {
    let mut formats = Vec::new();
    if tuning.rate == 44100 {
        if tuning.mono {
            formats.push(pcm44_mono_format());
        }
        formats.push(pcm44_format());
    } else if tuning.mono {
        formats.push(pcm_mono_format());
    }
    formats.push(pcm_format());
    formats
}

fn sample_at(pcm: &[u8], index: usize) -> i32 {
    i32::from(i16::from_le_bytes([pcm[index * 2], pcm[index * 2 + 1]]))
}

/// Interleaved stereo S16LE to mono as (L+R)/2. The sum is formed in i32, so
/// it cannot overflow; halves round to nearest, ties away from zero, which is
/// symmetric about zero and adds no DC offset. The result always fits i16.
fn downmix_to_mono(stereo: &[u8]) -> Vec<u8> {
    stereo
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|frame| {
            let sum = sample_at(frame, 0) + sample_at(frame, 1);
            (((sum + sum.signum()) / 2) as i16).to_le_bytes()
        })
        .collect()
}

/// Mean absolute sample of an S16LE block: 0 for digital silence.
fn block_level(pcm: &[u8]) -> u32 {
    let samples = pcm.len() / 2;
    if samples == 0 {
        return 0;
    }
    let sum: u64 = (0..samples)
        .map(|index| u64::from(sample_at(pcm, index).unsigned_abs()))
        .sum();
    (sum / samples as u64) as u32
}

/// Begin `next` with the leading stereo frames of a dropped block, then blend
/// linearly into `next`'s own samples. Frame 0 equals the dropped block's first
/// frame, which continued the previously sent block, so the splice is smooth;
/// the last faded frame is within one fade step of `next` itself.
fn crossfade_into(next: &mut [u8], dropped_head: &[u8]) {
    let frames = (dropped_head.len() / 4).min(next.len() / 4);
    if frames == 0 {
        return;
    }
    let length = frames as i32;
    for frame in 0..frames {
        for channel in 0..2 {
            let index = frame * 2 + channel;
            let from = sample_at(dropped_head, index);
            let to = sample_at(next, index);
            let step = (to - from) * frame as i32;
            // Round to nearest; the result lies between `from` and `to`.
            let blended = from + (step + step.signum() * (length / 2)) / length;
            next[index * 2..index * 2 + 2].copy_from_slice(&(blended as i16).to_le_bytes());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkipReason {
    Quiet,
    Forced,
}
impl SkipReason {
    fn name(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Forced => "forced",
        }
    }
}

/// Server-side estimate of the surplus in the client's playback queue: audio
/// it holds beyond what just-in-time delivery would need. It simulates a client
/// that starts with an empty queue, plays each block in 20 ms plus the drift
/// allowance (a client clock that slow), pauses when empty and receives each
/// wave when its first Wave Confirm arrives. mstsc appears to confirm on
/// arrival, so this replays the jitter ratchet: after a stall, waves sent late
/// arrive together, and the client keeps the excess queued. (A client
/// confirming at playback would give smooth receipts: only the allowance.)
///
/// Only waves actually sent can be confirmed. Capture discarded as stale or
/// under pressure is never credited, and a sending gap (credit wait, worker
/// stall, late capture, a skip without a queued successor) drains the estimate
/// as it would the real queue. Sub-block schedule catch-up only refills what a
/// preceding gap drained. The queue estimate is the lowest queue seen just
/// before an arrival over the last one to two [`ARRIVAL_LOW_WATER_WINDOW`]s, so
/// clumped confirmations do not count as surplus. Four transport credits bound
/// one stall's burst to 80 ms less the round trip. Growth that receipts cannot
/// show is covered by the drift allowance and by a starvation credit: one block
/// when the model runs dry for [`STARVATION_MIN`], at most once per
/// [`STARVATION_CREDIT_SPACING`], held until a skip uses it or silence.
#[derive(Debug)]
struct ArrivalModel {
    /// PCM only: compressed fallbacks never skip, so they never feed it.
    enabled: bool,
    drift_ppm: u32,
    /// Waves sent before the latest silence suppression are not counted.
    since: Option<Instant>,
    /// When the simulated client would run out of received audio.
    play_end: Option<Instant>,
    window_start: Option<Instant>,
    window_low: Duration,
    previous_low: Option<Duration>,
    /// Starvation credit not yet used by a skip.
    hidden: Duration,
    last_starvation_credit: Option<Instant>,
}
impl Default for ArrivalModel {
    fn default() -> Self {
        Self {
            enabled: true,
            drift_ppm: DRIFT_ALLOWANCE_PPM,
            since: None,
            play_end: None,
            window_start: None,
            window_low: Duration::ZERO,
            previous_low: None,
            hidden: Duration::ZERO,
            last_starvation_credit: None,
        }
    }
}
impl ArrivalModel {
    /// Whether a wave sent at `sent_at` belongs to the current sound run.
    fn counts(&self, sent_at: Instant) -> bool {
        self.enabled && self.since.is_none_or(|since| sent_at >= since)
    }
    /// `blocks` waves were first confirmed at `now`. Returns how long the
    /// model had run dry when that was at least [`STARVATION_MIN`].
    fn received(&mut self, now: Instant, blocks: u32) -> Option<Duration> {
        if blocks == 0 {
            return None;
        }
        let starved = self
            .play_end
            .map(|end| now.saturating_duration_since(end))
            .filter(|starved| *starved >= STARVATION_MIN);
        if starved.is_some()
            && self
                .last_starvation_credit
                .is_none_or(|at| now >= at + STARVATION_CREDIT_SPACING)
        {
            self.hidden = (self.hidden + STARVATION_CREDIT).min(DRIFT_SKIP_EARNED_CAP);
            self.last_starvation_credit = Some(now);
        }
        let queued = self
            .play_end
            .map_or(Duration::ZERO, |end| end.saturating_duration_since(now))
            .min(DRIFT_SKIP_EARNED_CAP);
        match self.window_start {
            Some(start) if now < start + ARRIVAL_LOW_WATER_WINDOW => {
                self.window_low = self.window_low.min(queued);
            }
            started => {
                if started.is_some() {
                    self.previous_low = Some(self.window_low);
                }
                self.window_start = Some(now);
                self.window_low = queued;
            }
        }
        let per_block = BLOCK_DURATION + BLOCK_DURATION * self.drift_ppm / 1_000_000;
        self.play_end = Some(now + queued + per_block * blocks);
        starved
    }
    /// Observed queue surplus: zero until one full window has been seen.
    fn queue_low(&self) -> Duration {
        self.previous_low
            .map_or(Duration::ZERO, |previous| previous.min(self.window_low))
    }
    /// Estimated surplus: observed queue plus unused starvation credit.
    fn earned(&self) -> Duration {
        (self.queue_low() + self.hidden).min(DRIFT_SKIP_EARNED_CAP)
    }
    /// A block was dropped. Without a queued successor its gap shows in later
    /// receipts, but only as far as the observed queue reaches; the rest is
    /// charged to the starvation credit.
    fn skipped(&mut self) {
        let unobserved = BLOCK_DURATION.saturating_sub(self.queue_low());
        self.hidden = self.hidden.saturating_sub(unobserved);
    }
    /// Silence suppression stopped sending: the client queue drains to empty.
    fn reset(&mut self, now: Instant) {
        *self = Self {
            enabled: self.enabled,
            drift_ppm: self.drift_ppm,
            since: Some(now),
            last_starvation_credit: self.last_starvation_credit,
            ..Self::default()
        };
    }
}

/// Playback tuning, read once at frontend startup from the environment that
/// `/etc/niri-rdp/frontend.conf` supplies. The defaults are the accepted
/// baseline; invalid values warn and keep the default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioTuning {
    /// `NIRI_RDP_AUDIO_DRIFT_PPM`: drift allowance, 0..=2000, default 100.
    pub drift_ppm: u32,
    /// `NIRI_RDP_AUDIO_CHANNELS`: `stereo` (default) offers stereo PCM first
    /// and no mono; `mono` is an opt-in that offers mono PCM first. Mono
    /// sounded pitched-up and robotic in Windows 10 mstsc (2026-10-20).
    pub mono: bool,
    /// `NIRI_RDP_AUDIO_RATE`: `44100` (default) offers 44.1 kHz PCM,
    /// resampled in the bridge, ahead of 48 kHz stereo PCM; `48000` restores
    /// the former 48 kHz-first offer, which mstsc played about 8.8% slow.
    pub rate: u32,
}
impl Default for AudioTuning {
    fn default() -> Self {
        Self {
            drift_ppm: DRIFT_ALLOWANCE_PPM,
            mono: false,
            rate: 44100,
        }
    }
}
impl AudioTuning {
    pub fn from_env() -> Self {
        let read = |name| match std::env::var(name) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => Some("<not unicode>".into()),
        };
        let (tuning, warnings) = Self::parse(
            read("NIRI_RDP_AUDIO_DRIFT_PPM").as_deref(),
            read("NIRI_RDP_AUDIO_CHANNELS").as_deref(),
            read("NIRI_RDP_AUDIO_RATE").as_deref(),
        );
        for warning in warnings {
            eprintln!("{warning}");
        }
        tuning
    }
    fn parse(
        drift_ppm: Option<&str>,
        channels: Option<&str>,
        rate: Option<&str>,
    ) -> (Self, Vec<String>) {
        let mut tuning = Self::default();
        let mut warnings = Vec::new();
        if let Some(value) = drift_ppm {
            match value.trim().parse::<u64>() {
                Ok(ppm) if ppm <= u64::from(MAX_DRIFT_PPM) => tuning.drift_ppm = ppm as u32,
                Ok(_) => {
                    warnings.push(format!(
                        "RDP audio: NIRI_RDP_AUDIO_DRIFT_PPM={value} above {MAX_DRIFT_PPM}; using {MAX_DRIFT_PPM}"
                    ));
                    tuning.drift_ppm = MAX_DRIFT_PPM;
                }
                Err(_) => warnings.push(format!(
                    "RDP audio: NIRI_RDP_AUDIO_DRIFT_PPM={value} is not 0-{MAX_DRIFT_PPM}; using {DRIFT_ALLOWANCE_PPM}"
                )),
            }
        }
        match channels.map(str::trim) {
            None | Some("stereo") => {}
            Some("mono") => tuning.mono = true,
            Some(value) => warnings.push(format!(
                "RDP audio: NIRI_RDP_AUDIO_CHANNELS={value} is not mono or stereo; using stereo"
            )),
        }
        match rate.map(str::trim) {
            None | Some("44100") => {}
            Some("48000") => tuning.rate = 48000,
            Some(value) => warnings.push(format!(
                "RDP audio: NIRI_RDP_AUDIO_RATE={value} is not 44100 or 48000; using 44100"
            )),
        }
        (tuning, warnings)
    }
}

/// Earned catch-up for client playback-queue growth. During continuous
/// audible PCM, drop one 20 ms block when the [`ArrivalModel`] surplus is at
/// least [`DRIFT_SKIP_RESERVE`] plus one block, at most once per
/// [`DRIFT_SKIP_MIN_SPACING`], preferring a quiet block. Without a queued
/// successor, the skip leaves a 20 ms sending gap, and the estimate falls by
/// 20 ms once later waves are confirmed; the spacing exceeds any plausible
/// round trip. With one, the skip removes 20 ms of server-side capture backlog
/// instead and the client queue is unchanged. A skip is neither
/// capture loss nor a credit or pressure event: codec state, counters and the
/// send schedule are untouched; only one block of content is removed.
#[derive(Debug, Default)]
struct DriftSkip {
    /// Start of the current spacing interval: first send, resume or last skip.
    run_start: Option<Instant>,
    /// When the surplus first allowed a skip in this interval.
    armed_at: Option<Instant>,
    /// Recent mean level of audible sent blocks.
    average: Option<f32>,
}
impl DriftSkip {
    /// Whether to drop this block instead of sending it. Digital silence is
    /// never dropped: it belongs to the silence tail and suppression path.
    fn consider(&mut self, now: Instant, block: &[u8], earned: Duration) -> Option<SkipReason> {
        if now < self.run_start? + DRIFT_SKIP_MIN_SPACING || block.iter().all(|byte| *byte == 0) {
            return None;
        }
        if earned < DRIFT_SKIP_RESERVE + BLOCK_DURATION {
            self.armed_at = None;
            return None;
        }
        let armed_at = *self.armed_at.get_or_insert(now);
        let level = block_level(block);
        let quiet = level <= DRIFT_SKIP_QUIET_LEVEL
            || self
                .average
                .is_some_and(|average| level as f32 <= average * DRIFT_SKIP_QUIET_FRACTION);
        let reason = if quiet {
            SkipReason::Quiet
        } else if now >= armed_at + DRIFT_SKIP_MAX_WAIT {
            SkipReason::Forced
        } else {
            return None;
        };
        self.run_start = Some(now);
        self.armed_at = None;
        Some(reason)
    }
    /// A block was sent to the client.
    fn sent(&mut self, now: Instant, block: &[u8]) {
        self.run_start.get_or_insert(now);
        if block.iter().any(|byte| *byte != 0) {
            let level = block_level(block) as f32;
            self.average = Some(self.average.map_or(level, |average| {
                average + (level - average) * DRIFT_SKIP_AVERAGE_WEIGHT
            }));
        }
    }
    /// Silence suppression stopped sending; the client queue drains.
    fn suppressed(&mut self) {
        self.run_start = None;
        self.armed_at = None;
    }
    fn average(&self) -> u32 {
        self.average.unwrap_or(0.0) as u32
    }
}

/// Opt-in Wave Confirm details: does mstsc ever echo real playback time, and
/// how far apart are its first and second confirmations?
#[derive(Debug, Default)]
struct ConfirmDiagnostics {
    detail_lines: u32,
    nonzero_lines: u32,
    first: u64,
    second: u64,
    repeat: u64,
    unmatched: u64,
    nonzero: u64,
    last_nonzero: Option<(u8, u16)>,
    gaps_us: Vec<u64>,
    last_second: Option<(u8, u16, Duration)>,
    last_report: Option<Instant>,
    /// Latest drift-skip surplus estimate, reported with each summary.
    earned: Duration,
}
impl ConfirmDiagnostics {
    fn record(
        &mut self,
        number: u8,
        timestamp: u16,
        now: Instant,
        matched: Option<(Duration, Option<Duration>, bool)>,
    ) {
        let kind = match matched {
            None => {
                self.unmatched += 1;
                "unmatched"
            }
            Some((_, None, _)) => {
                self.first += 1;
                "first"
            }
            Some((_, Some(gap), false)) => {
                self.second += 1;
                if self.gaps_us.len() < 1024 {
                    self.gaps_us.push(gap.as_micros() as u64);
                }
                self.last_second = Some((number, timestamp, gap));
                "second"
            }
            Some((_, Some(_), true)) => {
                self.repeat += 1;
                "repeat"
            }
        };
        if timestamp != 0 {
            self.nonzero += 1;
            self.last_nonzero = Some((number, timestamp));
        }
        let detail = self.detail_lines < CONFIRM_DETAIL_LINES
            || (timestamp != 0 && self.nonzero_lines < CONFIRM_DETAIL_LINES);
        if detail {
            if self.detail_lines < CONFIRM_DETAIL_LINES {
                self.detail_lines += 1;
            } else {
                self.nonzero_lines += 1;
            }
            let (elapsed, gap) =
                matched.map_or((None, None), |(elapsed, gap, _)| (Some(elapsed), gap));
            eprintln!(
                "RDP audio confirm: block={number} timestamp={timestamp} kind={kind} sent_ms={} second_gap_ms={}",
                elapsed.map_or("none".into(), |at| format!(
                    "{:.1}",
                    at.as_secs_f64() * 1000.0
                )),
                gap.map_or("none".into(), |gap| format!(
                    "{:.1}",
                    gap.as_secs_f64() * 1000.0
                )),
            );
        }
        let began = *self.last_report.get_or_insert(now);
        if now.saturating_duration_since(began) >= CONFIRM_REPORT_INTERVAL {
            self.report(now.saturating_duration_since(began));
            self.last_report = Some(now);
        }
    }
    fn report(&mut self, interval: Duration) {
        self.gaps_us.sort_unstable();
        let gap = |index: usize| {
            self.gaps_us
                .get(index)
                .map_or("none".into(), |us| format!("{:.1}", *us as f64 / 1000.0))
        };
        let count = self.gaps_us.len();
        eprintln!(
            "RDP audio confirms: interval_ms={} first={} second={} repeat={} unmatched={} nonzero_timestamps={} last_nonzero={} second_gap_ms_min={} second_gap_ms_p50={} second_gap_ms_max={} last_second={} earned_ms={:.1}",
            interval.as_millis(),
            self.first,
            self.second,
            self.repeat,
            self.unmatched,
            self.nonzero,
            self.last_nonzero
                .map_or("none".into(), |(block, ts)| format!("{block}/{ts}")),
            gap(0),
            gap(count.saturating_sub(1) / 2),
            gap(count.saturating_sub(1)),
            self.last_second
                .map_or("none".into(), |(block, ts, gap)| format!(
                    "{block}/{ts}/{:.1}ms",
                    gap.as_secs_f64() * 1000.0
                )),
            self.earned.as_secs_f64() * 1000.0,
        );
        *self = Self {
            detail_lines: self.detail_lines,
            nonzero_lines: self.nonzero_lines,
            ..Self::default()
        };
    }
}

#[derive(Debug)]
struct SentWave {
    number: u8,
    timestamp: u16,
    sent_at: Instant,
    playback_epoch: u64,
    acknowledged: bool,
    first_confirm_at: Option<Instant>,
    feedback_ms: u16,
    feedback_elapsed: Duration,
    playback_confirmed: bool,
}

/// Receipt confirmations release transport credit. Playback confirmations are
/// optional delay measurements, never a second credit that a client must return.
/// Like GNOME Remote Desktop, only recent measurements can affect capture.
#[derive(Default, Debug)]
struct FlowControl {
    pending: usize,
    recent: VecDeque<SentWave>,
    hold_until: Option<Instant>,
    last_warning: Option<Instant>,
    latest_sent: Option<Instant>,
    playback_epoch: u64,
    receipt_baseline: Option<Duration>,
    last_render_delay: Duration,
    /// Receipt-time model of the client queue for the PCM drift skip.
    arrivals: ArrivalModel,
    /// Present only with `NIRI_RDP_DIAGNOSTICS=1`; observes, never decides.
    diagnostics: Option<ConfirmDiagnostics>,
}
impl FlowControl {
    const HISTORY: usize = 2048;
    const INITIAL_RECEIPT_BUDGET: Duration = Duration::from_millis(500);
    const DISTINCT_CONFIRM_INTERVAL: Duration = Duration::from_millis(10);
    const FEEDBACK_WINDOW: Duration = Duration::from_secs(1);

    fn receipt_budget(&self) -> Duration {
        self.receipt_baseline
            .map_or(Self::INITIAL_RECEIPT_BUDGET, |baseline| {
                (baseline + Duration::from_millis(100)).max(Duration::from_millis(150))
            })
    }
    fn receipt_age(&self, now: Instant) -> Duration {
        self.recent
            .iter()
            .find(|wave| !wave.acknowledged)
            .map_or(Duration::ZERO, |wave| {
                now.saturating_duration_since(wave.sent_at)
            })
    }
    fn render_age(&self, now: Instant) -> Duration {
        // An absent optional rendering echo is not evidence that sound is
        // still queued. In particular, silence or mixed single/double replies
        // must not turn an old block into minutes of fabricated playback delay.
        if self.hold_until.is_some_and(|until| now < until) {
            self.last_render_delay
        } else {
            Duration::ZERO
        }
    }
    fn can_send(&self, now: Instant) -> bool {
        self.pending < MAX_PENDING
            && self.hold_until.is_none_or(|until| now >= until)
            && self.receipt_age(now) <= self.receipt_budget()
    }
    fn sent(&mut self, number: u8, timestamp: u16, now: Instant) {
        // Reaching number reuse with four transport credits proves FIFO
        // dispatch reached this older wave. Retire a lost receipt credit, but
        // retain its timestamp so a delayed render echo can still be matched.
        if let Some(old) = self.recent.iter_mut().rfind(|wave| wave.number == number)
            && !old.acknowledged
        {
            old.acknowledged = true;
            self.pending -= 1;
        }
        if self.recent.len() == Self::HISTORY
            && let Some(old) = self.recent.pop_front()
            && !old.acknowledged
        {
            self.pending -= 1;
        }
        self.pending += 1;
        if self
            .latest_sent
            .is_some_and(|latest| now.saturating_duration_since(latest) > Self::FEEDBACK_WINDOW)
        {
            self.playback_epoch = self.playback_epoch.wrapping_add(1);
        }
        self.latest_sent = Some(now);
        self.recent.push_back(SentWave {
            number,
            timestamp,
            sent_at: now,
            playback_epoch: self.playback_epoch,
            acknowledged: false,
            first_confirm_at: None,
            feedback_ms: 0,
            feedback_elapsed: Duration::ZERO,
            playback_confirmed: false,
        });
    }
    fn warn_delay(&mut self, now: Instant, receipt: Duration, render: Duration) {
        if self
            .last_warning
            .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(5))
        {
            eprintln!(
                "RDP audio delay: receipt {} ms, render {} ms; dropping stale capture and reducing video bandwidth",
                receipt.as_millis(),
                render.as_millis(),
            );
            self.last_warning = Some(now);
        }
    }
    fn check_pressure(&mut self, now: Instant) -> bool {
        let receipt = self.receipt_age(now);
        let render = self.render_age(now);
        let draining = self.hold_until.is_some_and(|until| now < until);
        let congested = receipt > self.receipt_budget() || render > PLAYBACK_BUDGET || draining;
        if congested {
            self.warn_delay(
                now,
                receipt,
                render.max(if draining {
                    self.last_render_delay
                } else {
                    Duration::ZERO
                }),
            );
        }
        congested
    }
    fn confirm(&mut self, number: u8, timestamp: u16, now: Instant) {
        // With an immediate-playback zero reference, block numbers alone
        // identify the wave. Receipt belongs to the newest pending generation;
        // an optional later echo belongs to the oldest unfinished generation.
        // Its age is validated below before it affects current playback.
        let zero_reference = self
            .recent
            .iter()
            .filter(|wave| wave.number == number)
            .all(|wave| wave.timestamp == 0);
        // An exact echo identifies a nonzero-timestamp generation even after
        // wrap. A modular difference alone could attribute it to a reused ID.
        let position = if zero_reference {
            self.recent
                .iter()
                .rposition(|wave| wave.number == number && !wave.acknowledged)
                .or_else(|| {
                    self.recent.iter().position(|wave| {
                        wave.number == number && wave.acknowledged && !wave.playback_confirmed
                    })
                })
                .or_else(|| self.recent.iter().rposition(|wave| wave.number == number))
        } else {
            self.recent
                .iter()
                .rposition(|wave| wave.number == number && wave.timestamp == timestamp)
                .or_else(|| {
                    self.recent.iter().rposition(|wave| {
                        wave.number == number
                            && (wave.timestamp == 0
                                || timestamp.wrapping_sub(wave.timestamp) < 0x8000)
                    })
                })
        };
        if let Some(diagnostics) = &mut self.diagnostics {
            let matched = position.map(|position| {
                let wave = &self.recent[position];
                (
                    now.saturating_duration_since(wave.sent_at),
                    wave.first_confirm_at
                        .map(|first| now.saturating_duration_since(first)),
                    wave.playback_confirmed,
                )
            });
            diagnostics.earned = self.arrivals.earned();
            diagnostics.record(number, timestamp, now, matched);
        }
        let Some(position) = position else {
            return;
        };
        let wave = &self.recent[position];
        let held_ms = timestamp.wrapping_sub(wave.timestamp);
        let increased = held_ms > wave.feedback_ms;
        if wave.playback_confirmed && !increased {
            return;
        }
        let first_confirm_at = wave.first_confirm_at;
        let elapsed = now.saturating_duration_since(wave.sent_at);
        // A fresh completion can describe a wave queued for more than a
        // second. Keep it only while the same audio run is still active and
        // this block number has not been reused: with zero timestamps, a
        // newer generation's second echo could otherwise look like old delay.
        let active_generation = wave.playback_epoch == self.playback_epoch
            && self.latest_sent.is_some_and(|latest| {
                now.saturating_duration_since(latest) <= Self::FEEDBACK_WINDOW
            })
            && !self
                .recent
                .iter()
                .skip(position + 1)
                .any(|later| later.number == number);
        let distinct_second = first_confirm_at.is_some_and(|first| {
            now.saturating_duration_since(first) >= Self::DISTINCT_CONFIRM_INTERVAL
        });
        if first_confirm_at.is_none() {
            self.recent[position].first_confirm_at = Some(now);
            // A delayed first sample must not establish a multi-second healthy
            // baseline. Stable ordinary network RTT still receives its margin.
            let receipt = elapsed.min(Self::INITIAL_RECEIPT_BUDGET);
            self.receipt_baseline = Some(
                self.receipt_baseline
                    .map_or(receipt, |old| old.min(receipt)),
            );
            // Receipt of a later FIFO wave also proves earlier transport
            // events reached the client, even if one receipt ACK went missing.
            let mut received = 0;
            for old in self.recent.iter_mut().take(position + 1) {
                if !old.acknowledged {
                    old.acknowledged = true;
                    self.pending -= 1;
                    received += u32::from(self.arrivals.counts(old.sent_at));
                }
            }
            if let Some(starved) = self.arrivals.received(now, received) {
                use crate::performance::{Counter, connection};
                connection().add(Counter::AudioModelStarvations, 1);
                connection().add(Counter::AudioModelStarvedMs, starved.as_millis() as u64);
            }
        }
        // Even an immediate equal second packet finishes this generation.
        // It need not prove paired rendering behavior, but leaving it unfinished
        // would make healthy two-ACK clients accumulate ambiguous old records.
        let playback_confirmed = first_confirm_at.is_some();
        if playback_confirmed {
            for old in self.recent.iter_mut().take(position + 1) {
                old.playback_confirmed = true;
                if !old.acknowledged {
                    old.acknowledged = true;
                    self.pending -= 1;
                }
            }
        }
        // Never interpret a clock-like or wrapped timestamp as seconds of
        // buffering in a wave we sent milliseconds ago. A short allowance
        // permits clients that acknowledge scheduled playback ahead of time.
        // Old echoes can retire history but cannot restart capture recovery.
        let reported = Duration::from_millis(u64::from(held_ms));
        let plausible_timestamp = reported <= elapsed + PLAYBACK_BUDGET;
        let reported = if plausible_timestamp {
            reported
        } else {
            Duration::ZERO
        };
        let render = reported.max(if playback_confirmed && distinct_second {
            elapsed.saturating_sub(self.receipt_baseline.unwrap_or(Duration::ZERO))
        } else {
            Duration::ZERO
        });
        let wave = &mut self.recent[position];
        // Invalid timestamps must not hide a later valid latency report for
        // this same wave by becoming the highest remembered feedback value.
        wave.feedback_ms = wave.feedback_ms.max(reported.as_millis() as u16);
        let recent_feedback = elapsed <= Self::FEEDBACK_WINDOW
            || (active_generation && plausible_timestamp && playback_confirmed && distinct_second);
        if !recent_feedback || render <= wave.feedback_elapsed {
            return;
        }
        wave.feedback_elapsed = render;
        if render > PLAYBACK_BUDGET {
            // Bound recovery, then resume a consecutive stream of fresh
            // packets. AAC needs more than one priming packet to become audible.
            let until = (self.latest_sent.unwrap_or(now) + (render - PLAYBACK_BUDGET))
                .min(now + Self::FEEDBACK_WINDOW);
            if until > now {
                self.hold_until = Some(self.hold_until.map_or(until, |old| old.max(until)));
                self.last_render_delay = render;
                self.warn_delay(now, self.receipt_age(now), render);
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct Audio {
    socket: Arc<Mutex<Option<CaptureSocket>>>,
    wake: Arc<Wake>,
    video: Option<crate::video::Video>,
    clock: AudioClock,
    tuning: AudioTuning,
    /// Drift skips taken by this audio's workers, for exact test assertions.
    #[cfg(test)]
    skips: Arc<std::sync::atomic::AtomicU64>,
}
impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionAudio")
    }
}
impl Audio {
    pub fn with_video(mut self, video: crate::video::Video) -> Self {
        self.video = Some(video);
        self
    }
    pub fn with_tuning(mut self, tuning: AudioTuning) -> Self {
        self.tuning = tuning;
        self
    }
    pub fn attach(&self, socket: Option<UnixStream>) {
        if socket.is_none() {
            eprintln!(
                "RDP audio unavailable: desktop remains usable; reconnect after private audio recovery"
            );
        }
        *self.socket.lock().unwrap() = socket.map(|stream| CaptureSocket {
            stream,
            framed: true,
        });
        self.wake.notify();
    }
    pub fn clear(&self) {
        self.socket.lock().unwrap().take();
        self.wake.notify();
    }
    #[cfg(test)]
    fn attach_test_pcm(&self, stream: UnixStream) {
        *self.socket.lock().unwrap() = Some(CaptureSocket {
            stream,
            framed: false,
        });
        self.wake.notify();
    }
    pub fn factory(&self) -> Factory {
        Factory {
            audio: self.clone(),
            sender: None,
        }
    }
}

struct FeedbackGuard(Option<crate::video::AudioFeedback>);
impl Drop for FeedbackGuard {
    fn drop(&mut self) {
        if let Some(feedback) = &self.0 {
            feedback.clear();
        }
    }
}
pub struct Factory {
    audio: Audio,
    sender: Option<UnboundedSender<ServerEvent>>,
}
impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}
impl SoundServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        let aac = Aac::new()
            .map_err(|error| {
                eprintln!("RDP audio AAC-LC unavailable; offering PCM: {error}");
                error
            })
            .ok();
        let encoder = Adpcm::new()
            .map_err(|error| {
                eprintln!("RDP audio MS ADPCM unavailable; offering PCM: {error}");
                error
            })
            .ok();
        // mstsc advertises compressed formats that have rendered as static in
        // actual Windows playback. Prefer the PCM path verified by listening;
        // keep compressed codecs for clients that do not accept PCM. 44.1 kHz
        // stereo is the default (user-approved 2026-10-20): mstsc played
        // 48 kHz PCM about 8.8% slow, and mono 48 kHz pitched up. See
        // `pcm_offer` for the `NIRI_RDP_AUDIO_CHANNELS`/`_RATE` opt-ins.
        let mut formats = pcm_offer(self.audio.tuning);
        if aac.is_some() {
            formats.push(aac_format());
        }
        if let Some(encoder) = &encoder {
            formats.push(encoder.format.clone());
        }
        Box::new(Backend {
            audio: self.audio.clone(),
            sender: self.sender.clone().expect("sound factory sender"),
            formats,
            aac,
            encoder,
            stopped: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(Mutex::new(FlowControl {
                diagnostics: crate::performance::diagnostics_enabled()
                    .then(ConfirmDiagnostics::default),
                ..FlowControl::default()
            })),
            feedback: self
                .audio
                .video
                .as_ref()
                .map(crate::video::Video::audio_feedback),
            worker: None,
        })
    }
}
struct Backend {
    audio: Audio,
    sender: UnboundedSender<ServerEvent>,
    formats: Vec<AudioFormat>,
    aac: Option<Aac>,
    encoder: Option<Adpcm>,
    stopped: Arc<AtomicBool>,
    pending: Arc<Mutex<FlowControl>>,
    feedback: Option<crate::video::AudioFeedback>,
    worker: Option<thread::JoinHandle<()>>,
}
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionAudio")
    }
}
impl RdpsndServerHandler for Backend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }
    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        // `common` is our offer order, filtered to what the client accepted.
        // Log it once per channel start so the client's real list is visible.
        let describe = |format: &AudioFormat| {
            format!(
                "{}/{}ch/{}Hz/{}bit",
                format.format, format.n_channels, format.n_samples_per_sec, format.bits_per_sample
            )
        };
        let chosen = common.first();
        eprintln!(
            "RDP audio formats: client accepted [{}]; chosen {}",
            common
                .iter()
                .map(|format| describe(format.format()))
                .collect::<Vec<_>>()
                .join(", "),
            chosen.map_or("none".into(), |format| describe(format.format())),
        );
        chosen
    }
    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let mut encoder = if format.format().format == WaveFormat::AAC_MS {
            Codec::Aac(self.aac.take().ok_or_else(|| {
                Box::new(std::io::Error::other("AAC-LC encoder unavailable"))
                    as Box<dyn RdpsndError>
            })?)
        } else if format.format().format == WaveFormat::ADPCM {
            Codec::Adpcm(self.encoder.take().ok_or_else(|| {
                Box::new(std::io::Error::other("MS ADPCM encoder unavailable"))
                    as Box<dyn RdpsndError>
            })?)
        } else {
            // Branch on the negotiated PCM rate and channels: 44.1 kHz is
            // resampled from the 48 kHz capture; 48 kHz passes through.
            let negotiated = format.format();
            let channels = negotiated.n_channels;
            let resampler = match negotiated.n_samples_per_sec {
                48000 => None,
                44100 => Some(Resampler::new(channels).map_err(|error| {
                    Box::new(std::io::Error::other(error.to_string())) as Box<dyn RdpsndError>
                })?),
                rate => {
                    return Err(Box::new(std::io::Error::other(format!(
                        "unsupported PCM rate {rate}"
                    ))));
                }
            };
            Codec::Pcm {
                mono: channels == 1,
                resampler,
            }
        };
        self.aac.take();
        self.encoder.take();
        let block_bytes = if matches!(encoder, Codec::Adpcm(_)) {
            ADPCM_SAMPLES * 4
        } else {
            BLOCK_BYTES
        };
        // Capture-side size: 48 kHz stereo S16LE is 192000 bytes/s. Pacing is
        // per capture block, whatever the wire size (3528 bytes at 44.1 kHz).
        let block_duration = Duration::from_secs_f64(block_bytes as f64 / 192000.0);
        // AAC and ADPCM keep their unchanged fallback behavior: no drift skip.
        let drift_skip_enabled = matches!(encoder, Codec::Pcm { .. });
        let tuning = self.audio.tuning;
        {
            let mut flow = self.pending.lock().unwrap();
            flow.arrivals.enabled = drift_skip_enabled;
            flow.arrivals.drift_ppm = tuning.drift_ppm;
        }
        eprintln!(
            "RDP audio {}: {} Hz {}, {} kbps, {:.2} ms blocks; channels={} rate={} drift_ppm={} drift_skip={}",
            match &encoder {
                Codec::Aac(_) => "AAC-LC",
                Codec::Adpcm(_) => "MS ADPCM",
                Codec::Pcm { .. } => "PCM",
            },
            format.format().n_samples_per_sec,
            if format.format().n_channels == 1 {
                "mono"
            } else {
                "stereo"
            },
            format.format().n_avg_bytes_per_sec * 8 / 1000,
            block_duration.as_secs_f64() * 1000.0,
            if tuning.mono { "mono" } else { "stereo" },
            tuning.rate,
            tuning.drift_ppm,
            if drift_skip_enabled { "on" } else { "off" },
        );
        // Read-only record for the disconnect summary; selects nothing.
        crate::performance::connection().set_audio_codec(match &encoder {
            Codec::Aac(_) => crate::performance::AudioCodec::Aac,
            Codec::Adpcm(_) => crate::performance::AudioCodec::Adpcm,
            Codec::Pcm { .. } => crate::performance::AudioCodec::Pcm,
        });
        let audio = self.audio.clone();
        let sender = self.sender.clone();
        let stopped = self.stopped.clone();
        let pending = self.pending.clone();
        let feedback = self.feedback.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("rdp-audio".into())
                .spawn(move || {
                    let _feedback_guard = FeedbackGuard(feedback.clone());
                    // Negotiation may precede submission of credentials on the login form.
                    let wake = &audio.wake;
                    let clock = audio.clock.clone();
                    let socket = loop {
                        if stopped.load(Ordering::Acquire) {
                            return;
                        }
                        if let Some(socket) = audio.socket.lock().unwrap().take() {
                            break socket;
                        }
                        if !wake.wait(None, None, &*clock) { return; }
                    };
                    let mut socket = match CaptureReader::new(socket, clock.clone()) {
                        Ok(socket) => socket,
                        Err(error) => {
                            eprintln!("RDP audio capture unavailable: {error}; desktop continues, reconnect for a fresh descriptor");
                            return;
                        }
                    };
                    let start = clock.now();
                    let mut block = vec![0u8; block_bytes];
                    let mut captured = VecDeque::new();
                    let mut filled = 0;
                    let mut block_at: Option<Instant> = None;
                    let mut telemetry = crate::performance::Telemetry::from_env();
                    let mut credit_wait_start = None;
                    let mut number = 0u8;
                    let mut next_send = start;
                    let mut silent_blocks = 0usize;
                    let mut quiet_capture_blocks = 0usize;
                    let mut feedback_suspended = false;
                    let mut reset_codec = false;
                    let mut drift = DriftSkip::default();
                    // Leading frames of a drift-skipped block, faded into the next.
                    let mut fade_from: Option<Vec<u8>> = None;
                    'capture: while !stopped.load(Ordering::Acquire) {
                        // Keep a small FIFO for normal PipeWire quantum batching
                        // and scheduling jitter. Drop only capture exceeding the
                        // three-block budget, preventing replay after a stall.
                        // Keep partial blocks intact to preserve stereo alignment.
                        let mut drained = false;
                        for _ in 0..64 {
                            if stopped.load(Ordering::Acquire) {
                                break 'capture;
                            }
                            match socket.read(&mut block[filled..]) {
                                Ok((0, _)) => {
                                    eprintln!("RDP audio capture ended: desktop continues; reconnect for a fresh descriptor");
                                    break 'capture;
                                }
                                Ok((n, at)) => {
                                    if std::mem::take(&mut socket.discontinuity) {
                                        if filled > 0 {
                                            block.copy_within(filled..filled + n, 0);
                                            filled = 0;
                                            block_at = None;
                                        }
                                        reset_codec = true;
                                        telemetry.count("discarded_blocks", 1);
                                    }
                                    filled += n;
                                    block_at = Some(if socket.socket.framed { block_at.map_or(at, |old| old.min(at)) } else { at });
                                    telemetry.count("capture_reads", 1);
                                    telemetry.sample("monitor_source_age", clock.now().saturating_duration_since(at));
                                    telemetry.sample("relay_age", socket.relay_age);
                                    telemetry.sample("socket_age", socket.socket_age);
                                },
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    drained = true;
                                    break;
                                }
                                Err(_) => break 'capture,
                            }
                            if filled == block_bytes {
                                quiet_capture_blocks = if block.iter().all(|sample| *sample == 0) {
                                    quiet_capture_blocks.saturating_add(1)
                                } else {
                                    0
                                };
                                if captured.len() == MAX_CAPTURE_BLOCKS {
                                    captured.pop_front();
                                    reset_codec = true;
                                    telemetry.count("discarded_blocks", 1);
                                }
                                captured.push_back((block.clone(), block_at.take().unwrap()));
                                filled = 0;
                            }
                        }
                        if !drained {
                            continue;
                        }
                        let now = clock.now();
                        let mut queue = pending.lock().unwrap();
                        let pressure = queue.check_pressure(now);
                        let transport_pressure = queue.receipt_age(now) > queue.receipt_budget();
                        // Idle sound must not lower healthy video. Transport
                        // congestion remains actionable; recent render feedback
                        // expires without requiring a second reply for every wave.
                        if quiet_capture_blocks > SILENCE_TAIL_BLOCKS && !transport_pressure {
                            if !feedback_suspended && let Some(feedback) = &feedback {
                                feedback.clear();
                            }
                            feedback_suspended = true;
                        } else {
                            feedback_suspended = false;
                            if pressure && let Some(feedback) = &feedback {
                                feedback.report_congestion(now);
                            }
                        }
                        while captured.front().is_some_and(|(_, at)| {
                            now.saturating_duration_since(*at) > CAPTURE_BUDGET
                        }) {
                            captured.pop_front();
                            reset_codec = true;
                            telemetry.count("discarded_blocks", 1);
                        }
                        if !queue.can_send(now) {
                            telemetry.count("credit_waits", 1);
                            credit_wait_start.get_or_insert(now);
                            // A full credit window can be an ordinary ACK wait.
                            // Keep its bounded recent capture and codec overlap;
                            // only discard samples during actual delay recovery.
                            if pressure && !captured.is_empty() {
                                telemetry.count("discarded_blocks", captured.len() as u64);
                                captured.clear();
                                reset_codec = true;
                            }
                        } else if now >= next_send
                            && let Some((mut data, captured_at)) = captured.pop_front()
                        {
                            if let Some(at) = credit_wait_start.take() {
                                telemetry.sample("credit_wait", now.saturating_duration_since(at));
                            }
                            telemetry.sample("sample_age_at_send", now.saturating_duration_since(captured_at));
                            if reset_codec {
                                telemetry.count("codec_recoveries", 1);
                                if let Err(error) = encoder.reset() {
                                    eprintln!("RDP audio recovery stopped: {error}");
                                    break;
                                }
                                reset_codec = false;
                                // Lost samples already break continuity. The
                                // arrival model stays: discarded capture was
                                // never sent, and any sending gap it caused
                                // already shows in later confirmations.
                                fade_from = None;
                            }
                            let earned = queue.arrivals.earned();
                            if drift_skip_enabled && let Some(reason) = drift.consider(now, &data, earned) {
                                // Drop this block's content but keep the send
                                // schedule: a queued successor takes this tick;
                                // otherwise the next block leaves on arrival.
                                telemetry.count("drift_skips", 1);
                                queue.arrivals.skipped();
                                #[cfg(test)]
                                audio.skips.fetch_add(1, Ordering::Relaxed);
                                if telemetry.enabled() {
                                    eprintln!(
                                        "RDP audio drift skip: reason={} level={} average={} earned_ms={:.1} next_block={number} successor={}",
                                        reason.name(),
                                        block_level(&data),
                                        drift.average(),
                                        earned.as_secs_f64() * 1000.0,
                                        if captured.is_empty() { "arriving" } else { "queued" },
                                    );
                                }
                                data.truncate(DRIFT_SKIP_FADE_FRAMES * 4);
                                fade_from = Some(data);
                                silent_blocks = 0;
                                match captured.pop_front() {
                                    Some((next, _)) => data = next,
                                    None => continue,
                                }
                            }
                            if let Some(head) = fade_from.take() {
                                crossfade_into(&mut data, &head);
                            }
                            silent_blocks = if data.iter().all(|sample| *sample == 0) {
                                silent_blocks.saturating_add(1)
                            } else {
                                0
                            };
                            next_send = if now.duration_since(next_send) >= block_duration {
                                now + block_duration
                            } else {
                                next_send + block_duration
                            };
                            // Finish the codec tail, then avoid feeding a silent
                            // Windows playback queue for the entire idle session.
                            if silent_blocks > SILENCE_TAIL_BLOCKS {
                                drift.suppressed();
                                queue.arrivals.reset(now);
                                if silent_blocks == SILENCE_TAIL_BLOCKS + 1
                                    && let Err(error) = encoder.suppressed()
                                {
                                    eprintln!("RDP audio recovery stopped: {error}");
                                    break;
                                }
                                continue;
                            }
                            drift.sent(now, &data);
                            // Drift skip, fade and levels above all act on the
                            // 48 kHz stereo capture block; only now does the
                            // codec downmix and/or resample it.
                            let encode_started = Instant::now();
                            let data = match encoder.encode(data) {
                                Ok(Some(data)) => data,
                                Ok(None) => continue, // Bounded AAC frame assembly/lookahead.
                                Err(error) => {
                                    eprintln!("RDP audio encoding stopped: {error}");
                                    break;
                                }
                            };
                            telemetry.sample("encode", encode_started.elapsed());
                            telemetry.count("wire_payload_bytes", data.len() as u64);
                            // The graphics path has no RDPEVOR presentation
                            // clock to pair with Wave2. Match GNOME Remote
                            // Desktop's immediate playback timestamps rather
                            // than giving mstsc an unrelated boot clock.
                            queue.sent(number, 0, now);
                            number = number.wrapping_add(1);
                            if sender
                                .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, 0)))
                                .is_err()
                            {
                                break;
                            }
                            telemetry.count("waves_sent", 1);
                        }
                        // Capture readiness, exact pacing/expiry, feedback, and
                        // stop are the only wake sources. No fixed idle polling.
                        let mut deadline = captured.front().map(|(_, at)| *at + CAPTURE_BUDGET + Duration::from_millis(1));
                        if !captured.is_empty() {
                            let send_at = if queue.can_send(now) { Some(next_send) } else { queue.hold_until.filter(|until| *until > now) };
                            if let Some(send_at) = send_at {
                                deadline = Some(deadline.map_or(send_at, |old| old.min(send_at)));
                            }
                            if !pressure && let Some(oldest) = queue.recent.iter().find(|wave| !wave.acknowledged) {
                                let pressure_at = oldest.sent_at + queue.receipt_budget() + Duration::from_millis(1);
                                deadline = Some(deadline.map_or(pressure_at, |old| old.min(pressure_at)));
                            }
                        }
                        drop(queue);
                        telemetry.report("audio", now);
                        if !wake.wait(Some(&socket.socket.stream), deadline, &*clock) { break; }
                        telemetry.count("readiness_wakes", 1);
                    }
                })
                .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?,
        );
        Ok(())
    }
    fn wave_confirm(&mut self, block_no: u8, timestamp: u16) {
        self.pending
            .lock()
            .unwrap()
            .confirm(block_no, timestamp, self.audio.clock.now());
        self.audio.wake.notify();
    }
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.audio.wake.notify();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.audio.clear();
        if let Some(feedback) = &self.feedback {
            feedback.clear();
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::{decode, encode_vec};
    use ironrdp_rdpsnd::{pdu::*, server::RdpsndServer};
    use ironrdp_svc::SvcProcessor;
    use std::io::Write;

    type Events = tokio::sync::mpsc::UnboundedReceiver<ServerEvent>;

    /// Virtual time for the capture worker. It moves only when a test
    /// advances it, so scheduler load cannot expire capture or skip pacing.
    struct ManualClock {
        start: Instant,
        start_ns: u64,
        elapsed: Mutex<Duration>,
    }
    impl ManualClock {
        fn new() -> Self {
            Self {
                start: Instant::now(),
                start_ns: monotonic_ns(),
                elapsed: Mutex::new(Duration::ZERO),
            }
        }
    }
    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.start + *self.elapsed.lock().unwrap()
        }
        fn monotonic_ns(&self) -> u64 {
            self.start_ns + self.elapsed.lock().unwrap().as_nanos() as u64
        }
        fn poll_timeout(&self, deadline: Instant) -> i32 {
            // Future deadlines wait for the test to advance time (and wake).
            if deadline <= self.now() { 0 } else { -1 }
        }
    }

    /// Session audio driven by a manual clock. Every step lets the worker
    /// finish all due work before time moves again.
    struct Rig {
        audio: Audio,
        clock: Arc<ManualClock>,
    }
    impl Rig {
        fn new() -> Self {
            let clock = Arc::new(ManualClock::new());
            let audio = Audio {
                clock: AudioClock(clock.clone()),
                ..Audio::default()
            };
            Self { audio, clock }
        }
        fn settle(&self) {
            self.audio.wake.settle();
        }
        fn advance(&self, duration: Duration) {
            self.advance_with(duration, || {});
        }
        /// Advance in 1 ms steps, running `each` after the worker settles at
        /// the current time and again after every step.
        fn advance_with(&self, duration: Duration, mut each: impl FnMut()) {
            self.settle();
            each();
            let mut remaining = duration;
            while !remaining.is_zero() {
                let step = remaining.min(Duration::from_millis(1));
                *self.clock.elapsed.lock().unwrap() += step;
                remaining -= step;
                self.settle();
                each();
            }
        }
        /// Attach unframed test capture and return its source end once the
        /// worker has sent its readiness byte.
        fn attach_pcm(&self) -> UnixStream {
            let (mut source, capture) = UnixStream::pair().unwrap();
            source
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            self.audio.attach_test_pcm(capture);
            let mut ready = [0];
            source.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"A");
            source
        }
        /// The next wave, allowing at most one virtual second of pacing.
        fn take_wave(&self, receiver: &mut Events) -> (Vec<u8>, u32) {
            self.settle();
            for _ in 0..1000 {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    return (data, timestamp);
                }
                self.advance(Duration::from_millis(1));
            }
            panic!("expected an audio wave within one virtual second");
        }
    }

    fn confirm(server: &mut RdpsndServer, block_no: u8, timestamp: u16) {
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no,
                    timestamp,
                }))
                .unwrap(),
            )
            .unwrap();
    }

    /// Dispatch a wave through the RDPSND server and return its Wave2 PDU.
    fn wave2(server: &mut RdpsndServer, data: Vec<u8>, timestamp: u32) -> Wave2Pdu<'static> {
        let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
        let ServerAudioOutputPdu::Wave2(wave) =
            decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
        else {
            panic!("expected Wave2");
        };
        wave
    }

    fn negotiate(audio: &Audio, formats: Vec<AudioFormat>) -> (RdpsndServer, Events) {
        negotiate_version(audio, formats, Version::V8)
    }

    fn negotiate_version(
        audio: &Audio,
        formats: Vec<AudioFormat>,
        version: Version,
    ) -> (RdpsndServer, Events) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut factory = audio.factory();
        factory.set_sender(sender);
        let mut server = RdpsndServer::new(factory.build_backend());
        server.start().unwrap();
        for pdu in [
            ClientAudioOutputPdu::AudioFormat(ClientAudioFormatPdu {
                version,
                flags: AudioFormatFlags::ALIVE,
                formats,
                volume_left: u16::MAX,
                volume_right: u16::MAX,
                pitch: 0,
                dgram_port: 0,
            }),
            ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            }),
            ClientAudioOutputPdu::TrainingConfirm(TrainingConfirmPdu {
                timestamp: 4231,
                pack_size: 0,
            }),
        ] {
            server.process(&encode_vec(&pdu).unwrap()).unwrap();
        }
        (server, receiver)
    }

    fn pcm() -> AudioFormat {
        pcm_format()
    }

    fn private_frame(clock: &dyn Clock, sequence: u64, source_ns: u64, pcm: &[u8]) -> Vec<u8> {
        assert_eq!(pcm.len(), BLOCK_BYTES);
        let mut frame = CAPTURE_MAGIC.to_vec();
        frame.extend_from_slice(&(BLOCK_BYTES as u32).to_be_bytes());
        frame.extend_from_slice(&sequence.to_be_bytes());
        frame.extend_from_slice(&source_ns.to_be_bytes());
        frame.extend_from_slice(&clock.monotonic_ns().to_be_bytes());
        frame.extend_from_slice(pcm);
        frame
    }

    #[test]
    fn private_capture_metadata_survives_fragmentation_and_sequence_wrap() {
        let clock = Arc::new(ManualClock::new());
        let (mut source, capture) = UnixStream::pair().unwrap();
        let mut reader = CaptureReader::new(
            CaptureSocket {
                stream: capture,
                framed: true,
            },
            AudioClock(clock.clone()),
        )
        .unwrap();
        let mut hello = [0; 4];
        source.read_exact(&mut hello).unwrap();
        assert_eq!(&hello, CAPTURE_MAGIC);
        let at = clock.monotonic_ns() - 35_000_000;
        let pcm = (0..BLOCK_BYTES)
            .map(|index| index as u8)
            .collect::<Vec<_>>();
        let frame = private_frame(&*clock, u64::MAX, at, &pcm);
        source.write_all(&frame[..7]).unwrap();
        let mut data = [0; BLOCK_BYTES];
        assert_eq!(
            reader.read(&mut data).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        source
            .write_all(&frame[7..CAPTURE_HEADER_BYTES + 5])
            .unwrap();
        assert_eq!(
            reader.read(&mut data).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        source
            .write_all(&frame[CAPTURE_HEADER_BYTES + 5..])
            .unwrap();
        let (length, observed) = reader.read(&mut data).unwrap();
        assert_eq!(length, BLOCK_BYTES);
        assert_eq!(data.as_slice(), pcm);
        assert_eq!(
            clock.now().duration_since(observed),
            Duration::from_millis(35),
            "capture time is the helper's source time, not receipt time"
        );
        assert!(!reader.discontinuity);
        source
            .write_all(&private_frame(&*clock, 0, clock.monotonic_ns(), &pcm))
            .unwrap();
        reader.read(&mut data).unwrap();
        assert!(!reader.discontinuity, "sequence wrap is healthy continuity");
        source
            .write_all(&private_frame(&*clock, 5, clock.monotonic_ns(), &pcm))
            .unwrap();
        reader.read(&mut data).unwrap();
        assert!(
            reader.discontinuity,
            "relay sample loss must reset codec overlap"
        );
    }

    #[test]
    fn malformed_private_metadata_cannot_be_decoded_as_pcm() {
        for field in [0, 4, 16] {
            let (mut source, capture) = UnixStream::pair().unwrap();
            let mut reader = CaptureReader::new(
                CaptureSocket {
                    stream: capture,
                    framed: true,
                },
                AudioClock::default(),
            )
            .unwrap();
            let mut frame =
                private_frame(&SystemClock, 0, monotonic_ns(), &vec![0x33; BLOCK_BYTES]);
            frame[field..field + 4].fill(0xff);
            source.write_all(&frame).unwrap();
            let mut destination = [0; BLOCK_BYTES];
            assert_eq!(
                reader.read(&mut destination).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
            assert_eq!(destination, [0; BLOCK_BYTES]);
        }
    }

    #[test]
    fn whole_pipeline_expiry_discards_pcm_received_after_frontend_stall() {
        let rig = Rig::new();
        let (server, mut receiver) = negotiate(&rig.audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let stale = vec![0x22; BLOCK_BYTES];
        source
            .write_all(&private_frame(
                &*rig.clock,
                0,
                rig.clock.monotonic_ns() - 100_000_000,
                &stale,
            ))
            .unwrap();
        rig.audio.attach(Some(capture));
        source.read_exact(&mut [0; 4]).unwrap();
        rig.advance(Duration::from_millis(30));
        assert!(
            receiver.is_empty(),
            "receipt time must not renew stale source samples"
        );
        let fresh = vec![0x55; BLOCK_BYTES];
        source
            .write_all(&private_frame(
                &*rig.clock,
                1,
                rig.clock.monotonic_ns(),
                &fresh,
            ))
            .unwrap();
        assert_eq!(rig.take_wave(&mut receiver), (fresh, 0));
        drop(server);
    }

    #[test]
    fn worker_stop_interrupts_indefinite_capture_and_attachment_waits() {
        // Production clock: once idle, the worker must block without any
        // timeout (no polling), and stopping must still interrupt that wait.
        for attach in [false, true] {
            let audio = Audio::default();
            let (server, _) = negotiate(&audio, vec![pcm()]);
            let mut source = None;
            if attach {
                let (mut stream, capture) = UnixStream::pair().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                audio.attach(Some(capture));
                stream.read_exact(&mut [0; 4]).unwrap();
                source = Some(stream);
            }
            assert_eq!(
                audio.wake.settle(),
                -1,
                "quiet capture and unauthenticated waits must not poll"
            );
            let started = Instant::now();
            drop(server);
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "stop must interrupt an indefinite wait"
            );
            if let Some(mut source) = source {
                assert_eq!(source.read(&mut [0]).unwrap(), 0);
            }
        }
    }

    #[test]
    fn aac_fallback_uses_raw_packets_and_immediate_timestamps() {
        for version in [Version::V6, Version::V8] {
            let rig = Rig::new();
            let adpcm = Adpcm::new().unwrap().format.clone();
            let (mut server, mut receiver) =
                negotiate_version(&rig.audio, vec![adpcm, aac_format()], version);
            assert!(server.is_ready());
            let mut source = rig.attach_pcm();
            source.write_all(&vec![0x35; BLOCK_BYTES * 3]).unwrap();
            let (data, timestamp) = rig.take_wave(&mut receiver);
            assert!(
                data.len() < BLOCK_BYTES / 3,
                "AAC must reserve audio bandwidth"
            );
            assert_eq!(timestamp, 0);
            let messages: Vec<_> = server.wave(data.clone(), timestamp).unwrap().into();
            match decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap() {
                ServerAudioOutputPdu::Wave2(wave) => {
                    assert_eq!(messages.len(), 1);
                    assert_eq!(wave.format_no, 1);
                    assert_eq!(wave.timestamp, 0);
                    assert_eq!(wave.audio_timestamp, 0);
                    assert_eq!(wave.data.as_ref(), data);
                }
                ServerAudioOutputPdu::Wave(info) => {
                    assert_eq!(messages.len(), 2);
                    assert_eq!(info.format_no, 1);
                    assert_eq!(info.timestamp, 0);
                    let payload = messages[1].encode_unframed_pdu().unwrap();
                    let mut reconstructed = info.data_prefix.to_vec();
                    reconstructed.extend_from_slice(&payload[4..]);
                    assert_eq!(
                        reconstructed, data,
                        "raw AAC must not gain headers or padding"
                    );
                }
                _ => panic!("expected an audio wave"),
            }
            drop(server);
            assert_eq!(source.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn digital_silence_stops_idle_traffic_and_resumes_current_pcm() {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![pcm()]);
        let mut source = rig.attach_pcm();
        let mut sent = 0usize;
        for _ in 0..SILENCE_TAIL_BLOCKS + 5 {
            source.write_all(&vec![0; BLOCK_BYTES]).unwrap();
            rig.advance(Duration::from_millis(25));
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                assert_eq!(data, vec![0; BLOCK_BYTES]);
                let wave = wave2(&mut server, data, timestamp);
                confirm(&mut server, wave.block_no, 0);
                sent += 1;
            }
        }
        assert_eq!(sent, SILENCE_TAIL_BLOCKS);
        rig.settle();
        assert!(
            receiver.is_empty(),
            "idle silence must not fill client playback buffers"
        );
        source.write_all(&vec![0x33; BLOCK_BYTES]).unwrap();
        assert_eq!(
            rig.take_wave(&mut receiver).0,
            vec![0x33; BLOCK_BYTES],
            "current sound must resume after silence"
        );
        drop(server);
    }

    #[test]
    fn aac_transport_recovery_discards_codec_lookahead_before_fresh_sound() {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![aac_format()]);
        let mut source = rig.attach_pcm();
        // Withhold receipt credit until the transport queue is full. Continue
        // capturing so recovery must discard both old PCM and AAC lookahead.
        for _ in 0..12 {
            source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
            rig.advance(Duration::from_millis(25));
        }
        for _ in 0..80 {
            if receiver.len() >= MAX_PENDING {
                break;
            }
            source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
            rig.advance(Duration::from_millis(25));
        }
        assert_eq!(
            receiver.len(),
            MAX_PENDING,
            "AAC must fill bounded transport credit"
        );
        let mut tail = 0;
        for _ in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected AAC wave")
            };
            tail = wave2(&mut server, data, timestamp).block_no;
        }
        source.write_all(&vec![0x22; BLOCK_BYTES * 20]).unwrap();
        rig.advance(Duration::from_millis(80));
        assert!(
            receiver.is_empty(),
            "unconfirmed transport cannot queue more sound"
        );
        confirm(&mut server, tail, 0);
        let fresh = vec![0x33; BLOCK_BYTES];
        let mut encoder = Aac::new().unwrap();
        let expected = (0..3)
            .find_map(|_| encoder.encode(&fresh).unwrap())
            .unwrap();
        source.write_all(&fresh.repeat(3)).unwrap();
        assert_eq!(
            rig.take_wave(&mut receiver).0,
            expected,
            "recovery packet must contain only fresh codec state"
        );
        drop(server);
    }

    #[test]
    fn aac_transport_credit_wait_without_capture_preserves_codec_state() {
        aac_transport_credit_wait_preserves_codec_state(false);
    }

    #[test]
    fn aac_short_transport_credit_wait_preserves_queued_capture() {
        aac_transport_credit_wait_preserves_codec_state(true);
    }

    fn aac_transport_credit_wait_preserves_codec_state(queued_capture: bool) {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![aac_format()]);
        let mut source = rig.attach_pcm();
        let take_wave = |receiver: &mut Events| {
            let (data, timestamp) = rig.take_wave(receiver);
            assert_eq!(timestamp, 0);
            data
        };
        let initial = vec![0x11; BLOCK_BYTES];
        let mut reference = Aac::new().unwrap();
        let mut sent = 0;
        let mut tail = 0;
        // Fill transport credit with paced capture so no sample was dropped.
        while sent < MAX_PENDING {
            let expected = reference.encode(&initial).unwrap();
            source.write_all(&initial).unwrap();
            if let Some(expected) = expected {
                let data = take_wave(&mut receiver);
                assert_eq!(data, expected);
                tail = wave2(&mut server, data, 0).block_no;
                sent += 1;
            }
            rig.advance(Duration::from_millis(25));
        }
        // Capture stays empty while the fourth receipt is pending. Waiting
        // for credit alone must not discard AAC's buffered samples/lookahead.
        rig.advance(Duration::from_millis(30));
        assert!(receiver.is_empty());
        let fresh = vec![0x33; BLOCK_BYTES];
        let mut expected = VecDeque::new();
        if queued_capture {
            // One fresh block waits within the 60 ms capture budget. Four
            // occupied credits alone do not make these samples stale.
            let queued = vec![0x22; BLOCK_BYTES];
            if let Some(packet) = reference.encode(&queued).unwrap() {
                expected.push_back(packet);
            }
            source.write_all(&queued).unwrap();
            rig.advance(Duration::from_millis(15));
            assert!(receiver.is_empty());
        }
        confirm(&mut server, tail, 0);
        for _ in 0..3 {
            if let Some(packet) = reference.encode(&fresh).unwrap() {
                expected.push_back(packet);
            }
            source.write_all(&fresh).unwrap();
            rig.advance(Duration::from_millis(25));
        }
        assert!(!expected.is_empty());
        for expected in expected {
            let data = take_wave(&mut receiver);
            assert!(
                data == expected,
                "short credit waiting must preserve queued samples and consecutive AAC packets without codec priming"
            );
        }
        drop(server);
    }

    #[test]
    fn aac_idle_resume_keeps_audible_packets_after_missing_tail_feedback() {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![aac_format()]);
        let mut source = rig.attach_pcm();
        let take_wave = |receiver: &mut Events| {
            let (data, timestamp) = rig.take_wave(receiver);
            assert_eq!(timestamp, 0);
            data
        };
        let acknowledge = |server: &mut RdpsndServer, data: Vec<u8>| {
            let wave = wave2(server, data, 0);
            confirm(server, wave.block_no, 0);
            wave.block_no
        };
        let mut reference = Aac::new().unwrap();
        let initial = vec![0x11; BLOCK_BYTES];
        let first_expected = (0..3)
            .find_map(|_| reference.encode(&initial).unwrap())
            .unwrap();
        source.write_all(&initial.repeat(3)).unwrap();
        let first_data = take_wave(&mut receiver);
        assert_eq!(first_data, first_expected);
        let first = acknowledge(&mut server, first_data);
        // Establish a client that sometimes sends a separate render echo.
        rig.advance(Duration::from_millis(20));
        confirm(&mut server, first, 0);
        let tail_expected = reference.encode(&initial).unwrap().unwrap();
        source.write_all(&initial).unwrap();
        let tail = take_wave(&mut receiver);
        assert_eq!(tail, tail_expected);
        acknowledge(&mut server, tail);

        // Flush the bounded codec tail, then stop idle silence traffic. Every
        // later wave receives one confirmation; no final render echo follows.
        let silence = vec![0; BLOCK_BYTES];
        let mut expected = VecDeque::new();
        for block in 0..SILENCE_TAIL_BLOCKS + 3 {
            if block < SILENCE_TAIL_BLOCKS
                && let Some(packet) = reference.encode(&silence).unwrap()
            {
                expected.push_back(packet);
            }
            source.write_all(&silence).unwrap();
            rig.advance(Duration::from_millis(25));
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _))) =
                receiver.try_recv()
            {
                assert_eq!(Some(data.clone()), expected.pop_front());
                acknowledge(&mut server, data);
            }
        }
        while let Some(packet) = expected.pop_front() {
            let data = take_wave(&mut receiver);
            assert_eq!(data, packet);
            acknowledge(&mut server, data);
        }
        rig.advance(Duration::from_millis(550));
        assert!(receiver.is_empty(), "idle capture must stay silent");

        let fresh = vec![0x33; BLOCK_BYTES];
        let mut resumed_packets = 0;
        for _ in 0..12 {
            if let Some(packet) = reference.encode(&fresh).unwrap() {
                expected.push_back(packet);
            }
            source.write_all(&fresh).unwrap();
            rig.advance(Duration::from_millis(25));
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _))) =
                receiver.try_recv()
            {
                assert_eq!(
                    Some(data.clone()),
                    expected.pop_front(),
                    "missing render feedback must preserve fresh AAC samples"
                );
                acknowledge(&mut server, data);
                resumed_packets += 1;
            }
        }
        while let Some(packet) = expected.pop_front() {
            let data = take_wave(&mut receiver);
            assert_eq!(data, packet);
            acknowledge(&mut server, data);
            resumed_packets += 1;
        }
        assert!(
            resumed_packets >= 8,
            "resume must deliver audible AAC beyond the first priming packet"
        );
        drop(server);
    }

    #[test]
    fn aac_capture_overflow_discards_pre_gap_codec_state() {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![aac_format()]);
        let mut source = rig.attach_pcm();
        source.write_all(&vec![0x11; BLOCK_BYTES * 3]).unwrap();
        let (data, timestamp) = rig.take_wave(&mut receiver);
        let wave = wave2(&mut server, data, timestamp);
        confirm(&mut server, wave.block_no, 0);
        let fresh = vec![0x33; BLOCK_BYTES];
        let mut encoder = Aac::new().unwrap();
        let expected = (0..3)
            .find_map(|_| encoder.encode(&fresh).unwrap())
            .unwrap();
        source
            .write_all(&fresh.repeat(MAX_CAPTURE_BLOCKS + 5))
            .unwrap();
        assert_eq!(
            rig.take_wave(&mut receiver).0,
            expected,
            "dropping capture must also drop earlier codec lookahead"
        );
        drop(server);
    }

    #[test]
    fn stereo_pcm_fallback_preferred_over_aac_and_adpcm_preserves_signed_stereo_on_both_wave_versions()
     {
        for version in [Version::V6, Version::V8] {
            let rig = Rig::new();
            let adpcm = Adpcm::new().unwrap().format.clone();
            let mut unsupported = pcm();
            unsupported.n_samples_per_sec = 22050;
            let (mut server, mut receiver) = negotiate_version(
                &rig.audio,
                vec![adpcm, unsupported, pcm(), aac_format()],
                version,
            );
            assert!(server.is_ready());
            let mut source = rig.attach_pcm();
            // Distinct signed channels and nonzero first samples also check the
            // four-byte prefix split in the older WaveInfo/Wave wire format.
            let expected: Vec<u8> = (0..BLOCK_BYTES / 4)
                .flat_map(|frame| {
                    let left = ((-30000 + frame as i32 * 31) as i16).to_le_bytes();
                    let right = ((20000 - frame as i32 * 37) as i16).to_le_bytes();
                    [left[0], left[1], right[0], right[1]]
                })
                .collect();
            source.write_all(&expected).unwrap();
            let (data, timestamp) = rig.take_wave(&mut receiver);
            assert_eq!(
                data, expected,
                "stereo PCM must be selected when the client accepts AAC and ADPCM but not mono PCM"
            );
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let bytes = messages[0].encode_unframed_pdu().unwrap();
            match decode(&bytes).unwrap() {
                ServerAudioOutputPdu::Wave2(wave) => {
                    assert_eq!(messages.len(), 1);
                    assert_eq!(wave.timestamp, 0);
                    assert_eq!(wave.audio_timestamp, 0);
                    assert_eq!(wave.format_no, 2);
                    assert_eq!(wave.data.as_ref(), expected);
                }
                ServerAudioOutputPdu::Wave(info) => {
                    assert_eq!(messages.len(), 2);
                    assert_eq!(info.format_no, 2);
                    assert_eq!(usize::from(info.audio_length), expected.len());
                    let payload = messages[1].encode_unframed_pdu().unwrap();
                    assert_eq!(payload.len(), expected.len());
                    assert_eq!(&payload[..4], &[0; 4]);
                    let mut reconstructed = info.data_prefix.to_vec();
                    reconstructed.extend_from_slice(&payload[4..]);
                    assert_eq!(reconstructed, expected);
                }
                _ => panic!("expected an audio wave"),
            }
            drop(server);
            assert_eq!(source.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn negotiated_adpcm_used_when_pcm_not_accepted() {
        let rig = Rig::new();
        let format = Adpcm::new().unwrap().format.clone();
        let mut unsupported = pcm();
        unsupported.n_samples_per_sec = 22050;
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![unsupported, format.clone()]);
        let mut source = rig.attach_pcm();
        source.write_all(&vec![0x35; ADPCM_SAMPLES * 4]).unwrap();
        let (data, timestamp) = rig.take_wave(&mut receiver);
        assert_eq!(data.len(), ADPCM_BLOCK_BYTES);
        assert!(
            data.len() * 3 < ADPCM_SAMPLES * 4,
            "compact audio must reduce transport traffic"
        );
        assert_eq!(
            timestamp, 0,
            "ordinary graphics uses immediate audio playback"
        );
        let wave = wave2(&mut server, data, timestamp);
        assert_eq!(
            wave.format_no, 1,
            "server preference must keep the client's index"
        );
        assert_eq!(wave.audio_timestamp, timestamp);
        assert_eq!(wave.data.len(), usize::from(format.n_block_align));
        drop(server);
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "disconnect must release the encoder and capture FD"
        );
        // A client accepting only PCM must also remain usable.
        let (server, receiver) = negotiate(&rig.audio, vec![pcm()]);
        assert!(server.is_ready() && receiver.is_empty());
        drop(server);
    }

    #[test]
    fn adpcm_wire_and_flow_survive_more_than_a_minute_of_both_timestamp_wraps() {
        let audio = Audio::default();
        let mut encoder = Adpcm::new().unwrap();
        let (mut server, _receiver) = negotiate(&audio, vec![encoder.format.clone()]);
        let start = Instant::now();
        let pcm = vec![0x35; ADPCM_SAMPLES * 4];
        let mut flow = FlowControl::default();
        for sequence in 0..3400u32 {
            let elapsed_ms = u64::from(sequence) * ADPCM_SAMPLES as u64 * 1000 / 48000;
            let now = start + Duration::from_millis(elapsed_ms);
            let timestamp = 65000 + elapsed_ms as u32;
            assert!(flow.can_send(now));
            let messages: Vec<_> = server
                .wave(encoder.encode(&pcm).unwrap(), timestamp)
                .unwrap()
                .into();
            let ServerAudioOutputPdu::Wave2(wave) =
                decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
            else {
                panic!("expected wave2");
            };
            assert_eq!(wave.block_no, sequence as u8);
            assert_eq!(wave.timestamp, timestamp as u16);
            assert_eq!(wave.audio_timestamp, timestamp);
            assert_eq!(wave.data.len(), ADPCM_BLOCK_BYTES);
            flow.sent(wave.block_no, wave.timestamp, now);
            flow.confirm(
                wave.block_no,
                wave.timestamp.wrapping_add(3),
                now + Duration::from_millis(3),
            );
            flow.confirm(
                wave.block_no,
                wave.timestamp.wrapping_add(30),
                now + Duration::from_millis(30),
            );
            assert_eq!(flow.pending, 0);
            assert!(
                flow.hold_until.is_none(),
                "healthy playback must not be muted at either wrap"
            );
        }
        assert_eq!(flow.recent.len(), FlowControl::HISTORY);
        drop(server);
    }

    #[test]
    fn closed_event_receiver_stops_capture_and_releases_the_codec() {
        let rig = Rig::new();
        let format = Adpcm::new().unwrap().format.clone();
        let (server, receiver) = negotiate(&rig.audio, vec![format]);
        drop(receiver);
        let mut source = rig.attach_pcm();
        source.write_all(&vec![0x35; ADPCM_SAMPLES * 4]).unwrap();
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "failed event send must close the source socket"
        );
        drop(server);
    }

    #[test]
    fn negotiated_pcm_reaches_wire_with_client_format_index_and_bounded_backpressure() {
        let rig = Rig::new();
        let mut unsupported = pcm();
        unsupported.n_samples_per_sec = 22050;
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![unsupported, pcm()]);
        assert!(server.is_ready());
        assert!(
            receiver.try_recv().is_err(),
            "login screen must not produce sound"
        );
        let mut source = rig.attach_pcm();
        let expected = vec![0x35; BLOCK_BYTES];
        for _ in 0..12 {
            source.write_all(&expected).unwrap();
            rig.advance(Duration::from_millis(25));
        }
        rig.advance(Duration::from_millis(100));
        assert_eq!(receiver.len(), MAX_PENDING);
        for number in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected PCM wave")
            };
            assert_eq!(data, expected);
            let wave = wave2(&mut server, data, timestamp);
            assert_eq!(
                wave.format_no, 1,
                "format number must index the client's list"
            );
            assert_eq!(wave.block_no, number as u8);
            assert_eq!(wave.data.as_ref(), expected);
            confirm(&mut server, wave.block_no, wave.timestamp);
            confirm(&mut server, wave.block_no, wave.timestamp);
        }
        source.write_all(&expected).unwrap();
        rig.advance(Duration::from_millis(50));
        assert_eq!(receiver.len(), 1, "confirmation must unblock current audio");
        drop(server);
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "disconnect closes capture FD"
        );
        let (server, receiver) = negotiate(&rig.audio, vec![pcm()]);
        assert!(
            receiver.is_empty(),
            "reconnect must not replay previous audio"
        );
        drop(server);
    }

    #[test]
    fn invalid_timestamp_does_not_hide_later_valid_render_feedback() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(3));
        flow.confirm(0, 56301, start + Duration::from_millis(4));
        assert!(flow.hold_until.is_none());
        flow.sent(1, 0, start + Duration::from_millis(580));
        flow.confirm(1, 0, start + Duration::from_millis(583));
        flow.confirm(0, 600, start + Duration::from_millis(600));
        assert_eq!(flow.last_render_delay, Duration::from_millis(600));
        assert!(flow.check_pressure(start + Duration::from_millis(600)));
        assert!(!flow.can_send(start + Duration::from_millis(600)));
        assert!(flow.can_send(start + Duration::from_millis(880)));
        assert!(!flow.check_pressure(start + Duration::from_secs(120)));
    }

    #[test]
    fn recent_render_feedback_has_a_finite_hold_and_duplicates_do_not_extend_it() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(3));
        // A later wave forms the queued tail before a recent playback report.
        flow.sent(1, 0, start + Duration::from_millis(380));
        flow.confirm(1, 0, start + Duration::from_millis(383));
        flow.confirm(0, 400, start + Duration::from_millis(400));
        assert_eq!(flow.pending, 0);
        let deadline = flow.hold_until.unwrap();
        assert_eq!(deadline, start + Duration::from_millis(480));
        assert!(!flow.can_send(start + Duration::from_millis(450)));
        assert!(flow.check_pressure(start + Duration::from_millis(450)));
        flow.confirm(0, 400, start + Duration::from_millis(460));
        assert_eq!(flow.hold_until, Some(deadline));
        assert!(flow.can_send(deadline));
        assert!(!flow.check_pressure(deadline));
        // The later wave supplies only one confirmation. It never keeps
        // capture muted after the measured recovery interval has elapsed.
        assert!(flow.can_send(start + Duration::from_secs(120)));
        assert_eq!(
            flow.render_age(start + Duration::from_secs(120)),
            Duration::ZERO
        );
    }

    #[test]
    fn stale_render_echo_after_long_idle_cannot_restart_recovery() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 65000, start);
        flow.confirm(0, 65003, start + Duration::from_millis(3));
        flow.sent(1, 0, start + Duration::from_secs(120));
        flow.confirm(1, 0, start + Duration::from_millis(120003));
        // An old protocol echo belongs to history, not the current stream.
        flow.confirm(
            0,
            65000u16.wrapping_add(9000),
            start + Duration::from_secs(121),
        );
        assert_eq!(flow.pending, 0);
        assert!(flow.can_send(start + Duration::from_secs(121)));
        assert!(!flow.check_pressure(start + Duration::from_secs(121)));
        assert!(flow.hold_until.is_none());
    }

    #[test]
    fn delayed_playback_feedback_for_active_stream_has_a_bounded_hold() {
        for timestamp in [0, 2000] {
            let start = Instant::now();
            let mut flow = FlowControl::default();
            // Receipt stays healthy while the client queues two seconds of
            // playback. The completion report itself is a fresh measurement.
            for sequence in 0..100u64 {
                let at = start + Duration::from_millis(sequence * 20);
                assert!(flow.can_send(at));
                flow.sent(sequence as u8, 0, at);
                flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
            }
            let completion = start + Duration::from_millis(2003);
            flow.confirm(0, timestamp, completion);
            assert!(flow.check_pressure(completion));
            assert!(!flow.can_send(completion));
            assert_eq!(flow.pending, 0, "playback feedback is not a credit");
            let deadline = flow
                .hold_until
                .expect("recent completion must bound backlog");
            assert_eq!(deadline, completion + FlowControl::FEEDBACK_WINDOW);
            flow.confirm(0, timestamp, completion + Duration::from_millis(20));
            assert_eq!(flow.hold_until, Some(deadline));
            assert!(flow.can_send(deadline));
            assert!(!flow.check_pressure(deadline));
            assert!(flow.can_send(start + Duration::from_secs(120)));
        }
    }

    #[test]
    fn delayed_feedback_from_previous_audio_run_cannot_hold_resumed_stream() {
        for timestamp in [0, 2000] {
            let start = Instant::now();
            let mut flow = FlowControl::default();
            flow.sent(0, 0, start);
            flow.confirm(0, 0, start + Duration::from_millis(3));
            let resumed = start + Duration::from_secs(2);
            flow.sent(1, 0, resumed);
            flow.confirm(1, 0, resumed + Duration::from_millis(3));
            flow.confirm(0, timestamp, resumed + Duration::from_millis(10));
            assert!(flow.can_send(resumed + Duration::from_millis(10)));
            assert!(!flow.check_pressure(resumed + Duration::from_millis(10)));
            assert!(flow.hold_until.is_none());
        }
    }

    #[test]
    fn delayed_zero_echo_after_number_reuse_cannot_create_playback_hold() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for sequence in 0..=256u64 {
            let at = start + Duration::from_millis(sequence * 20);
            flow.sent(sequence as u8, 0, at);
            flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
        }
        let completion = start + Duration::from_millis(5140);
        flow.confirm(0, 0, completion);
        assert!(flow.can_send(completion));
        assert!(!flow.check_pressure(completion));
        assert!(flow.hold_until.is_none());
    }

    #[test]
    fn clock_like_delayed_echo_cannot_create_playback_hold() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for sequence in 0..100u64 {
            let at = start + Duration::from_millis(sequence * 20);
            flow.sent(sequence as u8, 0, at);
            flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
        }
        let completion = start + Duration::from_millis(2003);
        flow.confirm(0, 56301, completion);
        assert!(flow.can_send(completion));
        assert!(!flow.check_pressure(completion));
        assert!(flow.hold_until.is_none());
    }

    #[test]
    fn delayed_client_playback_drains_and_recovers_without_required_second_credits() {
        for reported_timestamp in [false, true] {
            let start = Instant::now();
            let mut flow = FlowControl::default();
            let mut receipts = VecDeque::new();
            let mut completions = VecDeque::new();
            let mut sequence = 0u64;
            let mut client_tail = 1980u64;
            let mut recovered_waves = 0;
            for millis in 0..10000u64 {
                let now = start + Duration::from_millis(millis);
                while receipts.front().is_some_and(|&(due, _)| due <= millis) {
                    let (_, number) = receipts.pop_front().unwrap();
                    flow.confirm(number, 0, now);
                }
                while completions
                    .front()
                    .is_some_and(|&(due, _, _)| due <= millis)
                {
                    let (due, number, sent) = completions.pop_front().unwrap();
                    let timestamp = if reported_timestamp {
                        (due - sent) as u16
                    } else {
                        0
                    };
                    flow.confirm(number, timestamp, now);
                }
                if millis % 20 == 0 && flow.can_send(now) {
                    let number = sequence as u8;
                    sequence += 1;
                    flow.sent(number, 0, now);
                    receipts.push_back((millis + 3, number));
                    // Real-time rendering starts with a two-second queue.
                    client_tail = client_tail.max(millis + 3) + 20;
                    completions.push_back((client_tail, number, millis));
                    if millis >= 6000 {
                        assert!(
                            client_tail - millis <= 323,
                            "fresh completion must drain playback to its budget plus a block and RTT: {} ms at {} ms",
                            client_tail - millis,
                            millis,
                        );
                        recovered_waves += 1;
                    }
                }
            }
            assert!(
                recovered_waves >= 190,
                "recovery must restore sample cadence"
            );
            assert!(flow.can_send(start + Duration::from_secs(11)));
            assert!(!flow.check_pressure(start + Duration::from_secs(11)));
        }
    }

    #[test]
    fn mixed_single_and_double_confirms_preserve_continuous_audio() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for sequence in 0..6500u64 {
            let at = start + Duration::from_millis(sequence * 20);
            assert!(
                flow.can_send(at),
                "optional second confirms cannot gate audio"
            );
            flow.sent(sequence as u8, 0, at);
            flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
            if sequence % 7 == 0 {
                flow.confirm(sequence as u8, 0, at + Duration::from_millis(15));
            }
            assert_eq!(flow.pending, 0);
            assert!(!flow.check_pressure(at + Duration::from_millis(16)));
        }
        assert!(flow.can_send(start + Duration::from_secs(600)));
        assert!(!flow.check_pressure(start + Duration::from_secs(600)));
    }

    #[test]
    fn missing_final_render_confirm_resumes_consecutive_packets_after_idle() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(3));
        flow.confirm(0, 0, start + Duration::from_millis(30));
        flow.sent(1, 0, start + Duration::from_millis(40));
        flow.confirm(1, 0, start + Duration::from_millis(43));
        // Sound pauses without a second reply for its final packet.
        for sequence in 2..200u64 {
            let at = start + Duration::from_secs(120) + Duration::from_millis(sequence * 20);
            assert!(
                flow.can_send(at),
                "resume must support more than one priming wave"
            );
            flow.sent(sequence as u8, 0, at);
            flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
            assert_eq!(flow.pending, 0);
            assert!(!flow.check_pressure(at + Duration::from_millis(4)));
        }
    }

    #[test]
    fn missing_transport_confirms_still_bound_the_event_queue() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for number in 0..MAX_PENDING as u8 {
            assert!(flow.can_send(start));
            flow.sent(number, 0, start);
        }
        assert_eq!(flow.pending, MAX_PENDING);
        assert!(!flow.can_send(start));
        assert!(!flow.can_send(start + Duration::from_secs(120)));
        assert!(flow.check_pressure(start + Duration::from_secs(120)));
        // A late receipt clears real transport credit even though its stale
        // delay measurement cannot affect current playback.
        flow.confirm(3, 0, start + Duration::from_secs(120));
        assert_eq!(flow.pending, 0);
        assert!(flow.can_send(start + Duration::from_secs(120)));
        assert!(!flow.check_pressure(start + Duration::from_secs(120)));
    }

    #[test]
    fn incomplete_transport_cannot_be_bypassed_by_stale_playback_feedback() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(3));
        flow.confirm(0, 0, start + Duration::from_millis(30));
        flow.sent(1, 0, start + Duration::from_millis(40));
        let delayed = start + Duration::from_secs(120);
        assert!(!flow.can_send(delayed));
        flow.confirm(0, 5000, delayed);
        assert_eq!(
            flow.pending, 1,
            "old playback replies cannot return newer credit"
        );
        assert!(!flow.can_send(delayed));
        flow.confirm(1, 0, delayed + Duration::from_millis(3));
        assert_eq!(flow.pending, 0);
        assert!(flow.can_send(delayed + Duration::from_millis(3)));
    }

    #[test]
    fn old_equal_render_echoes_after_block_and_timestamp_wrap_do_not_mute_audio() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for sequence in 0..3500u64 {
            let at = start + Duration::from_millis(sequence * 20);
            let timestamp = (65000 + sequence * 20) as u16;
            assert!(flow.can_send(at));
            flow.sent(sequence as u8, timestamp, at);
            flow.confirm(sequence as u8, timestamp, at + Duration::from_millis(3));
            if sequence == 500 || sequence == 1750 {
                flow.confirm(0, 65000, at + Duration::from_millis(4));
                assert!(flow.can_send(at + Duration::from_millis(4)));
                assert!(!flow.check_pressure(at + Duration::from_millis(4)));
            }
        }
        assert_eq!(flow.pending, 0);
        assert!(flow.hold_until.is_none());
    }

    #[test]
    fn delayed_recent_equal_echo_uses_wall_clock_delay_and_then_expires() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(3));
        flow.sent(1, 0, start + Duration::from_millis(850));
        flow.confirm(1, 0, start + Duration::from_millis(853));
        flow.confirm(0, 0, start + Duration::from_millis(900));
        assert_eq!(flow.last_render_delay, Duration::from_millis(897));
        assert!(!flow.can_send(start + Duration::from_millis(900)));
        let deadline = flow.hold_until.unwrap();
        assert_eq!(deadline, start + Duration::from_millis(1447));
        assert!(deadline <= start + Duration::from_millis(1900));
        flow.confirm(0, 141, start + Duration::from_secs(10));
        assert_eq!(flow.hold_until, Some(deadline));
        assert!(flow.can_send(start + Duration::from_secs(10)));
        assert!(!flow.check_pressure(start + Duration::from_secs(10)));
    }

    #[test]
    fn healthy_mstsc_render_latency_below_three_hundred_ms_preserves_cadence() {
        let start = Instant::now();
        let mut events = Vec::new();
        for sequence in 0..1500u64 {
            let render_ms = if sequence % 2 == 0 { 125 } else { 187 };
            events.extend([
                (sequence * 20, sequence, 0),
                (sequence * 20 + 3, sequence, 1),
                (sequence * 20 + 3 + render_ms, sequence, 2),
            ]);
        }
        events.sort_unstable();
        let mut flow = FlowControl::default();
        for (elapsed, sequence, kind) in events {
            let at = start + Duration::from_millis(elapsed);
            if kind == 0 {
                assert!(
                    flow.can_send(at),
                    "ordinary Windows rendering latency must not drop audio"
                );
                flow.sent(sequence as u8, 0, at);
            } else {
                flow.confirm(sequence as u8, 0, at);
            }
        }
        assert_eq!(flow.pending, 0);
        assert!(flow.hold_until.is_none());
        assert!(!flow.check_pressure(start + Duration::from_secs(31)));
    }

    #[test]
    fn single_zero_confirm_clients_remain_continuous_for_over_two_minutes() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for sequence in 0..6500u64 {
            let at = start + Duration::from_millis(sequence * 20);
            assert!(flow.can_send(at));
            flow.sent(sequence as u8, 0, at);
            flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
            assert_eq!(flow.pending, 0);
        }
        let at = start + Duration::from_secs(131);
        assert!(flow.can_send(at));
        assert!(!flow.check_pressure(at));
        assert_eq!(flow.recent.len(), FlowControl::HISTORY);
    }

    #[test]
    fn immediate_two_zero_confirms_retire_each_generation_without_false_pairing() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for sequence in 0..6500u64 {
            let at = start + Duration::from_millis(sequence * 20);
            assert!(flow.can_send(at));
            flow.sent(sequence as u8, 0, at);
            let confirmed = at + Duration::from_millis(3);
            flow.confirm(sequence as u8, 0, confirmed);
            flow.confirm(sequence as u8, 0, confirmed);
            assert_eq!(flow.pending, 0);
        }
        assert!(!flow.check_pressure(start + Duration::from_secs(131)));
    }

    #[test]
    fn clock_like_feedback_cannot_mute_audio_or_reduce_healthy_video() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        let mut rate = crate::performance::AdaptiveBitrate::new(12_000_000, start);
        for number in 0..4 {
            flow.sent(number, 0, start);
            flow.confirm(number, 0, start + Duration::from_millis(3));
        }
        // Reproduce clock-like feedback near the 56301 ms delay seen in the
        // journal, delivered immediately after sending a synthetic wave.
        flow.confirm(0, 56301, start + Duration::from_millis(4));
        assert!(flow.hold_until.is_none());
        for sequence in 4..6500u64 {
            let at = start + Duration::from_millis(sequence * 20);
            assert!(flow.can_send(at));
            flow.sent(sequence as u8, 0, at);
            flow.confirm(sequence as u8, 0, at + Duration::from_millis(3));
            if sequence % 256 == 0 {
                flow.confirm(sequence as u8, 40000, at + Duration::from_millis(4));
            }
            if flow.check_pressure(at + Duration::from_millis(5)) {
                rate.audio_congested(at + Duration::from_millis(5));
            }
            rate.sent(sequence as u32, at);
            rate.acknowledged(sequence as u32, 0, at + Duration::from_millis(10));
            assert_eq!(
                rate.current(),
                12_000_000,
                "phantom audio delay must not degrade video"
            );
        }
        assert_eq!(flow.pending, 0);
        assert!(flow.can_send(start + Duration::from_secs(600)));
        assert!(!flow.check_pressure(start + Duration::from_secs(600)));
    }

    #[test]
    fn immediate_duplicate_echo_does_not_establish_rendering_behavior() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(1));
        flow.confirm(0, 0, start + Duration::from_millis(2));
        assert_eq!(flow.pending, 0);
        assert!(flow.can_send(start + Duration::from_secs(1)));
        assert!(!flow.check_pressure(start + Duration::from_secs(1)));
    }

    #[test]
    fn healthy_zero_timestamp_rendering_remains_continuous_across_id_wraps() {
        let start = Instant::now();
        let mut events = Vec::new();
        for sequence in 0..1500u64 {
            events.extend([
                (sequence * 20, sequence, 0),
                (sequence * 20 + 3, sequence, 1),
                (sequence * 20 + 30, sequence, 2),
            ]);
        }
        events.sort_unstable();
        let mut flow = FlowControl::default();
        for (elapsed, sequence, kind) in events {
            let at = start + Duration::from_millis(elapsed);
            if kind == 0 {
                assert!(
                    flow.can_send(at),
                    "healthy audio must preserve its sample cadence"
                );
                flow.sent(sequence as u8, 0, at);
            } else {
                flow.confirm(sequence as u8, 0, at);
            }
        }
        assert_eq!(flow.pending, 0);
        assert!(!flow.check_pressure(start + Duration::from_secs(31)));
    }

    #[test]
    fn transport_pressure_uses_ack_age_with_a_stable_rtt_margin() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 1000, start);
        flow.confirm(0, 1000, start + Duration::from_millis(250));
        flow.sent(1, 1020, start + Duration::from_millis(500));
        assert!(!flow.check_pressure(start + Duration::from_millis(750)));
        assert!(flow.can_send(start + Duration::from_millis(750)));
        assert!(flow.check_pressure(start + Duration::from_millis(851)));
        assert!(!flow.can_send(start + Duration::from_millis(851)));
        flow.confirm(1, 1020, start + Duration::from_millis(901));
        assert_eq!(flow.receipt_baseline, Some(Duration::from_millis(250)));
        assert_eq!(flow.pending, 0);
        assert!(flow.can_send(start + Duration::from_millis(901)));
    }

    #[test]
    fn later_receipts_retire_lost_credit_without_requiring_playback_echoes() {
        let now = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 100, now);
        flow.confirm(0, 100, now);
        flow.confirm(0, 120, now);
        flow.sent(1, 120, now);
        flow.confirm(1, 120, now);
        flow.sent(2, 140, now);
        flow.sent(3, 160, now);
        flow.confirm(3, 160, now);
        assert_eq!(
            flow.pending, 0,
            "later FIFO receipt retires a lost earlier ACK"
        );
        for sequence in 4..=260u16 {
            flow.sent(sequence as u8, 100 + sequence * 20, now);
            flow.confirm(sequence as u8, 100 + sequence * 20, now);
            assert!(flow.can_send(now));
        }
        assert!(!flow.check_pressure(now + Duration::from_secs(120)));
    }

    #[test]
    fn stale_confirms_and_lost_confirm_at_number_reuse_preserve_credit() {
        let now = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 100, now);
        flow.confirm(0, 99, now);
        assert_eq!(flow.pending, 1, "older timestamp cannot free current wave");
        // Lose block zero's ACK, but continue to confirm the other waves.
        for sequence in 1..=300u16 {
            assert!(flow.can_send(now));
            flow.sent(sequence as u8, 100 + sequence * 20, now);
            flow.confirm(sequence as u8, 100 + sequence * 20, now);
        }
        assert_eq!(
            flow.pending, 0,
            "later FIFO receipt retires a lost earlier ACK"
        );
        assert_eq!(flow.recent.len(), 301);
    }

    #[test]
    fn capture_bursts_keep_bounded_recent_blocks_and_preserve_fragments() {
        let rig = Rig::new();
        let (server, mut receiver) = negotiate(&rig.audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut burst = vec![0x11; BLOCK_BYTES * 8];
        burst.extend(vec![0x22; BLOCK_BYTES]);
        burst.extend([0x33; 7]);
        source.write_all(&burst).unwrap();
        rig.audio.attach_test_pcm(capture);
        source.read_exact(&mut [0]).unwrap();
        rig.advance(Duration::from_millis(50));
        assert_eq!(
            receiver.len(),
            MAX_CAPTURE_BLOCKS,
            "capture burst must stay within the jitter budget"
        );
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected wave");
        };
        assert_eq!(data, vec![0x11; BLOCK_BYTES]);
        receiver.try_recv().unwrap();
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected newest wave");
        };
        assert_eq!(data, vec![0x22; BLOCK_BYTES]);
        source.write_all(&vec![0x33; BLOCK_BYTES - 7]).unwrap();
        rig.advance(Duration::from_millis(50));
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected wave");
        };
        assert_eq!(
            data,
            vec![0x33; BLOCK_BYTES],
            "partial source reads retain PCM alignment"
        );
        drop(server);
    }

    #[test]
    fn queued_pcm_is_paced_at_one_twenty_millisecond_block_per_block_duration() {
        let rig = Rig::new();
        let (server, mut receiver) = negotiate(&rig.audio, vec![pcm()]);
        let mut source = rig.attach_pcm();
        source
            .write_all(&vec![0x44; BLOCK_BYTES * MAX_CAPTURE_BLOCKS])
            .unwrap();
        rig.settle();
        let mut sent_at = Vec::new();
        rig.advance_with(Duration::from_millis(59), || {
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                assert_eq!((data.len(), timestamp), (BLOCK_BYTES, 0));
                sent_at.push(*rig.clock.elapsed.lock().unwrap());
            }
        });
        assert_eq!(
            sent_at,
            [0, 20, 40].map(Duration::from_millis),
            "a capture burst must leave at the 20 ms PCM block rate"
        );
        drop(server);
    }

    #[test]
    fn transport_backlog_drops_capture_and_resumes_with_fresh_pcm() {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![pcm()]);
        let mut source = rig.attach_pcm();
        for _ in 0..MAX_PENDING {
            source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
            rig.advance(Duration::from_millis(25));
        }
        let mut tail = 0;
        for _ in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected PCM wave")
            };
            tail = wave2(&mut server, data, timestamp).block_no;
        }
        source.write_all(&vec![0x22; BLOCK_BYTES * 20]).unwrap();
        rig.advance(Duration::from_millis(80));
        assert!(
            receiver.is_empty(),
            "transport credit must bound queued sound"
        );
        confirm(&mut server, tail, 0);
        source.write_all(&vec![0x33; BLOCK_BYTES]).unwrap();
        assert_eq!(
            rig.take_wave(&mut receiver).0,
            vec![0x33; BLOCK_BYTES],
            "recovery must not replay stale capture"
        );
        drop(server);
    }

    #[test]
    fn missing_render_tail_and_clock_like_echo_do_not_block_current_pcm() {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![pcm()]);
        let mut source = rig.attach_pcm();
        let take_wave = |receiver: &mut Events| {
            let (data, timestamp) = rig.take_wave(receiver);
            assert_eq!(timestamp, 0);
            data
        };
        let dispatch = |server: &mut RdpsndServer, data: Vec<u8>| {
            let wave = wave2(server, data, 0);
            confirm(server, wave.block_no, 0);
            wave.block_no
        };
        source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
        let first = dispatch(&mut server, take_wave(&mut receiver));
        confirm(&mut server, first, 56301);
        source.write_all(&vec![0x22; BLOCK_BYTES]).unwrap();
        dispatch(&mut server, take_wave(&mut receiver));
        // No render echo arrives for the tail while the application pauses.
        rig.advance(Duration::from_millis(600));
        for _ in 0..6 {
            let fresh = vec![0x33; BLOCK_BYTES];
            source.write_all(&fresh).unwrap();
            let received = take_wave(&mut receiver);
            assert_eq!(received, fresh);
            dispatch(&mut server, received);
        }
        drop(server);
    }

    #[test]
    fn healthy_pipewire_quantum_batching_preserves_every_sample() {
        healthy_capture_batching(pcm(), BLOCK_BYTES, None);
    }

    #[test]
    fn healthy_adpcm_quantum_batching_preserves_every_complete_frame() {
        let encoder = Adpcm::new().unwrap();
        healthy_capture_batching(
            encoder.format.clone(),
            ADPCM_SAMPLES * 4,
            Some(Codec::Adpcm(encoder)),
        );
    }

    #[test]
    fn healthy_aac_quantum_batching_preserves_every_complete_encoded_frame() {
        healthy_capture_batching(
            aac_format(),
            BLOCK_BYTES,
            Some(Codec::Aac(Aac::new().unwrap())),
        );
    }

    fn healthy_capture_batching(
        format: AudioFormat,
        block_bytes: usize,
        mut encoder: Option<Codec>,
    ) {
        let rig = Rig::new();
        let (mut server, mut receiver) = negotiate(&rig.audio, vec![format]);
        let mut source = rig.attach_pcm();
        const QUANTUM_BYTES: usize = 4096;
        let samples: Vec<u8> = (0..QUANTUM_BYTES * 32 / 4)
            .flat_map(|sample| (sample as u32).to_le_bytes())
            .collect();
        let complete = &samples[..samples.len() / block_bytes * block_bytes];
        let expected: Vec<u8> = if let Some(encoder) = &mut encoder {
            complete
                .chunks(block_bytes)
                .filter_map(|pcm| encoder.encode(pcm.to_vec()).unwrap())
                .flatten()
                .collect()
        } else {
            complete.to_vec()
        };
        // A healthy client confirms each wave as soon as it arrives.
        let mut received = Vec::new();
        let mut deliver = || {
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                received.extend_from_slice(&data);
                let wave = wave2(&mut server, data, timestamp);
                confirm(&mut server, wave.block_no, wave.timestamp);
            }
        };
        // PipeWire delivers fixed quanta at the real-time sample rate.
        let start = rig.clock.now();
        for (number, chunk) in samples.chunks(QUANTUM_BYTES).enumerate() {
            let due =
                start + Duration::from_secs_f64(number as f64 * QUANTUM_BYTES as f64 / 192000.0);
            rig.advance_with(due.saturating_duration_since(rig.clock.now()), &mut deliver);
            source.write_all(chunk).unwrap();
        }
        rig.advance_with(Duration::from_millis(200), &mut deliver);
        assert_eq!(
            received, expected,
            "healthy quantum batching must preserve every PCM sample"
        );
        drop(source);
        drop(server);
    }

    #[test]
    fn incompatible_audio_formats_produce_no_sound() {
        let audio = Audio::default();
        let mut unsupported = pcm_mono_format();
        unsupported.n_samples_per_sec = 22050;
        let (server, receiver) = negotiate(&audio, vec![unsupported]);
        assert!(!server.is_ready());
        assert!(receiver.is_empty());
    }

    /// The former 48 kHz mono opt-in: `NIRI_RDP_AUDIO_RATE=48000` with mono.
    const MONO_48K: AudioTuning = AudioTuning {
        drift_ppm: DRIFT_ALLOWANCE_PPM,
        mono: true,
        rate: 48000,
    };

    unsafe extern "C" {
        fn niri_audio_resampler_state(
            resampler: *const c_void,
            queued: *mut i32,
            adjusted: *mut u64,
        ) -> i32;
    }

    /// (frames queued between calls, frames ever padded or discarded).
    fn resampler_state(resampler: &Resampler) -> (i32, u64) {
        let (mut queued, mut adjusted) = (-1, u64::MAX);
        assert_eq!(
            unsafe { niri_audio_resampler_state(resampler.ptr, &mut queued, &mut adjusted) },
            0
        );
        (queued, adjusted)
    }

    /// 20 ms 48 kHz stereo blocks of a continuous tone: `frequency` Hz on the
    /// left, 0.7x amplitude at 1.5x frequency on the right.
    fn tone_blocks(frequency: f64, first_block: usize, blocks: usize) -> Vec<Vec<u8>> {
        let frames = BLOCK_BYTES / 4;
        (first_block..first_block + blocks)
            .map(|block| {
                (0..frames)
                    .flat_map(|frame| {
                        let t = (block * frames + frame) as f64 / 48000.0;
                        let tau = std::f64::consts::TAU;
                        let left = (12000.0 * (tau * frequency * t).sin()) as i16;
                        let right = (8400.0 * (tau * frequency * 1.5 * t).sin()) as i16;
                        let [l0, l1] = left.to_le_bytes();
                        let [r0, r1] = right.to_le_bytes();
                        [l0, l1, r0, r1]
                    })
                    .collect()
            })
            .collect()
    }

    /// Frequency of channel `channel` of interleaved S16LE `pcm` at `rate`,
    /// from interpolated rising zero crossings.
    fn zero_crossing_frequency(pcm: &[u8], channels: usize, channel: usize, rate: f64) -> f64 {
        let samples: Vec<f64> = (0..pcm.len() / 2 / channels)
            .map(|frame| f64::from(sample_at(pcm, frame * channels + channel)))
            .collect();
        let crossings: Vec<f64> = samples
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0] < 0.0 && pair[1] >= 0.0)
            .map(|(index, pair)| index as f64 + pair[0] / (pair[0] - pair[1]))
            .collect();
        assert!(crossings.len() > 10, "a tone, not silence");
        (crossings.len() - 1) as f64 * rate / (crossings[crossings.len() - 1] - crossings[0])
    }

    fn stereo_block(left: i16, right: i16) -> Vec<u8> {
        let [l0, l1] = left.to_le_bytes();
        let [r0, r1] = right.to_le_bytes();
        [l0, l1, r0, r1].repeat(BLOCK_BYTES / 4)
    }

    fn frame(pcm: &[u8], index: usize) -> (i32, i32) {
        (sample_at(pcm, index * 2), sample_at(pcm, index * 2 + 1))
    }

    fn offer(tuning: AudioTuning) -> Vec<(WaveFormat, u16, u32, u32, u16, u16)> {
        let audio = Audio::default().with_tuning(tuning);
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut factory = audio.factory();
        factory.set_sender(sender);
        let backend = factory.build_backend();
        assert!(
            backend
                .get_formats()
                .iter()
                .all(|format| { format.format == WaveFormat::ADPCM || format.data.is_none() })
        );
        backend
            .get_formats()
            .iter()
            .map(|format| {
                (
                    format.format,
                    format.n_channels,
                    format.n_samples_per_sec,
                    format.n_avg_bytes_per_sec,
                    format.n_block_align,
                    format.bits_per_sample,
                )
            })
            .collect()
    }

    const AAC_OFFER: (WaveFormat, u16, u32, u32, u16, u16) =
        (WaveFormat::AAC_MS, 2, 44100, 12000, 4, 16);
    const ADPCM_OFFER: (WaveFormat, u16, u32, u32, u16, u16) = (
        WaveFormat::ADPCM,
        2,
        48000,
        (48000 * ADPCM_BLOCK_BYTES / ADPCM_SAMPLES) as u32,
        ADPCM_BLOCK_BYTES as u16,
        4,
    );

    #[test]
    fn default_offers_44100_stereo_pcm_then_48000_stereo_pcm_then_compressed_fallbacks() {
        assert_eq!(
            offer(AudioTuning::default()),
            [
                (WaveFormat::PCM, 2, 44100, 176400, 4, 16),
                (WaveFormat::PCM, 2, 48000, 192000, 4, 16),
                AAC_OFFER,
                ADPCM_OFFER,
            ],
            "no mono by default"
        );
        // One 20 ms 44.1 kHz stereo wave: 882 frames of 4 bytes.
        assert_eq!(pcm44_format().n_avg_bytes_per_sec / 50, 3528);
        assert_eq!(PCM44_FRAMES * 4, 3528);
        assert_eq!(BLOCK_BYTES / 4 * 147 / 160, PCM44_FRAMES);
    }

    #[test]
    fn mono_opt_in_offers_44100_mono_then_44100_stereo_then_the_default_fallbacks() {
        assert_eq!(
            offer(AudioTuning {
                mono: true,
                ..AudioTuning::default()
            }),
            [
                (WaveFormat::PCM, 1, 44100, 88200, 2, 16),
                (WaveFormat::PCM, 2, 44100, 176400, 4, 16),
                (WaveFormat::PCM, 2, 48000, 192000, 4, 16),
                AAC_OFFER,
                ADPCM_OFFER,
            ]
        );
    }

    #[test]
    fn rate_48000_restores_the_former_48000_first_offers() {
        let rate_48k = AudioTuning {
            rate: 48000,
            ..AudioTuning::default()
        };
        assert_eq!(
            offer(rate_48k),
            [
                (WaveFormat::PCM, 2, 48000, 192000, 4, 16),
                AAC_OFFER,
                ADPCM_OFFER,
            ]
        );
        assert_eq!(
            offer(MONO_48K),
            [
                (WaveFormat::PCM, 1, 48000, 96000, 2, 16),
                (WaveFormat::PCM, 2, 48000, 192000, 4, 16),
                AAC_OFFER,
                ADPCM_OFFER,
            ]
        );
    }

    #[test]
    fn former_default_offers_stereo_pcm_first_then_compressed_fallbacks_with_exact_fields() {
        let audio = Audio::default().with_tuning(AudioTuning {
            rate: 48000,
            ..AudioTuning::default()
        });
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut factory = audio.factory();
        factory.set_sender(sender);
        let backend = factory.build_backend();
        let formats = backend.get_formats();
        assert!(
            formats.iter().all(|format| format.n_channels == 2),
            "no mono format is offered by default"
        );
        let summary: Vec<_> = formats
            .iter()
            .map(|format| {
                (
                    format.format,
                    format.n_channels,
                    format.n_samples_per_sec,
                    format.n_avg_bytes_per_sec,
                    format.n_block_align,
                    format.bits_per_sample,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (WaveFormat::PCM, 2, 48000, 192000, 4, 16),
                (WaveFormat::AAC_MS, 2, 44100, 12000, 4, 16),
                (
                    WaveFormat::ADPCM,
                    2,
                    48000,
                    (48000 * ADPCM_BLOCK_BYTES / ADPCM_SAMPLES) as u32,
                    ADPCM_BLOCK_BYTES as u16,
                    4
                ),
            ]
        );
        assert_eq!(formats[0].data, None);
        // One 20 ms stereo block: 960 frames of 4 bytes.
        assert_eq!(formats[0].n_avg_bytes_per_sec / 50, BLOCK_BYTES as u32,);
    }

    #[test]
    fn mono_48000_opt_in_offers_mono_pcm_first_then_stereo_pcm_then_compressed_fallbacks() {
        let audio = Audio::default().with_tuning(MONO_48K);
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut factory = audio.factory();
        factory.set_sender(sender);
        let backend = factory.build_backend();
        let formats = backend.get_formats();
        let summary: Vec<_> = formats
            .iter()
            .map(|format| (format.format, format.n_channels, format.n_avg_bytes_per_sec))
            .collect();
        assert_eq!(summary[0], (WaveFormat::PCM, 1, 96000));
        assert_eq!(summary[1], (WaveFormat::PCM, 2, 192000));
        assert_eq!(summary[2].0, WaveFormat::AAC_MS);
        assert_eq!(summary[3].0, WaveFormat::ADPCM);
        assert_eq!(summary.len(), 4);
        assert_eq!(downmix_to_mono(&vec![0x35; BLOCK_BYTES]).len(), 1920);
    }

    #[test]
    fn mono_48000_opt_in_preferred_over_stereo_aac_and_adpcm_downmixes_on_both_wave_versions() {
        for version in [Version::V6, Version::V8] {
            let mut rig = Rig::new();
            rig.audio = rig.audio.clone().with_tuning(MONO_48K);
            let adpcm = Adpcm::new().unwrap().format.clone();
            let mut unsupported = pcm_mono_format();
            unsupported.n_samples_per_sec = 22050;
            let (mut server, mut receiver) = negotiate_version(
                &rig.audio,
                vec![adpcm, pcm(), unsupported, pcm_mono_format(), aac_format()],
                version,
            );
            assert!(server.is_ready());
            let mut source = rig.attach_pcm();
            let stereo: Vec<u8> = (0..BLOCK_BYTES / 4)
                .flat_map(|frame| {
                    let left = ((-30000 + frame as i32 * 31) as i16).to_le_bytes();
                    let right = ((20001 - frame as i32 * 37) as i16).to_le_bytes();
                    [left[0], left[1], right[0], right[1]]
                })
                .collect();
            let expected: Vec<u8> = (0..BLOCK_BYTES / 4)
                .flat_map(|frame| {
                    let sum = -30000 + frame as i32 * 31 + 20001 - frame as i32 * 37;
                    let rounded = (f64::from(sum) / 2.0).round() as i16;
                    rounded.to_le_bytes()
                })
                .collect();
            assert_eq!(expected.len(), 1920);
            source.write_all(&stereo).unwrap();
            let (data, timestamp) = rig.take_wave(&mut receiver);
            assert_eq!(
                data, expected,
                "mono PCM must be selected and carry (L+R)/2 when the client also accepts stereo PCM, AAC and ADPCM"
            );
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let bytes = messages[0].encode_unframed_pdu().unwrap();
            match decode(&bytes).unwrap() {
                ServerAudioOutputPdu::Wave2(wave) => {
                    assert_eq!(messages.len(), 1);
                    assert_eq!(wave.timestamp, 0);
                    assert_eq!(wave.audio_timestamp, 0);
                    assert_eq!(wave.format_no, 3, "format number indexes the client's list");
                    assert_eq!(wave.data.as_ref(), expected);
                }
                ServerAudioOutputPdu::Wave(info) => {
                    assert_eq!(messages.len(), 2);
                    assert_eq!(info.format_no, 3);
                    assert_eq!(info.timestamp, 0);
                    assert_eq!(usize::from(info.audio_length), 1920);
                    let payload = messages[1].encode_unframed_pdu().unwrap();
                    assert_eq!(payload.len(), 1920);
                    let mut reconstructed = info.data_prefix.to_vec();
                    reconstructed.extend_from_slice(&payload[4..]);
                    assert_eq!(reconstructed, expected);
                }
                _ => panic!("expected an audio wave"),
            }
            drop(server);
            assert_eq!(source.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn downmix_rounds_half_away_from_zero_without_overflow_at_extremes() {
        let cases = [
            ((i16::MAX, i16::MAX), i16::MAX),
            ((i16::MIN, i16::MIN), i16::MIN),
            ((i16::MAX, i16::MIN), -1),
            ((i16::MIN, i16::MAX), -1),
            ((i16::MAX, 0), 16384),
            ((i16::MIN, 0), -16384),
            ((i16::MAX, i16::MAX - 1), i16::MAX),
            ((i16::MIN, i16::MIN + 1), i16::MIN),
            ((0, 0), 0),
            ((1, 0), 1),
            ((-1, 0), -1),
            ((2, 1), 2),
            ((-2, -1), -2),
            ((3, 1), 2),
            ((-3, -1), -2),
            ((1000, -1000), 0),
        ];
        for ((left, right), expected) in cases {
            let mut stereo = left.to_le_bytes().to_vec();
            stereo.extend_from_slice(&right.to_le_bytes());
            assert_eq!(
                downmix_to_mono(&stereo),
                expected.to_le_bytes(),
                "({left} + {right}) / 2"
            );
        }
        // Symmetric rounding: a sign-mirrored signal has a mirrored downmix
        // and no DC offset.
        let mut sum = 0i64;
        for left in (-32767i32..=32767).step_by(97) {
            for right in [-32767, -12345, -1, 0, 1, 777, 32767] {
                let pair = |l: i32, r: i32| {
                    let mut stereo = (l as i16).to_le_bytes().to_vec();
                    stereo.extend_from_slice(&(r as i16).to_le_bytes());
                    i64::from(i16::from_le_bytes(
                        downmix_to_mono(&stereo).try_into().unwrap(),
                    ))
                };
                let positive = pair(left, right);
                assert_eq!(positive, -pair(-left, -right));
                sum += positive + pair(-left, -right);
            }
        }
        assert_eq!(sum, 0);
    }

    const SKIP_THRESHOLD: Duration =
        Duration::from_millis(DRIFT_SKIP_RESERVE.as_millis() as u64 + 20);

    /// Feed real-time 20 ms blocks to the skip policy with a given surplus
    /// estimate, returning the elapsed time and reason of every skip.
    fn drive_policy(
        drift: &mut DriftSkip,
        start: Instant,
        from: Duration,
        until: Duration,
        mut earned_at: impl FnMut(Duration) -> Duration,
        mut block_at: impl FnMut(Duration) -> Vec<u8>,
    ) -> Vec<(Duration, SkipReason)> {
        let mut skips = Vec::new();
        let mut elapsed = from;
        while elapsed < until {
            let now = start + elapsed;
            let block = block_at(elapsed);
            match drift.consider(now, &block, earned_at(elapsed)) {
                Some(reason) => skips.push((elapsed, reason)),
                None => drift.sent(now, &block),
            }
            elapsed += BLOCK_DURATION;
        }
        skips
    }

    /// The worker's skip loop against the arrival model: every sent block is
    /// confirmed on arrival; a dropped block leaves a 20 ms sending gap.
    /// Returns each skip's elapsed time, reason and the surplus it relied on.
    fn drive_model(
        drift: &mut DriftSkip,
        arrivals: &mut ArrivalModel,
        start: Instant,
        from: Duration,
        until: Duration,
        mut block_at: impl FnMut(Duration) -> Vec<u8>,
    ) -> Vec<(Duration, SkipReason, Duration)> {
        let mut skips = Vec::new();
        let mut elapsed = from;
        while elapsed < until {
            let now = start + elapsed;
            let block = block_at(elapsed);
            let earned = arrivals.earned();
            match drift.consider(now, &block, earned) {
                Some(reason) => {
                    arrivals.skipped();
                    skips.push((elapsed, reason, earned));
                }
                None => {
                    drift.sent(now, &block);
                    arrivals.received(now, 1);
                }
            }
            elapsed += BLOCK_DURATION;
        }
        skips
    }

    fn within(value: Duration, low_ms: f64, high_ms: f64) -> bool {
        let ms = value.as_secs_f64() * 1000.0;
        (low_ms..=high_ms).contains(&ms)
    }

    #[test]
    fn drift_skip_needs_surplus_above_the_reserve_and_prefers_a_quiet_block() {
        let start = Instant::now();
        let mut drift = DriftSkip::default();
        let loud = stereo_block(12000, -12000);
        let quiet = stereo_block(200, -200);
        let quiet_each_second = |at: Duration| {
            if at.as_millis().is_multiple_of(1000) {
                quiet.clone()
            } else {
                loud.clone()
            }
        };
        // One microsecond short of reserve plus one block: never armed.
        let short = SKIP_THRESHOLD - Duration::from_micros(1);
        let skips = drive_policy(
            &mut drift,
            start,
            Duration::ZERO,
            Duration::from_secs(60),
            |_| short,
            quiet_each_second,
        );
        assert!(skips.is_empty(), "no surplus beyond the reserve: {skips:?}");
        // Enough surplus, but not within the minimum spacing of a fresh run.
        let mut drift = DriftSkip::default();
        let skips = drive_policy(
            &mut drift,
            start,
            Duration::ZERO,
            DRIFT_SKIP_MIN_SPACING,
            |_| SKIP_THRESHOLD,
            quiet_each_second,
        );
        assert!(
            skips.is_empty(),
            "nothing is dropped within 10 s of the run start"
        );
        // Armed: loud blocks still pass until the first quiet one.
        let skips = drive_policy(
            &mut drift,
            start,
            DRIFT_SKIP_MIN_SPACING,
            Duration::from_secs(14),
            |_| SKIP_THRESHOLD,
            |at| {
                if at == Duration::from_millis(12_500) {
                    quiet.clone()
                } else {
                    loud.clone()
                }
            },
        );
        assert_eq!(skips, [(Duration::from_millis(12_500), SkipReason::Quiet)]);
        // A block well below the recent average is quiet even above -40 dBFS;
        // the next skip waits the minimum spacing after the previous one.
        let relative = stereo_block(2000, -2000);
        assert!(block_level(&relative) > DRIFT_SKIP_QUIET_LEVEL);
        let skips = drive_policy(
            &mut drift,
            start,
            Duration::from_secs(14),
            Duration::from_secs(30),
            |_| Duration::from_millis(500),
            |at| {
                if at == Duration::from_millis(22_000) || at == Duration::from_millis(23_000) {
                    relative.clone()
                } else {
                    loud.clone()
                }
            },
        );
        assert_eq!(
            skips,
            [(Duration::from_millis(23_000), SkipReason::Quiet)],
            "the next skip arms 10 s after the previous one"
        );
    }

    #[test]
    fn drift_skip_is_forced_after_the_maximum_quiet_wait() {
        let start = Instant::now();
        let mut drift = DriftSkip::default();
        let loud = stereo_block(12000, -12000);
        let skips = drive_policy(
            &mut drift,
            start,
            Duration::ZERO,
            Duration::from_secs(40),
            |_| Duration::from_millis(200),
            |_| loud.clone(),
        );
        assert_eq!(
            skips,
            [
                (Duration::from_secs(15), SkipReason::Forced),
                (Duration::from_secs(30), SkipReason::Forced),
            ],
            "without a quiet block, skip 5 s after arming and rearm from the skip"
        );
        // A surplus that falls below the threshold while armed disarms; the
        // 5 s quiet wait restarts when it returns.
        let mut drift = DriftSkip::default();
        let skips = drive_policy(
            &mut drift,
            start,
            Duration::ZERO,
            Duration::from_secs(30),
            |at| {
                if (Duration::from_secs(12)..Duration::from_secs(13)).contains(&at) {
                    SKIP_THRESHOLD - BLOCK_DURATION
                } else {
                    SKIP_THRESHOLD
                }
            },
            |_| loud.clone(),
        );
        assert_eq!(skips, [(Duration::from_secs(18), SkipReason::Forced)]);
    }

    #[test]
    fn drift_skip_restarts_after_silence_resume_and_never_drops_silence() {
        let start = Instant::now();
        let mut drift = DriftSkip::default();
        let plenty = Duration::from_millis(500);
        assert_eq!(
            drift.consider(
                start + Duration::from_secs(100),
                &stereo_block(1, 1),
                plenty
            ),
            None,
            "nothing is armed before the first send"
        );
        let loud = stereo_block(12000, -12000);
        let quiet = stereo_block(200, -200);
        let silence = vec![0; BLOCK_BYTES];
        assert!(
            drive_policy(
                &mut drift,
                start,
                Duration::ZERO,
                Duration::from_secs(9),
                |_| plenty,
                |_| loud.clone()
            )
            .is_empty()
        );
        // Suppression drains the client queue; the spacing restarts on resume.
        drift.suppressed();
        let resume = Duration::from_secs(26);
        let skips = drive_policy(
            &mut drift,
            start,
            resume,
            resume + DRIFT_SKIP_MIN_SPACING,
            |_| plenty,
            |_| quiet.clone(),
        );
        assert!(
            skips.is_empty(),
            "the run restarted at the resume: {skips:?}"
        );
        // Armed again, but digital silence (the codec tail) is never dropped,
        // even though it is the quietest possible block.
        let armed = start + resume + DRIFT_SKIP_MIN_SPACING;
        for offset in [0, 3000, 10_000] {
            assert_eq!(
                drift.consider(armed + Duration::from_millis(offset), &silence, plenty),
                None
            );
        }
        assert_eq!(
            drift.consider(armed, &quiet, plenty),
            Some(SkipReason::Quiet)
        );
    }

    #[test]
    fn steady_on_schedule_audio_earns_only_the_drift_allowance() {
        let start = Instant::now();
        let mut drift = DriftSkip::default();
        let mut arrivals = ArrivalModel::default();
        let loud = stereo_block(12000, -12000);
        let quiet = stereo_block(200, -200);
        // Five minutes of continuous sound with frequent quiet candidates:
        // 100 ppm earns 30 ms, inside the reserve, so nothing is dropped.
        let skips = drive_model(
            &mut drift,
            &mut arrivals,
            start,
            Duration::ZERO,
            Duration::from_secs(300),
            |at| {
                if at.as_millis().is_multiple_of(500) {
                    quiet.clone()
                } else {
                    loud.clone()
                }
            },
        );
        assert!(skips.is_empty(), "{skips:?}");
        assert!(
            within(arrivals.earned(), 29.0, 30.0),
            "{:?}",
            arrivals.earned()
        );
        // The allowance alone first reaches reserve plus one block after
        // 800 s (40,000 blocks), then every further 200 s.
        let skips = drive_model(
            &mut drift,
            &mut arrivals,
            start,
            Duration::from_secs(300),
            Duration::from_secs(1100),
            |_| loud.clone(),
        );
        let times: Vec<_> = skips.iter().map(|skip| skip.0.as_secs_f64()).collect();
        assert_eq!(skips.len(), 2, "{times:?}");
        assert!((800.0..808.0).contains(&times[0]), "{times:?}");
        assert!((1000.0..1008.0).contains(&times[1]), "{times:?}");
        assert!(skips.iter().all(|skip| skip.1 == SkipReason::Forced));
        assert!(skips.iter().all(|skip| skip.2 >= SKIP_THRESHOLD));
        // Each skip's 20 ms gap shows in the next arrival's queue.
        assert!(arrivals.earned() < SKIP_THRESHOLD);
        assert!(arrivals.earned() >= DRIFT_SKIP_RESERVE);
    }

    #[test]
    fn catch_up_burst_earns_skips_only_above_the_reserve() {
        let start = Instant::now();
        let mut drift = DriftSkip::default();
        let mut arrivals = ArrivalModel::default();
        let loud = stereo_block(12000, -12000);
        let quiet = stereo_block(200, -200);
        let every_half_second = |at: Duration| {
            if at.as_millis().is_multiple_of(500) {
                quiet.clone()
            } else {
                loud.clone()
            }
        };
        let ms = Duration::from_millis;
        assert!(
            drive_model(
                &mut drift,
                &mut arrivals,
                start,
                ms(0),
                ms(30_000),
                every_half_second
            )
            .is_empty()
        );
        // A short stall (the model runs dry 60 ms, below the starvation
        // threshold), then the four waves in flight (the credit window) arrive
        // together and sending resumes on schedule: the queue gains 60 ms,
        // which stays inside the reserve.
        assert_eq!(arrivals.received(start + ms(30_060), 4), None);
        assert!(
            drive_model(
                &mut drift,
                &mut arrivals,
                start,
                ms(30_080),
                ms(60_000),
                every_half_second
            )
            .is_empty()
        );
        // 60 ms from the burst plus 100 ppm of the 30 s since.
        assert!(
            within(arrivals.earned(), 62.0, 64.0),
            "{:?}",
            arrivals.earned()
        );
        assert_eq!(arrivals.hidden, Duration::ZERO, "no starvation credit");
        // A larger catch-up: twelve blocks arrive together, one tick late.
        // Their 220 ms surplus is repaid in 20 ms skips, at least 10 s apart,
        // only while the estimate is at least reserve plus one block.
        assert_eq!(arrivals.received(start + ms(60_060), 12), None);
        let skips = drive_model(
            &mut drift,
            &mut arrivals,
            start,
            ms(60_080),
            ms(180_000),
            every_half_second,
        );
        assert_eq!(skips.len(), 8, "{skips:?}");
        assert!(skips.iter().all(|skip| skip.1 == SkipReason::Quiet));
        assert!(skips.iter().all(|skip| skip.2 >= SKIP_THRESHOLD));
        assert!(
            skips
                .windows(2)
                .all(|pair| pair[1].0 - pair[0].0 >= DRIFT_SKIP_MIN_SPACING)
        );
        assert!(arrivals.earned() >= DRIFT_SKIP_RESERVE);
        assert!(arrivals.earned() < SKIP_THRESHOLD);
    }

    #[test]
    fn arrival_estimate_is_capped_and_resets_on_silence_suppression() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut arrivals = ArrivalModel::default();
        arrivals.received(start, 1);
        assert_eq!(arrivals.earned(), Duration::ZERO, "no full window yet");
        // A long stall's huge burst cannot exceed the cap, however long the
        // sequence of later arrivals.
        arrivals.received(start + ms(10), 5000);
        let mut at = ms(30);
        while at < ms(5000) {
            arrivals.received(start + at, 1);
            assert!(arrivals.earned() <= DRIFT_SKIP_EARNED_CAP);
            at += BLOCK_DURATION;
        }
        assert!(arrivals.earned() + ms(30) >= DRIFT_SKIP_EARNED_CAP);
        // Suppression empties the estimate at once; late confirmations of
        // the silence tail cannot restore it.
        arrivals.reset(start + ms(5000));
        assert_eq!(arrivals.earned(), Duration::ZERO);
        for offset in 0..5 {
            arrivals.received(start + ms(5000 + offset * 20), 1);
        }
        assert_eq!(arrivals.earned(), Duration::ZERO);
    }

    #[test]
    fn first_confirmations_feed_the_arrival_model_once_per_wave() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut flow = FlowControl::default();
        let mut number = 0u8;
        let mut at = Duration::ZERO;
        // Steady waves, each confirmed on arrival and again 5 ms later.
        while at < ms(3000) {
            flow.sent(number, 0, start + at);
            flow.confirm(number, 0, start + at);
            flow.confirm(number, 0, start + at + ms(5));
            number = number.wrapping_add(1);
            at += BLOCK_DURATION;
        }
        assert!(
            flow.arrivals.earned() < ms(1),
            "{:?}",
            flow.arrivals.earned()
        );
        // A stall: four waves leave on schedule but only the last receipt
        // arrives, 300 ms later. FIFO proves all four reached the client.
        for _ in 0..4 {
            flow.sent(number, 0, start + at);
            number = number.wrapping_add(1);
            at += BLOCK_DURATION;
        }
        let release = start + at + ms(220);
        flow.confirm(number.wrapping_sub(1), 0, release);
        assert_eq!(flow.pending, 0);
        let mut at = ms(20);
        while at < ms(3000) {
            flow.sent(number, 0, release + at);
            flow.confirm(number, 0, release + at);
            number = number.wrapping_add(1);
            at += BLOCK_DURATION;
        }
        assert!(
            within(flow.arrivals.queue_low(), 60.0, 61.0),
            "{:?}",
            flow.arrivals.queue_low()
        );
        // The model ran dry for about 300 ms: one starvation credit.
        assert_eq!(flow.arrivals.hidden, STARVATION_CREDIT);
        assert_eq!(
            flow.arrivals.earned(),
            flow.arrivals.queue_low() + STARVATION_CREDIT
        );
    }

    #[test]
    fn starvation_credits_one_block_at_most_once_per_thirty_seconds() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut arrivals = ArrivalModel::default();
        let mut events = Vec::new();
        let mut at = Duration::ZERO;
        // Every 5 s the receipts stop for 200 ms (the model runs dry about
        // 200 ms); every 5 s offset by 2.5 s they stop for 80 ms only.
        while at < ms(100_000) {
            let gap = match at.as_millis() % 5000 {
                0 if !at.is_zero() => Some(ms(200)),
                2500 => Some(ms(80)),
                _ => None,
            };
            let at_arrival = at + gap.unwrap_or_default();
            if let Some(starved) = arrivals.received(start + at_arrival, 1) {
                events.push((at_arrival, starved));
            }
            at = at_arrival + BLOCK_DURATION;
        }
        // 19 starvations of about 200 ms; the 80 ms gaps are not starvation.
        assert_eq!(events.len(), 19, "{events:?}");
        assert!(
            events
                .iter()
                .all(|(_, starved)| within(*starved, 195.0, 200.1)),
            "{events:?}"
        );
        // Credited at the first, then 30 s later each time: four in 100 s.
        let credited: Vec<_> = events.iter().map(|(at, _)| at.as_millis()).collect();
        assert_eq!(credited[0], 5200);
        assert_eq!(arrivals.hidden, STARVATION_CREDIT * 4);
        // A skip the observed queue cannot show is charged to the credit.
        // (The observed queue holds only 62 us of drift allowance.)
        arrivals.skipped();
        assert!(within(arrivals.hidden, 60.0, 60.1), "{:?}", arrivals.hidden);
        // Suppression drops the credit but keeps its 30 s spacing.
        let credited = arrivals.last_starvation_credit;
        arrivals.reset(start + at);
        assert_eq!(arrivals.hidden, Duration::ZERO);
        assert_eq!(arrivals.last_starvation_credit, credited);
    }

    #[test]
    fn starvation_is_not_credited_across_silence_suppression_or_for_compressed_audio() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        for enabled in [true, false] {
            let mut flow = FlowControl::default();
            flow.arrivals.enabled = enabled;
            let mut number = 0u8;
            let mut at = Duration::ZERO;
            let send = |flow: &mut FlowControl, number: &mut u8, at: Duration| {
                flow.sent(*number, 0, start + at);
                flow.confirm(*number, 0, start + at);
                *number = number.wrapping_add(1);
            };
            while at < ms(3000) {
                send(&mut flow, &mut number, at);
                at += BLOCK_DURATION;
            }
            // The silence tail leaves, suppression starts, and the tail's
            // receipts arrive late, after the reset.
            let tail = at;
            for index in 0..4u32 {
                flow.sent(
                    number.wrapping_add(index as u8),
                    0,
                    start + tail + BLOCK_DURATION * index,
                );
            }
            flow.arrivals.reset(start + tail + ms(100));
            for index in 0..4u32 {
                flow.confirm(number.wrapping_add(index as u8), 0, start + tail + ms(150));
            }
            number = number.wrapping_add(4);
            // Sound resumes 5 s later: not a starvation.
            let mut at = tail + ms(5000);
            while at < tail + ms(8000) {
                send(&mut flow, &mut number, at);
                at += BLOCK_DURATION;
            }
            assert_eq!(flow.arrivals.hidden, Duration::ZERO);
            assert_eq!(flow.arrivals.last_starvation_credit, None);
            // A real stall inside the run is credited only for PCM.
            let stalled = at;
            for index in 0..4u32 {
                flow.sent(
                    number.wrapping_add(index as u8),
                    0,
                    start + stalled + BLOCK_DURATION * index,
                );
            }
            flow.confirm(number.wrapping_add(3), 0, start + stalled + ms(400));
            let expected = if enabled {
                STARVATION_CREDIT
            } else {
                Duration::ZERO
            };
            assert_eq!(flow.arrivals.hidden, expected, "enabled={enabled}");
            if !enabled {
                assert_eq!(flow.arrivals.earned(), Duration::ZERO);
            }
        }
    }

    #[test]
    fn audio_tuning_parses_defaults_valid_invalid_and_clamped_values() {
        let baseline = AudioTuning {
            drift_ppm: 100,
            mono: false,
            rate: 44100,
        };
        assert_eq!(AudioTuning::default(), baseline);
        assert_eq!(AudioTuning::parse(None, None, None), (baseline, vec![]));
        assert_eq!(
            AudioTuning::parse(Some("250"), Some("mono"), Some("48000")),
            (
                AudioTuning {
                    drift_ppm: 250,
                    mono: true,
                    rate: 48000,
                },
                vec![]
            )
        );
        assert_eq!(
            AudioTuning::parse(None, Some("stereo"), Some(" 44100 ")),
            (baseline, vec![])
        );
        assert_eq!(
            AudioTuning::parse(Some(" 0 "), Some("mono"), None)
                .0
                .drift_ppm,
            0
        );
        assert_eq!(
            AudioTuning::parse(Some("2000"), None, None).0.drift_ppm,
            2000
        );
        let (clamped, warnings) = AudioTuning::parse(Some("5000"), None, None);
        assert_eq!(clamped.drift_ppm, MAX_DRIFT_PPM);
        assert_eq!(warnings.len(), 1);
        for invalid in ["", "-5", "abc", "1.5", "<not unicode>"] {
            let (tuning, warnings) = AudioTuning::parse(Some(invalid), None, None);
            assert_eq!(tuning, baseline, "{invalid:?}");
            assert_eq!(warnings.len(), 1, "{invalid:?}");
        }
        for invalid in ["Stereo", "5.1", ""] {
            let (tuning, warnings) = AudioTuning::parse(None, Some(invalid), None);
            assert_eq!(tuning, baseline, "{invalid:?}");
            assert_eq!(warnings.len(), 1, "{invalid:?}");
        }
        for invalid in ["", "44.1", "44k", "22050", "96000", "<not unicode>"] {
            let (tuning, warnings) = AudioTuning::parse(None, None, Some(invalid));
            assert_eq!(tuning, baseline, "{invalid:?}");
            assert_eq!(warnings.len(), 1, "{invalid:?}");
            assert!(warnings[0].contains("NIRI_RDP_AUDIO_RATE"), "{invalid:?}");
        }
    }

    #[test]
    fn drift_skip_crossfade_splices_without_a_discontinuity() {
        // A continuous 441 Hz stereo tone across three blocks; 20 ms is 8.82
        // cycles, so removing the middle block leaves a large phase jump.
        let tone = |index: usize, channel: usize| {
            let phase = 2.0 * std::f64::consts::PI * 441.0 * index as f64 / 48000.0;
            (20000.0 * (phase + channel as f64).sin()).round() as i16
        };
        let frames = BLOCK_BYTES / 4;
        let block = |number: usize| -> Vec<u8> {
            (0..frames)
                .flat_map(|frame| {
                    let index = number * frames + frame;
                    let [l0, l1] = tone(index, 0).to_le_bytes();
                    let [r0, r1] = tone(index, 1).to_le_bytes();
                    [l0, l1, r0, r1]
                })
                .collect()
        };
        let (previous, dropped, next) = (block(0), block(1), block(2));
        let natural_step =
            (20000.0 * 2.0 * std::f64::consts::PI * 441.0 / 48000.0).ceil() as i32 + 1;
        let jump = |a: (i32, i32), b: (i32, i32)| (a.0 - b.0).abs().max((a.1 - b.1).abs());
        let last = frame(&previous, frames - 1);
        assert!(
            jump(last, frame(&next, 0)) > natural_step * 5,
            "without a crossfade the splice clicks"
        );
        let mut faded = next.clone();
        crossfade_into(&mut faded, &dropped[..DRIFT_SKIP_FADE_FRAMES * 4]);
        assert_eq!(frame(&faded, 0), frame(&dropped, 0));
        assert!(jump(last, frame(&faded, 0)) <= natural_step);
        // The fade adds at most (40000 / fade length) to the natural step.
        let fade_step = 40000 / DRIFT_SKIP_FADE_FRAMES as i32 + 1;
        for index in 1..frames {
            assert!(
                jump(frame(&faded, index - 1), frame(&faded, index)) <= natural_step + fade_step,
                "discontinuity at frame {index}"
            );
        }
        assert_eq!(
            faded[DRIFT_SKIP_FADE_FRAMES * 4..],
            next[DRIFT_SKIP_FADE_FRAMES * 4..],
            "only the first 5 ms changes"
        );
        // Extreme opposite samples stay in range with no wrap: a monotonic
        // ramp from the dropped extreme to the opposite one.
        let mut extreme = stereo_block(i16::MIN, i16::MAX);
        crossfade_into(
            &mut extreme,
            &stereo_block(i16::MAX, i16::MIN)[..DRIFT_SKIP_FADE_FRAMES * 4],
        );
        assert_eq!(frame(&extreme, 0), (32767, -32768));
        let extreme_step = 65535 / DRIFT_SKIP_FADE_FRAMES as i32 + 1;
        for index in 1..frames {
            let (previous, current) = (frame(&extreme, index - 1), frame(&extreme, index));
            assert!(current.0 <= previous.0 && current.1 >= previous.1);
            assert!(jump(previous, current) <= extreme_step);
        }
        assert_eq!(frame(&extreme, frames - 1), (-32768, 32767));
    }

    /// Real-time capture through the worker: each step writes `blocks`, lets
    /// the worker send what is due, confirms every wave on arrival unless the
    /// link is stalled, and advances 20 ms.
    struct LiveStream {
        rig: Rig,
        server: RdpsndServer,
        receiver: Events,
        source: UnixStream,
        withheld: Vec<u8>,
    }
    impl LiveStream {
        fn new(format: AudioFormat) -> Self {
            Self::with(vec![format], AudioTuning::default())
        }
        fn with(formats: Vec<AudioFormat>, tuning: AudioTuning) -> Self {
            let mut rig = Rig::new();
            rig.audio = rig.audio.clone().with_tuning(tuning);
            let (server, receiver) = negotiate(&rig.audio, formats);
            let source = rig.attach_pcm();
            Self {
                rig,
                server,
                receiver,
                source,
                withheld: Vec::new(),
            }
        }
        fn elapsed(&self) -> Duration {
            *self.rig.clock.elapsed.lock().unwrap()
        }
        /// Drift skips taken by this stream's worker.
        fn skips(&self) -> u64 {
            self.rig.audio.skips.load(Ordering::Relaxed)
        }
        fn step(&mut self, blocks: &[u8]) -> Vec<(Duration, Vec<u8>)> {
            self.step_with(blocks, true)
        }
        fn step_with(&mut self, blocks: &[u8], confirm_now: bool) -> Vec<(Duration, Vec<u8>)> {
            self.source.write_all(blocks).unwrap();
            self.rig.settle();
            let mut waves = Vec::new();
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                self.receiver.try_recv()
            {
                let wave = wave2(&mut self.server, data.clone(), timestamp);
                if confirm_now {
                    confirm(&mut self.server, wave.block_no, 0);
                } else {
                    self.withheld.push(wave.block_no);
                }
                waves.push((self.elapsed(), data));
            }
            *self.rig.clock.elapsed.lock().unwrap() += Duration::from_millis(20);
            waves
        }
        /// A forward-path stall of `steps` ticks; then the receipts of the
        /// waves sent during it arrive together. Returns how many were sent.
        fn stall(&mut self, block: &[u8], steps: usize) -> usize {
            let sent = (0..steps).map(|_| self.step_with(block, false).len()).sum();
            self.release();
            sent
        }
        /// Withheld receipts arrive together.
        fn release(&mut self) {
            for block_no in std::mem::take(&mut self.withheld) {
                confirm(&mut self.server, block_no, 0);
            }
        }
    }

    #[test]
    fn drift_skip_drops_one_quiet_pcm_block_without_moving_the_send_schedule() {
        use crate::performance::{Counter, connection};
        let before = connection().get(Counter::AudioDriftSkips);
        let mut stream = LiveStream::new(pcm());
        let loud = stereo_block(12000, -11000);
        let quiet = stereo_block(150, -90);
        let ms = Duration::from_millis;
        for step in 0..100u64 {
            let waves = stream.step(&loud);
            assert_eq!(waves.len(), 1, "one wave per tick");
            assert_eq!(waves[0].0, ms(step * 20));
        }
        // A 300 ms stall: four waves fill the credit window. Their receipts
        // arrive together and sending resumes at once, so the simulated
        // client queue holds 80 ms beyond just-in-time delivery; the model
        // ran dry for about 220 ms, adding 20 ms of starvation credit. The
        // surplus of about 100 ms is well above reserve plus one block.
        assert_eq!(stream.stall(&loud, 15), MAX_PENDING);
        while stream.elapsed() < ms(12_000) {
            let waves = stream.step(&loud);
            assert_eq!(waves.len(), 1);
            assert_eq!(waves[0].1, loud.clone(), "nothing dropped early");
        }
        // Armed at 10 s: the quiet block is dropped. Its successor has not
        // been captured yet, so it leaves on its own normal tick, faded in
        // from the dropped block's leading frames.
        assert!(stream.step(&quiet).is_empty(), "the quiet block is dropped");
        let mut faded = loud.clone();
        crossfade_into(&mut faded, &quiet[..DRIFT_SKIP_FADE_FRAMES * 4]);
        assert_eq!(stream.step(&loud), [(ms(12_020), faded.clone())]);
        assert_eq!(stream.step(&loud), [(ms(12_040), loud.clone())]);
        assert!(connection().get(Counter::AudioDriftSkips) > before);
        assert_eq!(stream.skips(), 1);
        // Nothing else is dropped within the 10 s spacing. Another stall's
        // burst restores the queue surplus (the starvation credit waits 30 s).
        // With backlog, the queued successor takes the dropped block's own
        // tick: the schedule does not move.
        while stream.elapsed() < ms(14_000) {
            assert_eq!(stream.step(&quiet).len(), 1, "within the spacing");
        }
        assert_eq!(stream.stall(&loud, 15), MAX_PENDING);
        while stream.elapsed() < ms(22_100) {
            let waves = stream.step(&loud);
            assert_eq!(waves.len(), 1);
            assert_eq!(waves[0].1, loud.clone(), "no other block is dropped");
        }
        let second = stereo_block(9000, 9000);
        let burst = [quiet.clone(), second.clone(), loud.clone()].concat();
        let mut faded = second.clone();
        crossfade_into(&mut faded, &quiet[..DRIFT_SKIP_FADE_FRAMES * 4]);
        assert_eq!(stream.step(&burst), [(ms(22_100), faded.clone())]);
        assert_eq!(stream.step(&loud), [(ms(22_120), loud.clone())]);
        assert_eq!(stream.step(&loud), [(ms(22_140), loud.clone())]);
        assert!(connection().get(Counter::AudioDriftSkips) >= before + 2);
        assert_eq!(stream.skips(), 2);
    }

    #[test]
    fn silence_suppression_resets_the_earned_surplus_and_audio_is_never_skipped_early() {
        let mut stream = LiveStream::new(pcm());
        let loud = stereo_block(12000, -11000);
        let quiet = stereo_block(150, -90);
        let silence = vec![0; BLOCK_BYTES];
        let ms = Duration::from_millis;
        for _ in 0..50 {
            stream.step(&loud);
        }
        // A stall's burst earns 80 ms; at 10 s a skip is armed and would be
        // forced at 15 s, but sound stops first.
        assert_eq!(stream.stall(&loud, 15), MAX_PENDING);
        while stream.elapsed() < ms(10_500) {
            assert_eq!(stream.step(&loud).len(), 1);
        }
        // Silence is suppressed after its codec tail, emptying the estimate.
        // The next 26 s of sound, with a quiet block every second, are
        // delivered complete.
        let mut sent = Vec::new();
        let mut quiet_written = 0;
        for step in 0..1310u64 {
            let block = match step {
                0..10 => &silence,
                _ if step % 50 == 0 => {
                    quiet_written += 1;
                    &quiet
                }
                _ => &loud,
            };
            sent.extend(stream.step(block).into_iter().map(|(_, data)| data));
        }
        let quiet_blocks = sent.iter().filter(|data| **data == quiet).count();
        assert_eq!(
            quiet_blocks, quiet_written,
            "every quiet block is delivered"
        );
        assert_eq!(sent.len(), 1300 + SILENCE_TAIL_BLOCKS);
        assert!(
            sent.iter()
                .all(|data| *data == loud || *data == quiet || *data == silence)
        );
    }

    #[test]
    fn discarded_capture_never_earns_a_drift_skip() {
        let mut stream = LiveStream::new(pcm());
        // Capture outruns the 20 ms send schedule threefold for a minute: the
        // bounded FIFO discards two of every three blocks. Discarded blocks
        // are never sent or confirmed, so they earn nothing. A skip would
        // crossfade the quiet block's head into a queued successor.
        let blocks = [
            stereo_block(150, -90),
            stereo_block(12000, -11000),
            stereo_block(9000, 9000),
        ];
        let expected: Vec<_> = blocks.to_vec();
        let burst = blocks.concat();
        let mut waves = 0;
        while stream.elapsed() < Duration::from_secs(60) {
            for (_, data) in stream.step(&burst) {
                assert!(expected.contains(&data), "a block was skipped");
                waves += 1;
            }
        }
        assert_eq!(waves, 3000, "one wave per tick, none skipped");
        assert_eq!(stream.skips(), 0);
    }

    #[test]
    fn compressed_fallbacks_never_drift_skip_after_stall_bursts_or_starvation() {
        let adpcm = Adpcm::new().unwrap().format.clone();
        for format in [aac_format(), adpcm] {
            let mut stream = LiveStream::new(format.clone());
            let loud = stereo_block(12000, -11000);
            let quiet = stereo_block(150, -90);
            let mut waves = 0;
            for _ in 0..100 {
                waves += stream.step(&loud).len();
            }
            // Zero-round-trip stalls: bursts plus starvation, twice, then 15 s
            // with a quiet block every 500 ms.
            for _ in 0..2 {
                waves += stream.stall(&loud, 15);
                for step in 0..250u64 {
                    let block = if step.is_multiple_of(25) {
                        &quiet
                    } else {
                        &loud
                    };
                    waves += stream.step(block).len();
                }
            }
            while stream.elapsed() < Duration::from_secs(20) {
                waves += stream.step(&quiet).len();
            }
            assert!(waves > 500, "{:?} kept sending: {waves}", format.format);
            assert_eq!(stream.skips(), 0, "{:?}", format.format);
        }
    }

    #[test]
    fn mono_channels_opt_in_downmixes_and_skips_the_same_way() {
        // The client accepts both; the opt-in puts mono first in the offer.
        let mut stream = LiveStream::with(vec![pcm_mono_format(), pcm()], MONO_48K);
        let loud = stereo_block(12000, -11000);
        let quiet = stereo_block(150, -90);
        let ms = Duration::from_millis;
        for _ in 0..100 {
            assert_eq!(stream.step(&loud)[0].1, downmix_to_mono(&loud));
        }
        assert_eq!(stream.stall(&loud, 15), MAX_PENDING);
        while stream.elapsed() < ms(12_000) {
            let at = stream.elapsed();
            assert_eq!(stream.step(&loud), [(at, downmix_to_mono(&loud))]);
        }
        assert!(stream.step(&quiet).is_empty(), "the quiet block is dropped");
        let mut faded = loud.clone();
        crossfade_into(&mut faded, &quiet[..DRIFT_SKIP_FADE_FRAMES * 4]);
        assert_eq!(stream.step(&loud), [(ms(12_020), downmix_to_mono(&faded))]);
        assert_eq!(stream.skips(), 1);
    }

    #[test]
    fn resampler_emits_exactly_882_frames_per_block_for_ten_minutes_without_queue_growth() {
        // 1 kHz has a whole number of cycles per 20 ms block, so one block
        // repeated is a continuous tone. 30000 blocks are ten minutes.
        let stereo = tone_blocks(1000.0, 0, 1).remove(0);
        for (channels, block) in [(2, stereo.clone()), (1, downmix_to_mono(&stereo))] {
            let mut resampler = Resampler::new(channels).unwrap();
            let mut frames = 0;
            let mut last = Vec::new();
            for _ in 0..30_000 {
                last = resampler.convert(&block).unwrap();
                assert_eq!(last.len(), PCM44_FRAMES * usize::from(channels) * 2);
                frames += last.len() / 2 / usize::from(channels);
                // Nothing is left queued between blocks and nothing was
                // ever padded or discarded: output rate equals input rate.
                assert_eq!(resampler_state(&resampler), (0, 0));
            }
            assert_eq!(frames, 30_000 * 882);
            if channels == 2 {
                let frequency = zero_crossing_frequency(&last, 2, 0, 44100.0);
                assert!((frequency - 1000.0).abs() < 1.0, "{frequency}");
            }
            // Wrong sizes are refused without disturbing the stream.
            assert!(resampler.convert(&block[2..]).is_err());
            assert_eq!(resampler.convert(&block).unwrap(), last);
        }
    }

    #[test]
    fn resampler_preserves_tone_frequency_level_and_channel_order() {
        let blocks = tone_blocks(997.0, 0, 100);
        let mut resampler = Resampler::new(2).unwrap();
        let output: Vec<u8> = blocks
            .iter()
            .flat_map(|block| resampler.convert(block).unwrap())
            .collect();
        assert_eq!(output.len(), 100 * 3528);
        // Skip the first block (filter priming) and measure the rest.
        let steady = &output[3528..];
        let left = zero_crossing_frequency(steady, 2, 0, 44100.0);
        let right = zero_crossing_frequency(steady, 2, 1, 44100.0);
        assert!((left - 997.0).abs() < 0.5, "left {left} Hz");
        assert!((right - 1495.5).abs() < 0.75, "right {right} Hz");
        // Played back at 44.1 kHz, the 48 kHz samples themselves would read
        // 8.8% low: the defect this conversion avoids.
        let unconverted = zero_crossing_frequency(&blocks.concat(), 2, 0, 44100.0);
        assert!((unconverted - 997.0 * 44100.0 / 48000.0).abs() < 1.0);
        let rms = |channel: usize| {
            let frames = steady.len() / 4;
            ((0..frames)
                .map(|frame| f64::from(sample_at(steady, frame * 2 + channel)).powi(2))
                .sum::<f64>()
                / frames as f64)
                .sqrt()
        };
        let expected = 12000.0 / 2f64.sqrt();
        assert!((rms(0) / expected - 1.0).abs() < 0.01, "{}", rms(0));
        assert!((rms(1) / (0.7 * expected) - 1.0).abs() < 0.01, "{}", rms(1));
        // Mono: downmix at 48 kHz first, then resample.
        let mut mono = Resampler::new(1).unwrap();
        let output: Vec<u8> = blocks
            .iter()
            .flat_map(|block| mono.convert(&downmix_to_mono(block)).unwrap())
            .collect();
        assert_eq!(output.len(), 100 * 1764);
        assert!(Resampler::new(3).is_err());
    }

    #[test]
    fn pcm_44100_preferred_reaches_wire_as_3528_byte_waves_with_client_format_index_on_both_wave_versions()
     {
        for version in [Version::V6, Version::V8] {
            let rig = Rig::new();
            let adpcm = Adpcm::new().unwrap().format.clone();
            let mut unsupported = pcm();
            unsupported.n_samples_per_sec = 22050;
            let (mut server, mut receiver) = negotiate_version(
                &rig.audio,
                vec![adpcm, pcm(), unsupported, pcm44_format(), aac_format()],
                version,
            );
            assert!(server.is_ready());
            let mut source = rig.attach_pcm();
            let mut reference = Resampler::new(2).unwrap();
            for block in tone_blocks(997.0, 0, 3) {
                let expected = reference.convert(&block).unwrap();
                assert_eq!(expected.len(), 3528);
                source.write_all(&block).unwrap();
                let (data, timestamp) = rig.take_wave(&mut receiver);
                assert_eq!(timestamp, 0);
                assert_eq!(
                    data, expected,
                    "44.1 kHz stereo PCM must be chosen over 48 kHz PCM, AAC and ADPCM"
                );
                let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
                let bytes = messages[0].encode_unframed_pdu().unwrap();
                match decode(&bytes).unwrap() {
                    ServerAudioOutputPdu::Wave2(wave) => {
                        assert_eq!(messages.len(), 1);
                        assert_eq!((wave.timestamp, wave.audio_timestamp), (0, 0));
                        assert_eq!(wave.format_no, 3, "indexes the client's list");
                        assert_eq!(wave.data.as_ref(), expected);
                        confirm(&mut server, wave.block_no, 0);
                    }
                    ServerAudioOutputPdu::Wave(info) => {
                        assert_eq!(messages.len(), 2);
                        assert_eq!(info.format_no, 3);
                        assert_eq!(info.timestamp, 0);
                        assert_eq!(usize::from(info.audio_length), 3528);
                        let payload = messages[1].encode_unframed_pdu().unwrap();
                        assert_eq!(payload.len(), 3528);
                        let mut reconstructed = info.data_prefix.to_vec();
                        reconstructed.extend_from_slice(&payload[4..]);
                        assert_eq!(reconstructed, expected);
                        confirm(&mut server, info.block_no, 0);
                    }
                    _ => panic!("expected an audio wave"),
                }
            }
            drop(server);
            assert_eq!(source.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn mono_opt_in_at_44100_downmixes_then_resamples_to_1764_byte_waves() {
        let mono = AudioTuning {
            mono: true,
            ..AudioTuning::default()
        };
        let mut stream = LiveStream::with(
            vec![
                pcm(),
                pcm44_format(),
                pcm_mono_format(),
                pcm44_mono_format(),
            ],
            mono,
        );
        let mut reference = Resampler::new(1).unwrap();
        for block in tone_blocks(997.0, 0, 20) {
            let waves = stream.step(&block);
            assert_eq!(waves.len(), 1);
            assert_eq!(waves[0].1.len(), 1764);
            assert_eq!(
                waves[0].1,
                reference.convert(&downmix_to_mono(&block)).unwrap()
            );
        }
    }

    #[test]
    fn pcm_44100_transport_credit_wait_preserves_resampler_continuity() {
        for queued_capture in [false, true] {
            let rig = Rig::new();
            let (mut server, mut receiver) = negotiate(&rig.audio, vec![pcm44_format()]);
            let mut source = rig.attach_pcm();
            let mut blocks = tone_blocks(997.0, 0, 12).into_iter();
            let mut reference = Resampler::new(2).unwrap();
            let mut tail = 0;
            // Fill transport credit with paced capture so no sample is lost.
            for _ in 0..MAX_PENDING {
                let block = blocks.next().unwrap();
                source.write_all(&block).unwrap();
                let (data, _) = rig.take_wave(&mut receiver);
                assert_eq!(data, reference.convert(&block).unwrap());
                tail = wave2(&mut server, data, 0).block_no;
                rig.advance(Duration::from_millis(25));
            }
            // An ordinary credit wait, within the receipt budget.
            rig.advance(Duration::from_millis(30));
            assert!(receiver.is_empty());
            let mut expected = VecDeque::new();
            if queued_capture {
                let block = blocks.next().unwrap();
                expected.push_back(reference.convert(&block).unwrap());
                source.write_all(&block).unwrap();
                rig.advance(Duration::from_millis(15));
                assert!(receiver.is_empty());
            }
            confirm(&mut server, tail, 0);
            for _ in 0..3 {
                let block = blocks.next().unwrap();
                expected.push_back(reference.convert(&block).unwrap());
                source.write_all(&block).unwrap();
                rig.advance(Duration::from_millis(25));
            }
            for expected in expected {
                let (data, _) = rig.take_wave(&mut receiver);
                assert!(
                    data == expected,
                    "a credit wait must not reset or re-prime the resampler"
                );
            }
            drop(server);
        }
    }

    #[test]
    fn pcm_44100_resampler_restarts_after_capture_loss_and_silence_suppression() {
        let mut stream = LiveStream::new(pcm44_format());
        let blocks = tone_blocks(997.0, 0, 40);
        let mut reference = Resampler::new(2).unwrap();
        for block in &blocks[..10] {
            assert_eq!(stream.step(block)[0].1, reference.convert(block).unwrap());
        }
        // Capture overflow: the oldest of four queued blocks is lost, so the
        // remaining three start a freshly primed resampler.
        let mut reference = Resampler::new(2).unwrap();
        let waves = stream.step(&blocks[10..14].concat());
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].1, reference.convert(&blocks[11]).unwrap());
        for block in &blocks[12..14] {
            assert_eq!(stream.step(&[])[0].1, reference.convert(block).unwrap());
        }
        for block in &blocks[14..20] {
            assert_eq!(stream.step(block)[0].1, reference.convert(block).unwrap());
        }
        // Five silent blocks are sent as the tail, then sending stops.
        let silence = vec![0; BLOCK_BYTES];
        for _ in 0..SILENCE_TAIL_BLOCKS {
            assert_eq!(
                stream.step(&silence)[0].1,
                reference.convert(&silence).unwrap()
            );
        }
        for _ in 0..10 {
            assert!(stream.step(&silence).is_empty());
        }
        // Sound resumes from a freshly primed resampler.
        let mut reference = Resampler::new(2).unwrap();
        for block in &blocks[20..30] {
            let waves = stream.step(block);
            assert_eq!(waves.len(), 1);
            assert_eq!(waves[0].1, reference.convert(block).unwrap());
        }
    }

    #[test]
    fn drift_skip_on_the_44100_path_drops_a_48000_block_before_resampling() {
        let mut stream = LiveStream::new(pcm44_format());
        let loud = stereo_block(12000, -11000);
        let quiet = stereo_block(150, -90);
        let ms = Duration::from_millis;
        for _ in 0..100 {
            let waves = stream.step(&loud);
            assert_eq!(waves.len(), 1);
            assert_eq!(waves[0].1.len(), 3528);
        }
        assert_eq!(stream.stall(&loud, 15), MAX_PENDING);
        while stream.elapsed() < ms(12_000) {
            let waves = stream.step(&loud);
            assert_eq!(waves.len(), 1);
            assert_eq!(waves[0].1.len(), 3528, "pacing by time, one wave per 20 ms");
        }
        // The filter spans less than one block, so after loud blocks the
        // resampler state is that of any warm reference fed loud blocks.
        let mut warm = Resampler::new(2).unwrap();
        warm.convert(&loud).unwrap();
        warm.convert(&loud).unwrap();
        assert!(stream.step(&quiet).is_empty(), "the quiet block is dropped");
        let mut faded = loud.clone();
        crossfade_into(&mut faded, &quiet[..DRIFT_SKIP_FADE_FRAMES * 4]);
        // The dropped 48 kHz block never reached the resampler; the faded
        // successor did, on its own tick.
        assert_eq!(
            stream.step(&loud),
            [(ms(12_020), warm.convert(&faded).unwrap())]
        );
        assert_eq!(
            stream.step(&loud),
            [(ms(12_040), warm.convert(&loud).unwrap())]
        );
        assert_eq!(stream.skips(), 1);
    }

    #[test]
    fn default_stereo_ignores_a_mono_client_format_and_keeps_exact_samples() {
        // Even if a client lists mono PCM, the default offer has none, so the
        // stereo format is selected and samples pass through unchanged.
        let mut stream = LiveStream::with(vec![pcm_mono_format(), pcm()], AudioTuning::default());
        let loud = stereo_block(12000, -11000);
        for _ in 0..10 {
            assert_eq!(stream.step(&loud)[0].1, loud, "exact stereo samples");
        }
    }

    #[test]
    fn bunched_and_delayed_confirmations_cannot_repeat_unearned_skips() {
        let mut stream = LiveStream::new(pcm());
        let loud = stereo_block(12000, -11000);
        let quiet = stereo_block(150, -90);
        let block = |step: u64| {
            if step.is_multiple_of(25) {
                &quiet
            } else {
                &loud
            }
        };
        // A minute of return-path batching: receipts come back four at a
        // time, the first 60 ms late. The forward stream is unchanged, so
        // nothing is earned beyond the drift allowance.
        let mut step = 0u64;
        while stream.elapsed() < Duration::from_secs(60) {
            assert_eq!(stream.step_with(block(step), false).len(), 1);
            if step % 4 == 3 {
                stream.release();
            }
            step += 1;
        }
        assert_eq!(stream.skips(), 0, "batched receipts earned nothing");
        // Two minutes more with a 200 ms receipt stall every 2 s on top. Each
        // stall starves the model, but starvation credit is limited to one
        // block per 30 s, and batching keeps the observed queue low.
        while stream.elapsed() < Duration::from_secs(180) {
            let stalled = step % 100 >= 90;
            stream.step_with(block(step), false);
            if !stalled && step % 4 == 3 {
                stream.release();
            }
            step += 1;
        }
        let skips = stream.skips();
        assert!(
            skips <= 4,
            "at most one per 30 s of starvation credit: {skips}"
        );
    }

    #[test]
    fn confirm_diagnostics_record_receipt_second_gap_and_echoed_timestamps() {
        let start = Instant::now();
        let mut flow = FlowControl {
            diagnostics: Some(ConfirmDiagnostics::default()),
            ..FlowControl::default()
        };
        flow.sent(0, 0, start);
        flow.confirm(0, 0, start + Duration::from_millis(3));
        flow.confirm(0, 0, start + Duration::from_millis(30));
        flow.confirm(0, 0, start + Duration::from_millis(31));
        flow.confirm(9, 0, start + Duration::from_millis(32));
        flow.sent(1, 0, start + Duration::from_millis(40));
        flow.confirm(1, 77, start + Duration::from_millis(43));
        let diagnostics = flow.diagnostics.as_ref().unwrap();
        assert_eq!(
            (
                diagnostics.first,
                diagnostics.second,
                diagnostics.repeat,
                diagnostics.unmatched,
                diagnostics.nonzero
            ),
            (2, 1, 1, 1, 1)
        );
        assert_eq!(diagnostics.gaps_us, [27_000]);
        assert_eq!(diagnostics.last_nonzero, Some((1, 77)));
        assert_eq!(flow.pending, 0, "diagnostics never change credit");
        // A five-second summary resets the counts but keeps the line budget.
        flow.confirm(1, 0, start + Duration::from_secs(6));
        let diagnostics = flow.diagnostics.as_ref().unwrap();
        assert_eq!((diagnostics.first, diagnostics.second), (0, 0));
        assert_eq!(diagnostics.detail_lines, 6);
    }
}
