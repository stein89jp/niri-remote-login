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
const ADPCM_SAMPLES: usize = 1012;
const ADPCM_BLOCK_BYTES: usize = 1024;
// IronRDP keeps at most four waves per event batch. Stay below that limit
// so every queued wave receives a block number and can be acknowledged.
const MAX_PENDING: usize = 4;
const MAX_CAPTURE_BLOCKS: usize = 3; // At most 64 ms of capture batching/jitter
const PLAYBACK_BUDGET: Duration = Duration::from_millis(300);
const CAPTURE_BUDGET: Duration = Duration::from_millis(60);
const SILENCE_TAIL_BLOCKS: usize = 5; // 100 ms flushes bounded AAC lookahead.

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
    Pcm,
    Aac(Aac),
    Adpcm(Adpcm),
}
impl Codec {
    fn encode(&mut self, pcm: Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Self::Pcm => Ok(Some(pcm)),
            Self::Aac(encoder) => encoder.encode(&pcm),
            Self::Adpcm(encoder) => encoder.encode(&pcm).map(Some),
        }
    }
    fn reset(&mut self) -> anyhow::Result<()> {
        if let Self::Aac(encoder) = self {
            *encoder = Aac::new()?;
        }
        Ok(())
    }
}

fn pcm_format() -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: 48000,
        n_avg_bytes_per_sec: 192000,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }
}

#[derive(Debug)]
struct SentWave {
    number: u8,
    timestamp: u16,
    sent_at: Instant,
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
    receipt_baseline: Option<Duration>,
    last_render_delay: Duration,
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
        self.latest_sent = Some(now);
        self.recent.push_back(SentWave {
            number,
            timestamp,
            sent_at: now,
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
            for old in self.recent.iter_mut().take(position + 1) {
                if !old.acknowledged {
                    old.acknowledged = true;
                    self.pending -= 1;
                }
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
        let reported = if reported <= elapsed + PLAYBACK_BUDGET {
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
        if elapsed > Self::FEEDBACK_WINDOW || render <= wave.feedback_elapsed {
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
    socket: Arc<Mutex<Option<UnixStream>>>,
    video: Option<crate::video::Video>,
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
    pub fn attach(&self, socket: Option<UnixStream>) {
        *self.socket.lock().unwrap() = socket;
    }
    pub fn clear(&self) {
        self.socket.lock().unwrap().take();
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
        // keep compressed codecs for clients that do not accept PCM.
        let mut formats = vec![pcm_format()];
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
            pending: Arc::new(Mutex::new(FlowControl::default())),
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
        common.first()
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
            Codec::Pcm
        };
        self.aac.take();
        self.encoder.take();
        let block_bytes = if matches!(encoder, Codec::Adpcm(_)) {
            ADPCM_SAMPLES * 4
        } else {
            BLOCK_BYTES
        };
        let block_duration = Duration::from_secs_f64(block_bytes as f64 / 192000.0);
        eprintln!(
            "RDP audio {}: {} Hz stereo, {} kbps, {:.2} ms capture blocks",
            match &encoder {
                Codec::Aac(_) => "AAC-LC",
                Codec::Adpcm(_) => "MS ADPCM",
                Codec::Pcm => "PCM",
            },
            format.format().n_samples_per_sec,
            format.format().n_avg_bytes_per_sec * 8 / 1000,
            block_duration.as_secs_f64() * 1000.0,
        );
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
                    let mut socket = loop {
                        if stopped.load(Ordering::Acquire) {
                            return;
                        }
                        if let Some(socket) = audio.socket.lock().unwrap().take() {
                            break socket;
                        }
                        thread::sleep(Duration::from_millis(20));
                    };
                    if socket.write_all(b"A").is_err() || socket.set_nonblocking(true).is_err() {
                        return;
                    }
                    let start = Instant::now();
                    let mut block = vec![0u8; block_bytes];
                    let mut captured = VecDeque::new();
                    let mut filled = 0;
                    let mut number = 0u8;
                    let mut next_send = start;
                    let mut silent_blocks = 0usize;
                    let mut quiet_capture_blocks = 0usize;
                    let mut feedback_suspended = false;
                    let mut reset_codec = false;
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
                                Ok(0) => break 'capture,
                                Ok(n) => filled += n,
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
                                }
                                captured.push_back((block.clone(), Instant::now()));
                                filled = 0;
                            }
                        }
                        if !drained {
                            continue;
                        }
                        let now = Instant::now();
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
                        }
                        if !queue.can_send(now) {
                            // A full credit window can be an ordinary ACK wait.
                            // Keep its bounded recent capture and codec overlap;
                            // only discard samples during actual delay recovery.
                            if pressure && !captured.is_empty() {
                                captured.clear();
                                reset_codec = true;
                            }
                        } else if now >= next_send
                            && let Some((data, _)) = captured.pop_front()
                        {
                            if reset_codec {
                                if let Err(error) = encoder.reset() {
                                    eprintln!("RDP audio recovery stopped: {error}");
                                    break;
                                }
                                reset_codec = false;
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
                                continue;
                            }
                            let data = match encoder.encode(data) {
                                Ok(Some(data)) => data,
                                Ok(None) => continue, // Bounded AAC frame assembly/lookahead.
                                Err(error) => {
                                    eprintln!("RDP audio encoding stopped: {error}");
                                    break;
                                }
                            };
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
                        }
                        drop(queue);
                        thread::sleep(Duration::from_millis(2));
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
            .confirm(block_no, timestamp, Instant::now());
    }
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
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

    fn negotiate(
        audio: &Audio,
        formats: Vec<AudioFormat>,
    ) -> (
        RdpsndServer,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        negotiate_version(audio, formats, Version::V8)
    }

    fn negotiate_version(
        audio: &Audio,
        formats: Vec<AudioFormat>,
        version: Version,
    ) -> (
        RdpsndServer,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
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

    #[test]
    fn aac_fallback_uses_raw_packets_and_immediate_timestamps() {
        for version in [Version::V6, Version::V8] {
            let audio = Audio::default();
            let adpcm = Adpcm::new().unwrap().format.clone();
            let (mut server, mut receiver) =
                negotiate_version(&audio, vec![adpcm, aac_format()], version);
            assert!(server.is_ready());
            let (mut source, capture) = UnixStream::pair().unwrap();
            source
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            audio.attach(Some(capture));
            source.read_exact(&mut [0]).unwrap();
            source.write_all(&vec![0x35; BLOCK_BYTES * 3]).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let (data, timestamp) = loop {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    break (data, timestamp);
                }
                assert!(Instant::now() < deadline, "AAC startup must remain bounded");
                thread::sleep(Duration::from_millis(1));
            };
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
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        let mut sent = 0usize;
        for _ in 0..SILENCE_TAIL_BLOCKS + 5 {
            source.write_all(&vec![0; BLOCK_BYTES]).unwrap();
            thread::sleep(Duration::from_millis(25));
            while let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                assert_eq!(data, vec![0; BLOCK_BYTES]);
                let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
                let ServerAudioOutputPdu::Wave2(wave) =
                    decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
                else {
                    panic!("expected wave2");
                };
                server
                    .process(
                        &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                            block_no: wave.block_no,
                            timestamp: 0,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                sent += 1;
            }
        }
        assert_eq!(sent, SILENCE_TAIL_BLOCKS);
        assert!(
            receiver.is_empty(),
            "idle silence must not fill client playback buffers"
        );
        source.write_all(&vec![0x33; BLOCK_BYTES]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _))) = receiver.try_recv()
            {
                assert_eq!(data, vec![0x33; BLOCK_BYTES]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "current sound must resume after silence"
            );
            thread::sleep(Duration::from_millis(1));
        }
        drop(server);
    }

    #[test]
    fn aac_transport_recovery_discards_codec_lookahead_before_fresh_sound() {
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![aac_format()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        // Withhold receipt credit until the transport queue is full. Continue
        // capturing so recovery must discard both old PCM and AAC lookahead.
        for _ in 0..12 {
            source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
            thread::sleep(Duration::from_millis(25));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while receiver.len() < MAX_PENDING {
            assert!(
                Instant::now() < deadline,
                "AAC must fill bounded transport credit"
            );
            source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
            thread::sleep(Duration::from_millis(25));
        }
        let mut tail = 0;
        for _ in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected AAC wave")
            };
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let ServerAudioOutputPdu::Wave2(wave) =
                decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
            else {
                panic!("expected Wave2")
            };
            tail = wave.block_no;
        }
        source.write_all(&vec![0x22; BLOCK_BYTES * 20]).unwrap();
        thread::sleep(Duration::from_millis(80));
        assert!(
            receiver.is_empty(),
            "unconfirmed transport cannot queue more sound"
        );
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no: tail,
                    timestamp: 0,
                }))
                .unwrap(),
            )
            .unwrap();
        let fresh = vec![0x33; BLOCK_BYTES];
        let mut encoder = Aac::new().unwrap();
        let expected = (0..3)
            .find_map(|_| encoder.encode(&fresh).unwrap())
            .unwrap();
        source.write_all(&fresh.repeat(3)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _))) = receiver.try_recv()
            {
                assert_eq!(
                    data, expected,
                    "recovery packet must contain only fresh codec state"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "AAC must recover with fresh packets"
            );
            thread::sleep(Duration::from_millis(1));
        }
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
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![aac_format()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        let take_wave = |receiver: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    assert_eq!(timestamp, 0);
                    return data;
                }
                assert!(Instant::now() < deadline, "expected consecutive AAC audio");
                thread::sleep(Duration::from_millis(1));
            }
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
                let messages: Vec<_> = server.wave(data, 0).unwrap().into();
                let ServerAudioOutputPdu::Wave2(wave) =
                    decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
                else {
                    panic!("expected Wave2");
                };
                tail = wave.block_no;
                sent += 1;
            }
            thread::sleep(Duration::from_millis(25));
        }
        // Capture stays empty while the fourth receipt is pending. Waiting
        // for credit alone must not discard AAC's buffered samples/lookahead.
        thread::sleep(Duration::from_millis(30));
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
            thread::sleep(Duration::from_millis(15));
            assert!(receiver.is_empty());
        }
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no: tail,
                    timestamp: 0,
                }))
                .unwrap(),
            )
            .unwrap();
        for _ in 0..3 {
            if let Some(packet) = reference.encode(&fresh).unwrap() {
                expected.push_back(packet);
            }
            source.write_all(&fresh).unwrap();
            thread::sleep(Duration::from_millis(25));
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
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![aac_format()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        let take_wave = |receiver: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    assert_eq!(timestamp, 0);
                    return data;
                }
                assert!(Instant::now() < deadline, "AAC playback must resume");
                thread::sleep(Duration::from_millis(1));
            }
        };
        let acknowledge = |server: &mut RdpsndServer, data: Vec<u8>| {
            let messages: Vec<_> = server.wave(data, 0).unwrap().into();
            let ServerAudioOutputPdu::Wave2(wave) =
                decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
            else {
                panic!("expected Wave2");
            };
            server
                .process(
                    &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                        block_no: wave.block_no,
                        timestamp: 0,
                    }))
                    .unwrap(),
                )
                .unwrap();
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
        thread::sleep(Duration::from_millis(20));
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no: first,
                    timestamp: 0,
                }))
                .unwrap(),
            )
            .unwrap();
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
            thread::sleep(Duration::from_millis(25));
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
        thread::sleep(Duration::from_millis(550));
        assert!(receiver.is_empty(), "idle capture must stay silent");

        let fresh = vec![0x33; BLOCK_BYTES];
        let mut resumed_packets = 0;
        for _ in 0..12 {
            if let Some(packet) = reference.encode(&fresh).unwrap() {
                expected.push_back(packet);
            }
            source.write_all(&fresh).unwrap();
            thread::sleep(Duration::from_millis(25));
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
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![aac_format()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        source.write_all(&vec![0x11; BLOCK_BYTES * 3]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let (data, timestamp) = loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                break (data, timestamp);
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
        let ServerAudioOutputPdu::Wave2(wave) =
            decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
        else {
            panic!("expected wave2");
        };
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no: wave.block_no,
                    timestamp: 0,
                }))
                .unwrap(),
            )
            .unwrap();
        let fresh = vec![0x33; BLOCK_BYTES];
        let mut encoder = Aac::new().unwrap();
        let expected = (0..3)
            .find_map(|_| encoder.encode(&fresh).unwrap())
            .unwrap();
        source
            .write_all(&fresh.repeat(MAX_CAPTURE_BLOCKS + 5))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _))) = receiver.try_recv()
            {
                assert_eq!(
                    data, expected,
                    "dropping capture must also drop earlier codec lookahead"
                );
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        drop(server);
    }

    #[test]
    fn pcm_preferred_over_aac_and_adpcm_preserves_signed_stereo_on_both_wave_versions() {
        for version in [Version::V6, Version::V8] {
            let audio = Audio::default();
            let adpcm = Adpcm::new().unwrap().format.clone();
            let mut unsupported = pcm();
            unsupported.n_samples_per_sec = 44100;
            let (mut server, mut receiver) = negotiate_version(
                &audio,
                vec![adpcm, unsupported, pcm(), aac_format()],
                version,
            );
            assert!(server.is_ready());
            let (mut source, capture) = UnixStream::pair().unwrap();
            source
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            audio.attach(Some(capture));
            source.read_exact(&mut [0]).unwrap();
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
            let deadline = Instant::now() + Duration::from_secs(2);
            let (data, timestamp) = loop {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    break (data, timestamp);
                }
                assert!(Instant::now() < deadline, "expected a 20 ms PCM block");
                thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(
                data, expected,
                "PCM must be selected when the client also accepts AAC and ADPCM"
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
        let audio = Audio::default();
        let format = Adpcm::new().unwrap().format.clone();
        let mut unsupported = pcm();
        unsupported.n_samples_per_sec = 44100;
        let (mut server, mut receiver) = negotiate(&audio, vec![unsupported, format.clone()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        source.write_all(&vec![0x35; ADPCM_SAMPLES * 4]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let (data, timestamp) = loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                break (data, timestamp);
            }
            assert!(
                Instant::now() < deadline,
                "capture must produce an ADPCM packet"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(data.len(), ADPCM_BLOCK_BYTES);
        assert!(
            data.len() * 3 < ADPCM_SAMPLES * 4,
            "compact audio must reduce transport traffic"
        );
        assert_eq!(
            timestamp, 0,
            "ordinary graphics uses immediate audio playback"
        );
        let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
        let ServerAudioOutputPdu::Wave2(wave) =
            decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
        else {
            panic!("expected wave2");
        };
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
        let (server, receiver) = negotiate(&audio, vec![pcm()]);
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
        let audio = Audio::default();
        let format = Adpcm::new().unwrap().format.clone();
        let (server, receiver) = negotiate(&audio, vec![format]);
        drop(receiver);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
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
        let audio = Audio::default();
        let mut unsupported = pcm();
        unsupported.n_samples_per_sec = 44100;
        let (mut server, mut receiver) = negotiate(&audio, vec![unsupported, pcm()]);
        assert!(server.is_ready());
        assert!(
            receiver.try_recv().is_err(),
            "login screen must not produce sound"
        );
        let (mut source, capture) = UnixStream::pair().unwrap();
        audio.attach(Some(capture));
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut ready = [0];
        source.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"A");
        let expected = vec![0x35; BLOCK_BYTES];
        for _ in 0..12 {
            source.write_all(&expected).unwrap();
            thread::sleep(Duration::from_millis(25));
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(receiver.len(), MAX_PENDING);
        for number in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected PCM wave")
            };
            assert_eq!(data, expected);
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let bytes = messages[0].encode_unframed_pdu().unwrap();
            let ServerAudioOutputPdu::Wave2(wave) = decode(&bytes).unwrap() else {
                panic!("expected wave2")
            };
            assert_eq!(
                wave.format_no, 1,
                "format number must index the client's list"
            );
            assert_eq!(wave.block_no, number as u8);
            assert_eq!(wave.data.as_ref(), expected);
            let confirm = ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                block_no: wave.block_no,
                timestamp: wave.timestamp,
            });
            server.process(&encode_vec(&confirm).unwrap()).unwrap();
            server.process(&encode_vec(&confirm).unwrap()).unwrap();
        }
        source.write_all(&expected).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(receiver.len(), 1, "confirmation must unblock current audio");
        drop(server);
        source
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "disconnect closes capture FD"
        );
        let (server, receiver) = negotiate(&audio, vec![pcm()]);
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
        let audio = Audio::default();
        let (server, mut receiver) = negotiate(&audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut burst = vec![0x11; BLOCK_BYTES * 8];
        burst.extend(vec![0x22; BLOCK_BYTES]);
        burst.extend([0x33; 7]);
        source.write_all(&burst).unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        thread::sleep(Duration::from_millis(50));
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
        thread::sleep(Duration::from_millis(50));
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
    fn transport_backlog_drops_capture_and_resumes_with_fresh_pcm() {
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        for _ in 0..MAX_PENDING {
            source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
            thread::sleep(Duration::from_millis(25));
        }
        let mut tail = 0;
        for _ in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected PCM wave")
            };
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let ServerAudioOutputPdu::Wave2(wave) =
                decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
            else {
                panic!("expected Wave2")
            };
            tail = wave.block_no;
        }
        source.write_all(&vec![0x22; BLOCK_BYTES * 20]).unwrap();
        thread::sleep(Duration::from_millis(80));
        assert!(
            receiver.is_empty(),
            "transport credit must bound queued sound"
        );
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no: tail,
                    timestamp: 0,
                }))
                .unwrap(),
            )
            .unwrap();
        source.write_all(&vec![0x33; BLOCK_BYTES]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _))) = receiver.try_recv()
            {
                assert_eq!(
                    data,
                    vec![0x33; BLOCK_BYTES],
                    "recovery must not replay stale capture"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected fresh PCM after receipt"
            );
            thread::sleep(Duration::from_millis(1));
        }
        drop(server);
    }

    #[test]
    fn missing_render_tail_and_clock_like_echo_do_not_block_current_pcm() {
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        let take_wave = |receiver: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    assert_eq!(timestamp, 0);
                    return data;
                }
                assert!(Instant::now() < deadline, "expected current PCM");
                thread::sleep(Duration::from_millis(1));
            }
        };
        let dispatch = |server: &mut RdpsndServer, data: Vec<u8>| {
            let messages: Vec<_> = server.wave(data, 0).unwrap().into();
            let ServerAudioOutputPdu::Wave2(wave) =
                decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
            else {
                panic!("expected Wave2")
            };
            server
                .process(
                    &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                        block_no: wave.block_no,
                        timestamp: 0,
                    }))
                    .unwrap(),
                )
                .unwrap();
            wave.block_no
        };
        source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
        let first = dispatch(&mut server, take_wave(&mut receiver));
        server
            .process(
                &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                    block_no: first,
                    timestamp: 56301,
                }))
                .unwrap(),
            )
            .unwrap();
        source.write_all(&vec![0x22; BLOCK_BYTES]).unwrap();
        dispatch(&mut server, take_wave(&mut receiver));
        // No render echo arrives for the tail while the application pauses.
        thread::sleep(Duration::from_millis(600));
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
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![format]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        const QUANTUM_BYTES: usize = 4096;
        let samples: Vec<u8> = (0..QUANTUM_BYTES * 32 / 4)
            .flat_map(|sample| (sample as u32).to_le_bytes())
            .collect();
        let complete = &samples[..samples.len() / block_bytes * block_bytes];
        let expected = if let Some(encoder) = &mut encoder {
            complete
                .chunks(block_bytes)
                .filter_map(|pcm| encoder.encode(pcm.to_vec()).unwrap())
                .flatten()
                .collect()
        } else {
            complete.to_vec()
        };
        let producer = thread::spawn(move || {
            let start = Instant::now();
            for (number, chunk) in samples.chunks(QUANTUM_BYTES).enumerate() {
                let deadline = start
                    + Duration::from_secs_f64(number as f64 * QUANTUM_BYTES as f64 / 192000.0);
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                source.write_all(chunk).unwrap();
            }
            // Keep source open until the consumer verifies all complete blocks.
            source
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = Vec::new();
        while received.len() < expected.len() && Instant::now() < deadline {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                received.extend_from_slice(&data);
                let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
                let ServerAudioOutputPdu::Wave2(wave) =
                    decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
                else {
                    panic!("expected wave2");
                };
                server
                    .process(
                        &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                            block_no: wave.block_no,
                            timestamp: wave.timestamp,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let source = producer.join().unwrap();
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
        let mut unsupported = pcm();
        unsupported.n_channels = 1;
        let (server, receiver) = negotiate(&audio, vec![unsupported]);
        assert!(!server.is_ready());
        assert!(receiver.is_empty());
    }
}
