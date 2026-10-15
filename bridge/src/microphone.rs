//! Windows microphone redirection: MS-RDPEAI `AUDIO_INPUT` into the session's
//! private PipeWire graph.
//!
//! The path is fully separate from playback (`audio.rs`, RDPSND): its own
//! dynamic channel, its own `rdp-microphone` thread per helper connection,
//! its own bounded drop-oldest queue, its own socket (`audio-in-0`, handed
//! over by the root broker as the third descriptor), its own NRM1 framing and
//! its own counters. It shares no flow control, clock or state with playback.
//! A microphone failure is logged once and leaves the microphone unavailable
//! until the next connection; it never ends the RDP session or the desktop.
//!
//! Recording follows the session helper: the client's capture is opened while
//! the helper reports a local recorder (IN_USE), and stopped by recycling the
//! AUDIO_INPUT channel when it reports none (IDLE, after its own debounce).
use crate::performance::{ConnectionStats, Counter};
use ironrdp_rdpeai::pdu::MAX_DATA_PACKET_SIZE;
use ironrdp_rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp_server::{
    RdpeaiServerBackend, RdpeaiServerFactory, RdpeaiServerMessage, ServerEvent, ServerEventSender,
};
use std::{
    collections::VecDeque,
    io::Read,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};
use tokio::sync::mpsc::UnboundedSender;

/// Formats offered in the AUDIO_INPUT Sound Formats PDU, in preference order:
/// 48 kHz matches the graph, and mono halves the upload of a normal microphone.
pub const OFFER: [PcmFormat; 4] = [
    PcmFormat::new(48_000, 1),
    PcmFormat::new(48_000, 2),
    PcmFormat::new(44_100, 1),
    PcmFormat::new(44_100, 2),
];
/// Open requests 20 ms packets.
const PACKET_MS: u32 = 20;
/// Audio older than this is never sent: the bridge queue holds at most this
/// much, and a frame that waited longer is dropped before its first byte.
const QUEUE_BUDGET_NS: u64 = 100_000_000;
/// After every Open attempt failed, retry only on a new recorder start that
/// comes at least this long after the failure.
const RETRY_AFTER_NS: u64 = 30_000_000_000;
/// An Open the client has not answered by then counts as failed: the channel
/// is recycled, so Windows releases the device and a late reply reaches a
/// closed backend, and [`RETRY_AFTER_NS`] applies. Slow device opens (a
/// Bluetooth headset switching profile) take a few seconds at most.
const OPEN_TIMEOUT_NS: u64 = 10_000_000_000;
/// A small socket buffer keeps the helper's view of the audio fresh.
const SEND_BUFFER_BYTES: libc::c_int = 8 * 1024;
/// Upper bound on a worker wait, so the retry window is re-evaluated.
const TICK_MS: libc::c_int = 1000;
/// KSDATAFORMAT_SUBTYPE_PCM, {00000001-0000-0010-8000-00AA00389B71}.
const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

/// NRM1, the framing on `audio-in-0` between the frontend and the session's
/// `session-microphone` helper. All integers are big endian.
pub mod wire {
    pub const MAGIC: &[u8; 4] = b"NRM1";
    /// Frontend records: magic, kind (u8), channels (u8), flags (u16, zero),
    /// rate (u32), sequence (u32), length (u32), received_ns (u64), then
    /// `length` bytes of S16LE PCM for AUDIO.
    pub const HEADER_BYTES: usize = 28;
    /// Helper records: magic, kind (u8), three reserved zero bytes.
    pub const HELPER_RECORD_BYTES: usize = 8;
    /// 100 ms of 48 kHz stereo S16LE.
    pub const MAX_AUDIO_BYTES: usize = 19_200;
    /// Sent once, first. Channels, rate, sequence, length and time are zero.
    pub const HELLO: u8 = 0;
    /// The client negotiated this rate and channel count; AUDIO in it follows.
    pub const AVAILABLE: u8 = 1;
    /// An earlier AVAILABLE no longer holds. Format fields are zero.
    pub const UNAVAILABLE: u8 = 2;
    /// One frame of captured audio; the sequence counts AUDIO per connection.
    pub const AUDIO: u8 = 3;
    pub const HELPER_HELLO: u8 = 0;
    /// A local recorder uses the source: the frontend opens the capture.
    pub const HELPER_IN_USE: u8 = 1;
    /// No local recorder (after the helper's debounce): the frontend stops it.
    pub const HELPER_IDLE: u8 = 2;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Header {
        pub kind: u8,
        pub channels: u8,
        pub rate: u32,
        pub sequence: u32,
        pub length: u32,
        /// CLOCK_MONOTONIC when the frontend received the Data PDU.
        pub received_ns: u64,
    }
    impl Header {
        pub fn encode(&self) -> [u8; HEADER_BYTES] {
            let mut bytes = [0; HEADER_BYTES];
            bytes[..4].copy_from_slice(MAGIC);
            bytes[4] = self.kind;
            bytes[5] = self.channels;
            bytes[8..12].copy_from_slice(&self.rate.to_be_bytes());
            bytes[12..16].copy_from_slice(&self.sequence.to_be_bytes());
            bytes[16..20].copy_from_slice(&self.length.to_be_bytes());
            bytes[20..28].copy_from_slice(&self.received_ns.to_be_bytes());
            bytes
        }
        /// The receiver's validation, mirrored for tests and documentation.
        pub fn decode(bytes: &[u8; HEADER_BYTES]) -> Result<Self, &'static str> {
            if &bytes[..4] != MAGIC {
                return Err("bad magic");
            }
            if bytes[6..8] != [0, 0] {
                return Err("non-zero flags");
            }
            let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
            let header = Self {
                kind: bytes[4],
                channels: bytes[5],
                rate: word(8),
                sequence: word(12),
                length: word(16),
                received_ns: u64::from_be_bytes(bytes[20..28].try_into().unwrap()),
            };
            match header.kind {
                HELLO | UNAVAILABLE if header.channels != 0 || header.length != 0 => {
                    Err("control record carries a format or payload")
                }
                HELLO | UNAVAILABLE => Ok(header),
                AVAILABLE | AUDIO => {
                    if !matches!(header.channels, 1 | 2) {
                        return Err("unsupported channel count");
                    }
                    if !matches!(header.rate, 44_100 | 48_000) {
                        return Err("unsupported rate");
                    }
                    let length = header.length as usize;
                    if header.kind == AVAILABLE && length != 0 {
                        return Err("AVAILABLE carries a payload");
                    }
                    if length > MAX_AUDIO_BYTES
                        || !length.is_multiple_of(2 * usize::from(header.channels))
                    {
                        return Err("invalid audio length");
                    }
                    Ok(header)
                }
                _ => Err("unknown kind"),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum HelperMessage {
        Hello,
        InUse,
        Idle,
    }
    impl HelperMessage {
        pub fn encode(self) -> [u8; HELPER_RECORD_BYTES] {
            let mut bytes = [0; HELPER_RECORD_BYTES];
            bytes[..4].copy_from_slice(MAGIC);
            bytes[4] = match self {
                Self::Hello => HELPER_HELLO,
                Self::InUse => HELPER_IN_USE,
                Self::Idle => HELPER_IDLE,
            };
            bytes
        }
        pub fn decode(bytes: &[u8; HELPER_RECORD_BYTES]) -> Result<Self, &'static str> {
            if &bytes[..4] != MAGIC {
                return Err("bad magic");
            }
            if bytes[5..] != [0, 0, 0] {
                return Err("non-zero reserved bytes");
            }
            match bytes[4] {
                HELPER_HELLO => Ok(Self::Hello),
                HELPER_IN_USE => Ok(Self::InUse),
                HELPER_IDLE => Ok(Self::Idle),
                _ => Err("unknown kind"),
            }
        }
    }
}

/// A 16-bit PCM format this server offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u8,
}
impl PcmFormat {
    pub const fn new(rate: u32, channels: u8) -> Self {
        Self { rate, channels }
    }
    fn block_align(self) -> usize {
        usize::from(self.channels) * 2
    }
    pub fn frames_per_packet(self) -> u32 {
        self.rate * PACKET_MS / 1000
    }
    /// Bytes of one 20 ms frame, the unit queued and sent to the helper.
    fn frame_bytes(self) -> usize {
        self.frames_per_packet() as usize * self.block_align()
    }
    fn duration_ns(self, bytes: usize) -> u64 {
        (bytes / self.block_align()) as u64 * 1_000_000_000 / u64::from(self.rate)
    }
    /// The offered format a client's (negotiated or Data) format names.
    pub fn from_audio_format(format: &AudioFormat) -> Option<Self> {
        if format.format != WaveFormat::PCM || format.bits_per_sample != 16 {
            return None;
        }
        let candidate = Self::new(
            format.n_samples_per_sec,
            u8::try_from(format.n_channels).ok()?,
        );
        OFFER.contains(&candidate).then_some(candidate)
    }
    fn fields(self, format: WaveFormat, data: Option<Vec<u8>>) -> AudioFormat {
        let block_align = self.block_align() as u16;
        AudioFormat {
            format,
            n_channels: u16::from(self.channels),
            n_samples_per_sec: self.rate,
            n_avg_bytes_per_sec: self.rate * u32::from(block_align),
            n_block_align: block_align,
            bits_per_sample: 16,
            data,
        }
    }
    /// Plain WAVE_FORMAT_PCM, as offered and as the fallback capture format.
    pub fn audio_format(self) -> AudioFormat {
        self.fields(WaveFormat::PCM, None)
    }
    /// WAVE_FORMAT_EXTENSIBLE PCM, the first capture format Open asks for.
    pub fn extensible(self) -> AudioFormat {
        let mask: u32 = if self.channels == 1 { 0x4 } else { 0x3 };
        let mut data = Vec::with_capacity(22);
        data.extend_from_slice(&16u16.to_le_bytes());
        data.extend_from_slice(&mask.to_le_bytes());
        data.extend_from_slice(&SUBTYPE_PCM);
        self.fields(WaveFormat::EXTENSIBLE, Some(data))
    }
    fn name(self) -> String {
        format!(
            "{} Hz {}",
            self.rate,
            if self.channels == 1 { "mono" } else { "stereo" }
        )
    }
}

/// One Open request: the client's index of a negotiated format, and whether
/// the capture format is WAVE_FORMAT_EXTENSIBLE or plain PCM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub index: u32,
    pub format: PcmFormat,
    pub extensible: bool,
}
impl Attempt {
    fn message(self) -> RdpeaiServerMessage {
        RdpeaiServerMessage::Open {
            frames_per_packet: self.format.frames_per_packet(),
            initial_format: self.index,
            capture_format: if self.extensible {
                self.format.extensible()
            } else {
                self.format.audio_format()
            },
        }
    }
}

/// Open attempts for a negotiated list (in the client's order), in our
/// preference order: each format first as EXTENSIBLE, then as plain PCM.
pub fn open_ladder(negotiated: &[AudioFormat]) -> Vec<Attempt> {
    let mut ladder = Vec::new();
    for format in OFFER {
        let Some(index) = negotiated
            .iter()
            .position(|offered| PcmFormat::from_audio_format(offered) == Some(format))
        else {
            continue;
        };
        for extensible in [true, false] {
            ladder.push(Attempt {
                index: index as u32,
                format,
                extensible,
            });
        }
    }
    ladder
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Channel {
    /// No negotiated AUDIO_INPUT channel with a usable format.
    #[default]
    Absent,
    /// Negotiated; no Open outstanding.
    Ready,
    /// Open sent; the client's reply is outstanding.
    Opening,
    /// The client is capturing.
    Opened,
    /// A recycle was requested; the channel closes and is offered again.
    Closing,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Open(Attempt),
    Recycle,
    /// The outstanding Open went unanswered for [`OPEN_TIMEOUT_NS`].
    OpenTimeout,
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    /// From a backend this connection no longer uses.
    Stale,
    /// An Open this connection did not send (queued for an earlier one).
    Unsolicited,
    Opened(PcmFormat),
    Failed(i32),
    /// Every attempt failed; wait for a new recorder and the retry window.
    Exhausted(i32),
}

#[derive(Debug, PartialEq, Eq)]
enum Admission {
    Queue,
    /// Received while nobody records or before the reply; dropped.
    Drop,
    /// Not the opened format.
    Invalid,
}

/// The open/stop decisions, free of I/O so the policy is testable.
#[derive(Debug, Default)]
struct Control {
    /// The backend whose channel negotiated most recently.
    backend: Option<u64>,
    channel: Channel,
    /// Every attempt for the current negotiation.
    full: Vec<Attempt>,
    /// Attempts not yet failed; the front is outstanding or next.
    ladder: VecDeque<Attempt>,
    /// The attempt that worked earlier on this connection goes first.
    worked: Option<Attempt>,
    opened: Option<PcmFormat>,
    /// What the helper should be told: AVAILABLE with this format, or not.
    available: Option<PcmFormat>,
    /// A helper connection is attached.
    attached: bool,
    /// The helper reports a local recorder.
    in_use: bool,
    /// When the last attempt failed, while no further attempt is allowed.
    failed_at: Option<u64>,
    /// A recorder started after that failure.
    edge_since_failure: bool,
    /// When the outstanding Open was sent.
    opening_since: u64,
}
impl Control {
    fn wanted(&self) -> bool {
        self.attached && self.in_use
    }
    fn evaluate(&mut self, now: u64) -> Vec<Command> {
        match self.channel {
            Channel::Ready if self.wanted() => {
                if self.ladder.is_empty() {
                    match self.failed_at {
                        Some(at)
                            if self.edge_since_failure
                                && now.saturating_sub(at) >= RETRY_AFTER_NS =>
                        {
                            self.failed_at = None;
                            self.edge_since_failure = false;
                            self.ladder = self.full.iter().copied().collect();
                        }
                        _ => return Vec::new(),
                    }
                }
                let Some(attempt) = self.ladder.front().copied() else {
                    return Vec::new();
                };
                self.channel = Channel::Opening;
                self.opening_since = now;
                vec![Command::Open(attempt)]
            }
            Channel::Opening | Channel::Opened if !self.wanted() => {
                self.channel = Channel::Closing;
                self.opened = None;
                vec![Command::Recycle]
            }
            Channel::Opening if now.saturating_sub(self.opening_since) >= OPEN_TIMEOUT_NS => {
                self.channel = Channel::Closing;
                self.opened = None;
                self.ladder.clear();
                self.failed_at = Some(now);
                self.edge_since_failure = false;
                vec![Command::OpenTimeout]
            }
            _ => Vec::new(),
        }
    }
    fn negotiated(&mut self, backend: u64, formats: &[AudioFormat], now: u64) -> Vec<Command> {
        let mut ladder = open_ladder(formats);
        if let Some(worked) = self.worked
            && let Some(position) = ladder.iter().position(|attempt| {
                (attempt.format, attempt.extensible) == (worked.format, worked.extensible)
            })
        {
            let attempt = ladder.remove(position);
            ladder.insert(0, attempt);
        }
        self.backend = Some(backend);
        self.opened = None;
        self.available = ladder.first().map(|attempt| attempt.format);
        self.channel = if ladder.is_empty() {
            Channel::Absent
        } else {
            Channel::Ready
        };
        // A failure earlier on this connection still holds after a recycle.
        self.ladder = if self.failed_at.is_some() {
            VecDeque::new()
        } else {
            ladder.iter().copied().collect()
        };
        self.full = ladder;
        self.evaluate(now)
    }
    fn open_reply(&mut self, backend: u64, result: i32, now: u64) -> (Vec<Command>, Reply) {
        if self.backend != Some(backend) {
            return (Vec::new(), Reply::Stale);
        }
        if self.channel != Channel::Opening {
            // Capture this connection never asked for: stop it at once.
            let commands = if result >= 0 && self.channel == Channel::Ready {
                self.channel = Channel::Closing;
                vec![Command::Recycle]
            } else {
                Vec::new()
            };
            return (commands, Reply::Unsolicited);
        }
        let Some(attempt) = self.ladder.front().copied() else {
            self.channel = Channel::Ready;
            return (self.evaluate(now), Reply::Unsolicited);
        };
        let reply = if result >= 0 {
            self.channel = Channel::Opened;
            self.opened = Some(attempt.format);
            self.available = Some(attempt.format);
            self.worked = Some(attempt);
            Reply::Opened(attempt.format)
        } else {
            self.ladder.pop_front();
            self.channel = Channel::Ready;
            if self.ladder.is_empty() {
                self.failed_at = Some(now);
                self.edge_since_failure = false;
                Reply::Exhausted(result)
            } else {
                Reply::Failed(result)
            }
        };
        // The recorder may have stopped while the client opened its device.
        (self.evaluate(now), reply)
    }
    fn closed(&mut self, backend: u64) {
        if self.backend != Some(backend) {
            return;
        }
        // After a recycle the replacement renegotiates the same formats, so
        // the helper keeps its announcement; a client-side close ends it.
        if self.channel != Channel::Closing {
            self.available = None;
        }
        self.backend = None;
        self.channel = Channel::Absent;
        self.opened = None;
        self.full.clear();
        self.ladder.clear();
    }
    fn set_in_use(&mut self, in_use: bool, now: u64) -> Vec<Command> {
        if in_use && !self.in_use && self.failed_at.is_some() {
            self.edge_since_failure = true;
        }
        self.in_use = in_use;
        self.evaluate(now)
    }
    fn attach(&mut self, now: u64) -> Vec<Command> {
        self.attached = true;
        // The helper reports its state right after its HELLO.
        self.in_use = false;
        self.evaluate(now)
    }
    /// The helper connection ended (or the RDP connection did).
    fn detach(&mut self, now: u64) -> Vec<Command> {
        self.attached = false;
        self.in_use = false;
        self.failed_at = None;
        self.edge_since_failure = false;
        self.worked = None;
        if self.channel == Channel::Ready {
            self.ladder = self.full.iter().copied().collect();
        }
        self.evaluate(now)
    }
    fn admit(&self, backend: u64, format: PcmFormat) -> Admission {
        if self.backend != Some(backend) || self.channel != Channel::Opened || !self.wanted() {
            Admission::Drop
        } else if self.opened != Some(format) {
            Admission::Invalid
        } else {
            Admission::Queue
        }
    }
}

/// One 20 ms (or shorter) piece of captured audio.
#[derive(Debug)]
struct Frame {
    format: PcmFormat,
    sequence: u32,
    received_ns: u64,
    bytes: Vec<u8>,
}
impl Frame {
    fn duration_ns(&self) -> u64 {
        self.format.duration_ns(self.bytes.len())
    }
    fn header(&self) -> wire::Header {
        wire::Header {
            kind: wire::AUDIO,
            channels: self.format.channels,
            rate: self.format.rate,
            sequence: self.sequence,
            length: self.bytes.len() as u32,
            received_ns: self.received_ns,
        }
    }
}

/// At most [`QUEUE_BUDGET_NS`] of audio; the oldest whole frames go first.
#[derive(Debug, Default)]
struct Queue {
    frames: VecDeque<Frame>,
    duration_ns: u64,
}
impl Queue {
    /// Returns how many frames were dropped to make room.
    fn push(&mut self, frame: Frame) -> u64 {
        let mut dropped = 0;
        while self.duration_ns + frame.duration_ns() > QUEUE_BUDGET_NS {
            let Some(old) = self.frames.pop_front() else {
                break;
            };
            self.duration_ns -= old.duration_ns();
            dropped += 1;
        }
        self.duration_ns += frame.duration_ns();
        self.frames.push_back(frame);
        dropped
    }
    /// The oldest frame still young enough to send, and how many older
    /// frames were dropped on the way.
    fn pop_fresh(&mut self, now: u64) -> (Option<Frame>, u64) {
        let mut dropped = 0;
        while let Some(frame) = self.frames.pop_front() {
            self.duration_ns -= frame.duration_ns();
            if now.saturating_sub(frame.received_ns) > QUEUE_BUDGET_NS {
                dropped += 1;
                continue;
            }
            return (Some(frame), dropped);
        }
        (None, dropped)
    }
    fn clear(&mut self) -> u64 {
        let dropped = self.frames.len() as u64;
        self.frames.clear();
        self.duration_ns = 0;
        dropped
    }
    fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// Monotonic time for frame stamps, expiry and the retry window. Tests
/// substitute a manual clock.
trait Clock: Send + Sync {
    /// CLOCK_MONOTONIC nanoseconds, comparable with the helper's.
    fn now_ns(&self) -> u64;
}
struct MonotonicClock;
impl Clock for MonotonicClock {
    fn now_ns(&self) -> u64 {
        let mut value: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: CLOCK_MONOTONIC with a valid out-pointer cannot fail on Linux.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) };
        value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
    }
}

/// Wakes one worker's poll(2).
struct Wake(OwnedFd);
impl Wake {
    fn new() -> std::io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: eventfd returned a new descriptor that nothing else owns.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
    fn notify(&self) {
        let one = 1u64;
        // A full counter already wakes the worker; nothing to handle.
        let _ = unsafe { libc::write(self.0.as_raw_fd(), (&raw const one).cast(), 8) };
    }
    fn drain(&self) {
        let mut value = 0u64;
        let _ = unsafe { libc::read(self.0.as_raw_fd(), (&raw mut value).cast(), 8) };
    }
}

/// The running worker of the attached helper connection.
struct Link {
    wake: Arc<Wake>,
}

#[derive(Default)]
struct State {
    control: Control,
    queue: Queue,
    /// The attached helper connection's worker, if any.
    link: Option<Link>,
    /// Bumped by every attach and clear; a worker of an older one exits.
    generation: u64,
    /// AUDIO sequence of the current helper connection.
    sequence: u32,
}

struct Shared {
    state: Mutex<State>,
    sender: Mutex<Option<UnboundedSender<ServerEvent>>>,
    clock: Arc<dyn Clock>,
    stats: &'static ConnectionStats,
    backends: AtomicU64,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
    /// Send under the state lock, so commands reach the server in decision
    /// order. The channel is unbounded: this never blocks.
    fn send(&self, commands: Vec<Command>) {
        if commands.is_empty() {
            return;
        }
        let sender = self.sender.lock().unwrap_or_else(PoisonError::into_inner);
        for command in commands {
            let event = match command {
                Command::Open(attempt) => {
                    eprintln!(
                        "RDP microphone opening: PCM {}{}",
                        attempt.format.name(),
                        if attempt.extensible {
                            ""
                        } else {
                            " (plain capture format)"
                        }
                    );
                    ServerEvent::Rdpeai(attempt.message())
                }
                Command::Recycle => {
                    eprintln!("RDP microphone stopping: no remote recorder");
                    ServerEvent::RdpeaiRecycle
                }
                Command::OpenTimeout => {
                    self.stats.add(Counter::MicErrors, 1);
                    eprintln!(
                        "RDP microphone unavailable: the client did not answer Open within {} s; \
                         retrying when a recorder starts again after 30 s",
                        OPEN_TIMEOUT_NS / 1_000_000_000
                    );
                    ServerEvent::RdpeaiRecycle
                }
            };
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(event);
            }
        }
    }
    fn wake(state: &State) {
        if let Some(link) = &state.link {
            link.wake.notify();
        }
    }
    fn update(&self, change: impl FnOnce(&mut Control, u64) -> Vec<Command>) {
        let now = self.clock.now_ns();
        let mut state = self.lock();
        let commands = change(&mut state.control, now);
        if state.control.channel != Channel::Opened {
            let dropped = state.queue.clear();
            self.stats.add(Counter::MicDropped, dropped);
        }
        self.send(commands);
        Self::wake(&state);
    }
    /// Validate, split into frames and queue. O(1) per packet; the lock is
    /// held only to push.
    fn audio(&self, backend: u64, format: &AudioFormat, data: &[u8]) {
        let now = self.clock.now_ns();
        let Some(pcm) = valid_packet(format, data) else {
            self.stats.add(Counter::MicErrors, 1);
            return;
        };
        self.stats.add(Counter::MicPackets, 1);
        self.stats.add(Counter::MicBytes, data.len() as u64);
        let frames = data.chunks(pcm.frame_bytes());
        let mut state = self.lock();
        match state.control.admit(backend, pcm) {
            Admission::Queue => {}
            Admission::Drop => {
                self.stats.add(Counter::MicDropped, frames.len() as u64);
                return;
            }
            Admission::Invalid => {
                self.stats.add(Counter::MicErrors, 1);
                return;
            }
        }
        // A packet longer than the budget keeps only its newest frames; the
        // skipped ones still take sequence numbers, so the helper sees a gap.
        let keep = (QUEUE_BUDGET_NS / pcm.duration_ns(pcm.frame_bytes())) as usize;
        let skip = frames.len().saturating_sub(keep);
        let mut dropped = skip as u64;
        for (index, bytes) in frames.enumerate() {
            let sequence = state.sequence;
            state.sequence = sequence.wrapping_add(1);
            if index < skip {
                continue;
            }
            dropped += state.queue.push(Frame {
                format: pcm,
                sequence,
                received_ns: now,
                bytes: bytes.to_vec(),
            });
        }
        self.stats.add(Counter::MicDropped, dropped);
        Self::wake(&state);
    }
}

/// A Data PDU's payload is accepted only as aligned, non-empty, bounded
/// 16-bit PCM in an offered format.
fn valid_packet(format: &AudioFormat, data: &[u8]) -> Option<PcmFormat> {
    let pcm = PcmFormat::from_audio_format(format)?;
    (!data.is_empty()
        && data.len() <= MAX_DATA_PACKET_SIZE
        && data.len().is_multiple_of(pcm.block_align()))
    .then_some(pcm)
}

/// The frontend's microphone: the helper connection of the session being
/// served, and the factory for the server's AUDIO_INPUT backends.
#[derive(Clone)]
pub struct Microphone {
    shared: Arc<Shared>,
}
impl Default for Microphone {
    fn default() -> Self {
        Self::with_clock(Arc::new(MonotonicClock), crate::performance::connection())
    }
}
impl std::fmt::Debug for Microphone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionMicrophone")
    }
}
impl Microphone {
    fn with_clock(clock: Arc<dyn Clock>, stats: &'static ConnectionStats) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::default(),
                sender: Mutex::default(),
                clock,
                stats,
                backends: AtomicU64::new(0),
            }),
        }
    }
    pub fn factory(&self) -> Factory {
        Factory {
            shared: self.shared.clone(),
        }
    }
    /// Attach the session helper's `audio-in-0` connection for the RDP
    /// connection now being served; `None` leaves the microphone unavailable.
    pub fn attach(&self, socket: Option<UnixStream>) {
        let now = self.shared.clock.now_ns();
        let mut state = self.shared.lock();
        Self::detach(&self.shared, &mut state, now);
        let Some(socket) = socket else {
            eprintln!("RDP microphone helper unavailable: desktop and playback continue");
            return;
        };
        let wake = match Wake::new().and_then(|wake| {
            socket.set_nonblocking(true)?;
            Ok(wake)
        }) {
            Ok(wake) => Arc::new(wake),
            Err(error) => {
                eprintln!("RDP microphone unavailable: {error}; desktop and playback continue");
                return;
            }
        };
        // A small buffer bounds the audio waiting in the socket; best effort.
        let buffer = SEND_BUFFER_BYTES;
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const buffer).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let generation = state.generation;
        let shared = self.shared.clone();
        let worker_wake = wake.clone();
        let spawned = thread::Builder::new()
            .name("rdp-microphone".into())
            .spawn(move || run_worker(shared, socket, generation, worker_wake));
        if let Err(error) = spawned {
            eprintln!("RDP microphone unavailable: {error}; desktop and playback continue");
            return;
        }
        state.link = Some(Link { wake });
        state.sequence = 0;
        let commands = state.control.attach(now);
        self.shared.send(commands);
    }
    /// End the helper connection: its worker exits promptly without sending
    /// queued audio, and any capture is stopped.
    pub fn clear(&self) {
        let now = self.shared.clock.now_ns();
        let mut state = self.shared.lock();
        Self::detach(&self.shared, &mut state, now);
    }
    fn detach(shared: &Shared, state: &mut State, now: u64) {
        state.generation = state.generation.wrapping_add(1);
        if let Some(link) = state.link.take() {
            link.wake.notify();
        }
        let dropped = state.queue.clear();
        shared.stats.add(Counter::MicDropped, dropped);
        let commands = state.control.detach(now);
        shared.send(commands);
    }
}

pub struct Factory {
    shared: Arc<Shared>,
}
impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self
            .shared
            .sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sender);
    }
}
impl RdpeaiServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpeaiServerBackend> {
        Box::new(Backend {
            shared: self.shared.clone(),
            id: self.shared.backends.fetch_add(1, Ordering::Relaxed),
            formats: OFFER.map(PcmFormat::audio_format).to_vec(),
        })
    }
}

/// One AUDIO_INPUT channel instance. Every callback runs in the server task
/// and returns without waiting on I/O.
struct Backend {
    shared: Arc<Shared>,
    id: u64,
    formats: Vec<AudioFormat>,
}
impl RdpeaiServerBackend for Backend {
    fn supported_formats(&self) -> &[AudioFormat] {
        &self.formats
    }
    fn on_formats_negotiated(&mut self, negotiated: &[AudioFormat]) {
        if open_ladder(negotiated).is_empty() {
            eprintln!("RDP microphone unavailable: the client offers no 16-bit PCM input format");
        }
        let id = self.id;
        self.shared
            .update(|control, now| control.negotiated(id, negotiated, now));
    }
    fn on_open_reply(&mut self, result: i32) {
        let id = self.id;
        let stats = self.shared.stats;
        self.shared.update(|control, now| {
            let (commands, reply) = control.open_reply(id, result, now);
            match reply {
                Reply::Stale => {}
                Reply::Unsolicited => {
                    eprintln!("RDP microphone ignored a capture this connection did not request")
                }
                Reply::Opened(format) => {
                    stats.add(Counter::MicOpens, 1);
                    stats.set_microphone_format(format.rate, format.channels);
                    eprintln!("RDP microphone recording: PCM {}", format.name());
                }
                Reply::Failed(result) => {
                    stats.add(Counter::MicErrors, 1);
                    eprintln!(
                        "RDP microphone: the client could not open capture ({:#010x}); trying another format",
                        result as u32
                    );
                }
                Reply::Exhausted(result) => {
                    stats.add(Counter::MicErrors, 1);
                    eprintln!(
                        "RDP microphone unavailable: the client could not open capture ({:#010x}); \
                         retrying when a recorder starts again after 30 s",
                        result as u32
                    );
                }
            }
            commands
        });
    }
    fn on_audio_data(&mut self, format: &AudioFormat, data: &[u8]) {
        self.shared.audio(self.id, format, data);
    }
    fn on_close(&mut self) {
        let id = self.id;
        self.shared.update(|control, _| {
            control.closed(id);
            Vec::new()
        });
    }
}

/// Why a helper connection ended.
enum End {
    /// Replaced or cleared by the frontend.
    Superseded,
    Failed(String),
}

/// The `rdp-microphone` worker of one helper connection: frames queued audio
/// as NRM1, reads the helper's IN_USE/IDLE, and exits when superseded.
fn run_worker(shared: Arc<Shared>, mut socket: UnixStream, generation: u64, wake: Arc<Wake>) {
    let mut out = Vec::with_capacity(wire::HEADER_BYTES * 2 + wire::MAX_AUDIO_BYTES);
    let mut sent = 0;
    out.extend_from_slice(
        &wire::Header {
            kind: wire::HELLO,
            ..Default::default()
        }
        .encode(),
    );
    let mut announced: Option<PcmFormat> = None;
    let mut inbound = [0u8; wire::HELPER_RECORD_BYTES];
    let mut inbound_len = 0;
    let mut greeted = false;
    let end = 'run: loop {
        let mut more = false;
        {
            let now = shared.clock.now_ns();
            let mut state = shared.lock();
            if state.generation != generation {
                break End::Superseded;
            }
            let commands = state.control.evaluate(now);
            shared.send(commands);
            let available = state.control.available;
            if available != announced {
                let header = match available {
                    Some(format) => wire::Header {
                        kind: wire::AVAILABLE,
                        channels: format.channels,
                        rate: format.rate,
                        ..Default::default()
                    },
                    None => wire::Header {
                        kind: wire::UNAVAILABLE,
                        ..Default::default()
                    },
                };
                out.extend_from_slice(&header.encode());
                announced = available;
            }
            // Take a frame only once the previous one is fully written, so
            // audio waits in the bounded queue, where expiry applies.
            if sent == out.len() {
                out.clear();
                sent = 0;
                let (frame, dropped) = state.queue.pop_fresh(now);
                shared.stats.add(Counter::MicDropped, dropped);
                if let Some(frame) = frame {
                    out.extend_from_slice(&frame.header().encode());
                    out.extend_from_slice(&frame.bytes);
                }
                more = !state.queue.is_empty();
            }
        }
        while sent < out.len() {
            // MSG_NOSIGNAL: a vanished helper is an error, never SIGPIPE.
            let n = unsafe {
                libc::send(
                    socket.as_raw_fd(),
                    out[sent..].as_ptr().cast(),
                    out.len() - sent,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if n >= 0 {
                sent += n as usize;
                continue;
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {
                    more = false;
                    break;
                }
                _ => break 'run End::Failed(format!("write failed: {error}")),
            }
        }
        loop {
            match socket.read(&mut inbound[inbound_len..]) {
                Ok(0) => break 'run End::Failed("helper closed the connection".into()),
                Ok(n) => {
                    inbound_len += n;
                    if inbound_len < inbound.len() {
                        continue;
                    }
                    inbound_len = 0;
                    let message = match wire::HelperMessage::decode(&inbound) {
                        Ok(message) => message,
                        Err(error) => break 'run End::Failed(format!("protocol error: {error}")),
                    };
                    let in_use = match (message, greeted) {
                        (wire::HelperMessage::Hello, false) => {
                            greeted = true;
                            continue;
                        }
                        (wire::HelperMessage::InUse, true) => true,
                        (wire::HelperMessage::Idle, true) => false,
                        _ => {
                            break 'run End::Failed(
                                "protocol error: HELLO must come first, and only once".into(),
                            );
                        }
                    };
                    let now = shared.clock.now_ns();
                    let mut state = shared.lock();
                    if state.generation != generation {
                        break 'run End::Superseded;
                    }
                    let commands = state.control.set_in_use(in_use, now);
                    if state.control.channel != Channel::Opened {
                        let dropped = state.queue.clear();
                        shared.stats.add(Counter::MicDropped, dropped);
                    }
                    shared.send(commands);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => break 'run End::Failed(format!("read failed: {error}")),
            }
        }
        if more {
            continue;
        }
        let mut fds = [
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN | if sent < out.len() { libc::POLLOUT } else { 0 },
                revents: 0,
            },
            libc::pollfd {
                fd: wake.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both descriptors stay open for the duration of the call.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, TICK_MS) };
        wake.drain();
    };
    if let End::Failed(reason) = end {
        let now = shared.clock.now_ns();
        let mut state = shared.lock();
        if state.generation == generation {
            eprintln!(
                "RDP microphone helper connection ended ({reason}); microphone unavailable \
                 until reconnect, desktop and playback continue"
            );
            shared.stats.add(Counter::MicErrors, 1);
            Microphone::detach(&shared, &mut state, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::UnboundedReceiver;

    struct ManualClock(AtomicU64);
    impl Clock for ManualClock {
        fn now_ns(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    impl ManualClock {
        fn advance(&self, duration: Duration) {
            self.0
                .fetch_add(duration.as_nanos() as u64, Ordering::SeqCst);
        }
    }

    fn stats() -> &'static ConnectionStats {
        Box::leak(Box::default())
    }

    struct Rig {
        microphone: Microphone,
        clock: Arc<ManualClock>,
        stats: &'static ConnectionStats,
        events: UnboundedReceiver<ServerEvent>,
        backend: Box<dyn RdpeaiServerBackend>,
    }
    impl Rig {
        fn new() -> Self {
            let clock = Arc::new(ManualClock(AtomicU64::new(1_000_000_000_000)));
            let stats = stats();
            let microphone = Microphone::with_clock(clock.clone(), stats);
            let mut factory = microphone.factory();
            let (sender, events) = tokio::sync::mpsc::unbounded_channel();
            factory.set_sender(sender);
            let backend = factory.build_backend();
            Self {
                microphone,
                clock,
                stats,
                events,
                backend,
            }
        }
        fn events(&mut self) -> Vec<ServerEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                events.push(event);
            }
            events
        }
        fn helper(&self) -> Helper {
            let (frontend, helper) = UnixStream::pair().unwrap();
            self.microphone.attach(Some(frontend));
            let mut helper = Helper(helper);
            helper
                .0
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(helper.record().0.kind, wire::HELLO);
            helper
        }
        fn negotiate(&mut self, formats: &[PcmFormat]) {
            let formats: Vec<_> = formats.iter().map(|f| f.audio_format()).collect();
            self.backend.on_formats_negotiated(&formats);
        }
        /// Wait until the worker has acted on helper input.
        fn wait_event(&mut self) -> ServerEvent {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(event) = self.events.try_recv() {
                    return event;
                }
                assert!(Instant::now() < deadline, "no server event");
                thread::sleep(Duration::from_millis(2));
            }
        }
        /// Negotiated 48 kHz mono, helper attached and in use, capture open.
        fn recording() -> (Self, Helper) {
            let mut rig = Self::new();
            rig.negotiate(&OFFER);
            let mut helper = rig.helper();
            assert_eq!(helper.record().0, available(PcmFormat::new(48_000, 1)));
            helper.send(wire::HelperMessage::Hello);
            helper.send(wire::HelperMessage::InUse);
            assert_open(rig.wait_event(), 0, PcmFormat::new(48_000, 1), true);
            rig.backend.on_open_reply(0);
            assert!(rig.events().is_empty());
            (rig, helper)
        }
    }

    struct Helper(UnixStream);
    impl Helper {
        fn send(&mut self, message: wire::HelperMessage) {
            self.0.write_all(&message.encode()).unwrap();
        }
        fn record(&mut self) -> (wire::Header, Vec<u8>) {
            let mut header = [0; wire::HEADER_BYTES];
            self.0.read_exact(&mut header).unwrap();
            let header = wire::Header::decode(&header).unwrap();
            let mut payload = vec![0; header.length as usize];
            self.0.read_exact(&mut payload).unwrap();
            (header, payload)
        }
        fn nothing_within(&mut self, wait: Duration) -> bool {
            self.0.set_read_timeout(Some(wait)).unwrap();
            let mut byte = [0];
            let result = match self.0.read(&mut byte) {
                Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
                Ok(_) => false,
            };
            self.0
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            result
        }
        /// The frontend closed its end. Closing with unread helper input
        /// shows up as a reset instead of end-of-file.
        fn eof_within(&mut self, wait: Duration) -> bool {
            self.0.set_read_timeout(Some(wait)).unwrap();
            let mut rest = Vec::new();
            match self.0.read_to_end(&mut rest) {
                Ok(_) => true,
                Err(error) => error.kind() == std::io::ErrorKind::ConnectionReset,
            }
        }
    }

    fn available(format: PcmFormat) -> wire::Header {
        wire::Header {
            kind: wire::AVAILABLE,
            channels: format.channels,
            rate: format.rate,
            ..Default::default()
        }
    }

    fn assert_open(event: ServerEvent, index: u32, format: PcmFormat, extensible: bool) {
        let ServerEvent::Rdpeai(RdpeaiServerMessage::Open {
            frames_per_packet,
            initial_format,
            capture_format,
        }) = event
        else {
            panic!("expected Open, got {event:?}");
        };
        assert_eq!(initial_format, index);
        assert_eq!(frames_per_packet, format.frames_per_packet());
        let expected = if extensible {
            format.extensible()
        } else {
            format.audio_format()
        };
        assert_eq!(capture_format, expected);
    }

    fn tone(format: PcmFormat, millis: usize) -> Vec<u8> {
        let frames = format.rate as usize * millis / 1000;
        (0..frames * usize::from(format.channels))
            .flat_map(|i| ((i % 97) as i16 * 300).to_le_bytes())
            .collect()
    }

    #[test]
    fn format_choice_prefers_48k_mono_then_stereo_then_44k_by_our_order() {
        let mono48 = PcmFormat::new(48_000, 1);
        let stereo48 = PcmFormat::new(48_000, 2);
        let mono44 = PcmFormat::new(44_100, 1);
        let stereo44 = PcmFormat::new(44_100, 2);
        let list = |formats: &[PcmFormat]| -> Vec<AudioFormat> {
            formats.iter().map(|f| f.audio_format()).collect()
        };
        // The client's order does not matter; its index for our choice does.
        let ladder = open_ladder(&list(&[stereo44, mono44, stereo48, mono48]));
        let order: Vec<_> = ladder
            .iter()
            .map(|a| (a.index, a.format, a.extensible))
            .collect();
        assert_eq!(
            order,
            [
                (3, mono48, true),
                (3, mono48, false),
                (2, stereo48, true),
                (2, stereo48, false),
                (1, mono44, true),
                (1, mono44, false),
                (0, stereo44, true),
                (0, stereo44, false),
            ]
        );
        // Only 44.1 kHz: the helper is told that rate; nothing resamples.
        assert_eq!(open_ladder(&list(&[stereo44]))[0].format, stereo44);
        assert_eq!(open_ladder(&list(&[stereo44, mono44]))[0].index, 1);
        // Offered formats are plain 16-bit PCM in the approved order.
        let offered = OFFER.map(PcmFormat::audio_format);
        assert!(offered.iter().all(|f| f.format == WaveFormat::PCM
            && f.bits_per_sample == 16
            && f.data.is_none()
            && u32::from(f.n_block_align) == u32::from(f.n_channels) * 2));
        assert_eq!(
            OFFER.map(|f| (f.rate, f.channels)),
            [(48_000, 1), (48_000, 2), (44_100, 1), (44_100, 2)]
        );
        // Anything else is not ours: other rates, depths and compressed formats.
        let mut eight_bit = mono48.audio_format();
        eight_bit.bits_per_sample = 8;
        let mut alaw = mono48.audio_format();
        alaw.format = WaveFormat::ALAW;
        let mut low = mono48.audio_format();
        low.n_samples_per_sec = 16_000;
        assert!(open_ladder(&[eight_bit, alaw, low]).is_empty());
        assert_eq!(mono48.frames_per_packet(), 960);
        assert_eq!(mono44.frames_per_packet(), 882);
    }

    #[test]
    fn open_capture_format_is_extensible_pcm_and_falls_back_to_plain_then_next_format() {
        let mono = PcmFormat::new(48_000, 1).extensible();
        assert_eq!(mono.format, WaveFormat::EXTENSIBLE);
        assert_eq!(
            (mono.n_channels, mono.n_samples_per_sec, mono.n_block_align),
            (1, 48_000, 2)
        );
        assert_eq!(
            (mono.n_avg_bytes_per_sec, mono.bits_per_sample),
            (96_000, 16)
        );
        let data = mono.data.unwrap();
        assert_eq!(data.len(), 22);
        assert_eq!(&data[..2], &16u16.to_le_bytes());
        assert_eq!(&data[2..6], &4u32.to_le_bytes());
        assert_eq!(&data[6..], &SUBTYPE_PCM);
        let stereo = PcmFormat::new(44_100, 2).extensible().data.unwrap();
        assert_eq!(&stereo[2..6], &3u32.to_le_bytes());

        let mut control = Control::default();
        control.attach(0);
        control.set_in_use(true, 0);
        let formats: Vec<_> = [PcmFormat::new(44_100, 2), PcmFormat::new(48_000, 1)]
            .map(PcmFormat::audio_format)
            .to_vec();
        let attempt = |command: &Command| match command {
            Command::Open(attempt) => (attempt.index, attempt.format.rate, attempt.extensible),
            other => panic!("unexpected {other:?}"),
        };
        let commands = control.negotiated(1, &formats, 0);
        assert_eq!(attempt(&commands[0]), (1, 48_000, true));
        let (commands, reply) = control.open_reply(1, -1, 0);
        assert_eq!(reply, Reply::Failed(-1));
        assert_eq!(attempt(&commands[0]), (1, 48_000, false));
        let (commands, _) = control.open_reply(1, -1, 0);
        assert_eq!(attempt(&commands[0]), (0, 44_100, true));
        let (commands, _) = control.open_reply(1, -1, 0);
        assert_eq!(attempt(&commands[0]), (0, 44_100, false));
        let (commands, reply) = control.open_reply(1, 0, 0);
        assert!(commands.is_empty());
        assert_eq!(reply, Reply::Opened(PcmFormat::new(44_100, 2)));
        // The helper learns the format that actually opened.
        assert_eq!(control.available, Some(PcmFormat::new(44_100, 2)));
        // After a recycle, the attempt that worked goes first.
        assert_eq!(control.set_in_use(false, 0), [Command::Recycle]);
        control.closed(1);
        control.set_in_use(true, 0);
        let commands = control.negotiated(2, &formats, 0);
        assert_eq!(attempt(&commands[0]), (0, 44_100, false));
    }

    #[test]
    fn open_is_sent_only_after_negotiation_attach_and_in_use() {
        let mut rig = Rig::new();
        // The login screen: negotiated, but no helper connection.
        rig.negotiate(&OFFER);
        assert!(rig.events().is_empty());
        let mut helper = rig.helper();
        assert_eq!(helper.record().0, available(PcmFormat::new(48_000, 1)));
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::Idle);
        assert!(helper.nothing_within(Duration::from_millis(100)));
        assert!(rig.events().is_empty(), "nothing while no recorder listens");
        helper.send(wire::HelperMessage::InUse);
        assert_open(rig.wait_event(), 0, PcmFormat::new(48_000, 1), true);
        assert!(rig.events().is_empty(), "one Open at a time");

        // In use before negotiation: Open follows negotiation.
        let mut rig = Rig::new();
        let mut helper = rig.helper();
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::InUse);
        assert!(helper.nothing_within(Duration::from_millis(100)));
        assert!(rig.events().is_empty());
        rig.negotiate(&[PcmFormat::new(44_100, 1)]);
        assert_open(rig.events().remove(0), 0, PcmFormat::new(44_100, 1), true);
        assert_eq!(helper.record().0, available(PcmFormat::new(44_100, 1)));
    }

    #[test]
    fn empty_negotiation_reports_unavailable_and_never_opens() {
        let mut rig = Rig::new();
        let mut helper = rig.helper();
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::InUse);
        rig.backend.on_formats_negotiated(&[]);
        assert!(helper.nothing_within(Duration::from_millis(100)));
        assert!(rig.events().is_empty());
        // A connection that had a format and loses it says so once.
        let mut rig = Rig::new();
        rig.negotiate(&OFFER);
        let mut helper = rig.helper();
        assert_eq!(helper.record().0.kind, wire::AVAILABLE);
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::Idle);
        rig.backend.on_close();
        let (header, _) = helper.record();
        assert_eq!(
            header,
            wire::Header {
                kind: wire::UNAVAILABLE,
                ..Default::default()
            }
        );
        helper.send(wire::HelperMessage::InUse);
        assert!(helper.nothing_within(Duration::from_millis(100)));
        assert!(rig.events().is_empty());
    }

    #[test]
    fn audio_data_rejects_empty_misaligned_oversize_and_non_pcm() {
        let (mut rig, mut helper) = Rig::recording();
        let mono = PcmFormat::new(48_000, 1);
        let format = mono.audio_format();
        rig.backend.on_audio_data(&format, &[]);
        rig.backend.on_audio_data(&format, &[1, 2, 3]);
        rig.backend
            .on_audio_data(&format, &vec![0; MAX_DATA_PACKET_SIZE + 2]);
        let mut alaw = format.clone();
        alaw.format = WaveFormat::ALAW;
        rig.backend.on_audio_data(&alaw, &[0; 1920]);
        // A valid format that is not the opened one.
        rig.backend
            .on_audio_data(&PcmFormat::new(48_000, 2).audio_format(), &[0; 3840]);
        assert_eq!(rig.stats.get(Counter::MicErrors), 5);
        assert_eq!(rig.stats.get(Counter::MicPackets), 1);
        assert!(helper.nothing_within(Duration::from_millis(100)));
        // A valid packet longer than 20 ms is split into frames.
        let data = tone(mono, 50);
        rig.backend.on_audio_data(&format, &data);
        let mut received = Vec::new();
        for (sequence, length) in [(0, 1920), (1, 1920), (2, 960)] {
            let (header, payload) = helper.record();
            assert_eq!((header.kind, header.sequence), (wire::AUDIO, sequence));
            assert_eq!((header.rate, header.channels), (48_000, 1));
            assert_eq!(header.length, length);
            assert_eq!(header.received_ns, rig.clock.now_ns());
            received.extend(payload);
        }
        assert_eq!(received, data);
        assert_eq!(rig.stats.get(Counter::MicPackets), 2);
        assert_eq!(rig.stats.get(Counter::MicBytes), 3840 + data.len() as u64);
        // A packet longer than the 100 ms budget keeps only its newest
        // frames, and the skipped ones leave a sequence gap.
        let long = tone(mono, 300);
        rig.backend.on_audio_data(&format, &long);
        let mut sequences = Vec::new();
        let mut tail = Vec::new();
        for _ in 0..5 {
            let (header, payload) = helper.record();
            sequences.push(header.sequence);
            tail.extend(payload);
        }
        assert_eq!(sequences, [13, 14, 15, 16, 17]);
        assert_eq!(tail, long[long.len() - 5 * 1920..]);
        assert_eq!(rig.stats.get(Counter::MicDropped), 10);
        assert!(helper.nothing_within(Duration::from_millis(50)));
    }

    #[test]
    fn queue_is_bounded_and_drops_oldest_under_stalled_writer() {
        let mono = PcmFormat::new(48_000, 1);
        let mut queue = Queue::default();
        let frame = |sequence, at| Frame {
            format: mono,
            sequence,
            received_ns: at,
            bytes: vec![0; mono.frame_bytes()],
        };
        let mut dropped = 0;
        for sequence in 0..12 {
            dropped += queue.push(frame(sequence, 0));
            assert!(queue.duration_ns <= QUEUE_BUDGET_NS);
        }
        // 100 ms holds five 20 ms frames; the oldest seven went first.
        assert_eq!(dropped, 7);
        let sequences: Vec<_> = queue.frames.iter().map(|f| f.sequence).collect();
        assert_eq!(sequences, [7, 8, 9, 10, 11]);
        assert_eq!(queue.duration_ns, QUEUE_BUDGET_NS);
    }

    #[test]
    fn frames_older_than_budget_are_not_sent() {
        let mono = PcmFormat::new(48_000, 1);
        let mut queue = Queue::default();
        for (sequence, at) in [(0, 0), (1, 10_000_000), (2, 30_000_000)] {
            queue.push(Frame {
                format: mono,
                sequence,
                received_ns: at,
                bytes: vec![0; 1920],
            });
        }
        let (frame, dropped) = queue.pop_fresh(QUEUE_BUDGET_NS + 20_000_000);
        assert_eq!((frame.unwrap().sequence, dropped), (2, 2));
        assert_eq!(queue.duration_ns, 0);
        assert_eq!(queue.pop_fresh(0).1, 0);

        // End to end: audio that waited too long behind a stalled helper is
        // discarded before its first byte; fresh audio is sent.
        let (rig, mut helper) = Rig::recording();
        rig.clock.advance(Duration::from_secs(1));
        let now = rig.clock.now_ns();
        {
            let mut state = rig.microphone.shared.lock();
            for (sequence, age_ms) in [(10, 150), (11, 101), (12, 10)] {
                state.queue.push(Frame {
                    format: mono,
                    sequence,
                    received_ns: now - age_ms * 1_000_000,
                    bytes: vec![0; 1920],
                });
            }
            Shared::wake(&state);
        }
        let (header, _) = helper.record();
        assert_eq!(header.sequence, 12, "expired frames were dropped");
        assert_eq!(header.received_ns, now - 10_000_000);
        assert_eq!(rig.stats.get(Counter::MicDropped), 2);
        assert!(helper.nothing_within(Duration::from_millis(50)));
    }

    #[test]
    fn on_audio_data_never_blocks_while_writer_holds_socket() {
        let (mut rig, mut helper) = Rig::recording();
        let mono = PcmFormat::new(48_000, 1);
        let format = mono.audio_format();
        let packet = tone(mono, 20);
        // The helper never reads: the socket fills, the worker stalls, and
        // the server's callback still returns at once every time.
        let started = Instant::now();
        for _ in 0..2_000 {
            rig.backend.on_audio_data(&format, &packet);
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(50));
        let queued = rig.microphone.shared.lock().queue.duration_ns;
        assert!(queued <= QUEUE_BUDGET_NS);
        assert!(rig.stats.get(Counter::MicDropped) > 1_000);
        assert_eq!(rig.stats.get(Counter::MicPackets), 2_000);
        // Draining the helper side delivers aligned whole frames again.
        let (header, payload) = helper.record();
        assert_eq!(header.kind, wire::AUDIO);
        assert_eq!(payload.len(), 1920);
        assert!(rig.events().is_empty(), "drops are never server events");
    }

    #[test]
    fn idle_requests_recycle_and_in_use_reopens() {
        let (mut rig, mut helper) = Rig::recording();
        let mono = PcmFormat::new(48_000, 1);
        assert_eq!(rig.stats.get(Counter::MicOpens), 1);
        // IDLE arrives after the helper's 5 s debounce: stop at once.
        helper.send(wire::HelperMessage::Idle);
        assert!(matches!(rig.wait_event(), ServerEvent::RdpeaiRecycle));
        // Audio racing the close is dropped, not queued.
        rig.backend
            .on_audio_data(&mono.audio_format(), &tone(mono, 20));
        assert!(helper.nothing_within(Duration::from_millis(100)));
        // The server closes the channel and offers a replacement backend.
        rig.backend.on_close();
        rig.backend = rig.microphone.factory().build_backend();
        rig.negotiate(&OFFER);
        assert!(rig.events().is_empty(), "no recorder: stays closed");
        assert!(
            helper.nothing_within(Duration::from_millis(100)),
            "same format: no new announcement"
        );
        helper.send(wire::HelperMessage::InUse);
        assert_open(rig.wait_event(), 0, mono, true);
        rig.backend.on_open_reply(0);
        assert_eq!(rig.stats.get(Counter::MicOpens), 2);
        rig.backend
            .on_audio_data(&mono.audio_format(), &tone(mono, 20));
        let (header, _) = helper.record();
        assert_eq!(header.kind, wire::AUDIO);
        // IDLE while the Open is still outstanding also stops it.
        let mut control = Control::default();
        control.attach(0);
        control.set_in_use(true, 0);
        control.negotiated(1, &[mono.audio_format()], 0);
        assert_eq!(control.set_in_use(false, 0), [Command::Recycle]);
        // A late success reply does not request a second recycle.
        assert_eq!(control.open_reply(1, 0, 0).0, []);
    }

    #[test]
    fn unsolicited_open_reply_from_stale_event_is_closed() {
        let mono = PcmFormat::new(48_000, 1);
        let mut control = Control::default();
        control.negotiated(1, &[mono.audio_format()], 0);
        // No helper, no recorder: this connection never sent Open.
        let (commands, reply) = control.open_reply(1, 0, 0);
        assert_eq!(
            (commands, reply),
            (vec![Command::Recycle], Reply::Unsolicited)
        );
        assert_eq!(control.admit(1, mono), Admission::Drop);
        // A reply from a backend of an earlier channel is ignored entirely.
        let mut control = Control::default();
        control.negotiated(2, &[mono.audio_format()], 0);
        assert_eq!(control.open_reply(1, 0, 0), (Vec::new(), Reply::Stale));
        assert_eq!(control.admit(1, mono), Admission::Drop);
        control.closed(1);
        assert_eq!(
            control.channel,
            Channel::Ready,
            "stale close changes nothing"
        );

        let mut rig = Rig::new();
        rig.negotiate(&OFFER);
        rig.backend.on_open_reply(0);
        assert!(matches!(rig.events().remove(0), ServerEvent::RdpeaiRecycle));
        assert_eq!(rig.stats.get(Counter::MicOpens), 0);
    }

    #[test]
    fn failed_open_reply_backs_off_until_next_use_edge() {
        let mono = PcmFormat::new(48_000, 1);
        let mut control = Control::default();
        control.attach(0);
        control.set_in_use(true, 0);
        let mut commands = control.negotiated(1, &[mono.audio_format()], 0);
        // Both capture formats fail (for example, Windows privacy settings).
        assert_eq!(commands.len(), 1);
        commands = control.open_reply(1, -1, 0).0;
        assert_eq!(commands.len(), 1);
        let (commands, reply) = control.open_reply(1, -1, 1_000);
        assert!(commands.is_empty());
        assert_eq!(reply, Reply::Exhausted(-1));
        // Still in use: no retry, even long after.
        assert!(control.evaluate(RETRY_AFTER_NS * 10).is_empty());
        // A new recorder within 30 s does not retry, nor does waiting after it.
        control.set_in_use(false, 2_000);
        assert!(control.set_in_use(true, 3_000).is_empty());
        assert!(control.evaluate(1_000 + RETRY_AFTER_NS - 1).is_empty());
        // At 30 s the pending recorder start retries the whole ladder once.
        let commands = control.evaluate(1_000 + RETRY_AFTER_NS);
        assert!(matches!(
            commands[..],
            [Command::Open(Attempt {
                extensible: true,
                ..
            })]
        ));
        // A new connection starts without the backoff.
        let mut control = Control::default();
        control.attach(0);
        control.set_in_use(true, 0);
        control.negotiated(1, &[mono.audio_format()], 0);
        control.open_reply(1, -1, 0);
        control.open_reply(1, -1, 0);
        assert!(control.evaluate(1).is_empty());
        control.detach(1);
        control.attach(1);
        assert_eq!(control.set_in_use(true, 1).len(), 1);
    }

    #[test]
    fn unanswered_open_times_out_like_a_failed_open() {
        let mono = PcmFormat::new(48_000, 1);
        let mut control = Control::default();
        control.attach(0);
        control.set_in_use(true, 0);
        let commands = control.negotiated(1, &[mono.audio_format()], 0);
        assert!(matches!(commands[..], [Command::Open(_)]));
        // A slow device open is not a failure.
        assert!(control.evaluate(OPEN_TIMEOUT_NS - 1).is_empty());
        assert_eq!(control.evaluate(OPEN_TIMEOUT_NS), [Command::OpenTimeout]);
        assert!(
            control.evaluate(OPEN_TIMEOUT_NS + 1).is_empty(),
            "only once"
        );
        // The recycle closes the channel; a late reply reaches no live backend.
        control.closed(1);
        assert_eq!(control.open_reply(1, 0, OPEN_TIMEOUT_NS).1, Reply::Stale);
        // The replacement waits, as after a failed open: not for the same
        // recorder, and only for a new one 30 s after the timeout.
        let later = OPEN_TIMEOUT_NS + 1_000;
        assert!(
            control
                .negotiated(2, &[mono.audio_format()], later)
                .is_empty()
        );
        assert!(control.evaluate(later + RETRY_AFTER_NS * 2).is_empty());
        control.set_in_use(false, later + 1);
        assert!(control.set_in_use(true, later + 2).is_empty());
        let commands = control.evaluate(OPEN_TIMEOUT_NS + RETRY_AFTER_NS);
        assert!(matches!(commands[..], [Command::Open(_)]));
        // An answered Open never times out, nor does an idle channel.
        let mut control = Control::default();
        control.attach(0);
        control.set_in_use(true, 0);
        control.negotiated(1, &[mono.audio_format()], 0);
        assert!(matches!(control.open_reply(1, 0, 1).1, Reply::Opened(_)));
        assert!(control.evaluate(OPEN_TIMEOUT_NS * 3).is_empty());
        control.set_in_use(false, OPEN_TIMEOUT_NS * 3);
        assert!(control.evaluate(OPEN_TIMEOUT_NS * 6).is_empty());
    }

    #[test]
    fn worker_recycles_an_unanswered_open_and_counts_an_error() {
        let mut rig = Rig::new();
        let mono = PcmFormat::new(48_000, 1);
        rig.negotiate(&OFFER);
        let mut helper = rig.helper();
        assert_eq!(helper.record().0, available(mono));
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::InUse);
        assert_open(rig.wait_event(), 0, mono, true);
        // No reply. The worker re-evaluates at least once per tick.
        rig.clock.advance(Duration::from_nanos(OPEN_TIMEOUT_NS));
        assert!(matches!(rig.wait_event(), ServerEvent::RdpeaiRecycle));
        assert_eq!(rig.stats.get(Counter::MicErrors), 1);
        assert_eq!(rig.stats.get(Counter::MicOpens), 0);
        // Audio from the abandoned capture is never forwarded.
        rig.backend
            .on_audio_data(&mono.audio_format(), &tone(mono, 20));
        assert!(helper.nothing_within(Duration::from_millis(100)));
    }

    #[test]
    fn close_and_clear_stop_writer_promptly_without_flushing_stale_frames() {
        let (mut rig, mut helper) = Rig::recording();
        let mono = PcmFormat::new(48_000, 1);
        // Audio is queued and the connection ends before the worker runs
        // (one lock scope, as clear() does it).
        let started = Instant::now();
        {
            let shared = &rig.microphone.shared;
            let mut state = shared.lock();
            for sequence in 0..3 {
                state.queue.push(Frame {
                    format: mono,
                    sequence,
                    received_ns: rig.clock.now_ns(),
                    bytes: vec![0; 1920],
                });
            }
            Microphone::detach(shared, &mut state, rig.clock.now_ns());
        }
        // The worker exits and closes the socket; queued audio is not sent.
        let mut rest = Vec::new();
        helper.0.read_to_end(&mut rest).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(rest.is_empty(), "no audio after clear");
        assert!(rig.microphone.shared.lock().queue.is_empty());
        assert_eq!(rig.stats.get(Counter::MicDropped), 3);
        // Clearing while capture is open stops the client's capture too.
        assert!(matches!(rig.events().remove(0), ServerEvent::RdpeaiRecycle));
        // A channel close drops queued audio.
        let (rig, _helper) = Rig::recording();
        rig.microphone.shared.lock().queue.push(Frame {
            format: mono,
            sequence: 0,
            received_ns: rig.clock.now_ns(),
            bytes: vec![0; 1920],
        });
        let mut rig = rig;
        rig.backend.on_close();
        assert!(rig.microphone.shared.lock().queue.is_empty());
    }

    #[test]
    fn nrm1_roundtrip_and_malformed_helper_message_disables_only_microphone() {
        let header = wire::Header {
            kind: wire::AUDIO,
            channels: 2,
            rate: 44_100,
            sequence: u32::MAX,
            length: 3528,
            received_ns: 0x0102_0304_0506_0708,
        };
        let bytes = header.encode();
        assert_eq!(&bytes[..8], b"NRM1\x03\x02\0\0");
        assert_eq!(&bytes[8..12], &44_100u32.to_be_bytes());
        assert_eq!(&bytes[20..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(wire::Header::decode(&bytes), Ok(header));
        let mut bad = bytes;
        bad[6] = 1;
        assert!(wire::Header::decode(&bad).is_err());
        let mut odd = header;
        odd.length = 3;
        assert!(wire::Header::decode(&odd.encode()).is_err());
        for message in [
            wire::HelperMessage::Hello,
            wire::HelperMessage::InUse,
            wire::HelperMessage::Idle,
        ] {
            assert_eq!(wire::HelperMessage::decode(&message.encode()), Ok(message));
        }
        assert_eq!(&wire::HelperMessage::Idle.encode(), b"NRM1\x02\0\0\0");
        assert!(wire::HelperMessage::decode(b"NRM1\x01\0\0\x01").is_err());
        assert!(wire::HelperMessage::decode(b"NRM1\x09\0\0\0").is_err());
        assert!(wire::HelperMessage::decode(b"NRA1\x01\0\0\0").is_err());

        // A malformed helper record while recording: the helper connection
        // closes, capture stops, and nothing else is touched.
        let (mut rig, mut helper) = Rig::recording();
        helper.0.write_all(b"NRM1\x07\0\0\0").unwrap();
        assert!(helper.eof_within(Duration::from_secs(5)));
        assert!(matches!(rig.wait_event(), ServerEvent::RdpeaiRecycle));
        assert!(rig.events().is_empty(), "never a disconnect or other event");
        assert_eq!(rig.stats.get(Counter::MicErrors), 1);
        assert!(rig.microphone.shared.lock().link.is_none());
        // A second HELLO, or IN_USE before HELLO, is also a protocol error.
        for records in [
            [wire::HelperMessage::Hello, wire::HelperMessage::Hello],
            [wire::HelperMessage::InUse, wire::HelperMessage::Hello],
        ] {
            let rig = Rig::new();
            let mut helper = rig.helper();
            for record in records {
                helper.send(record);
            }
            assert!(helper.eof_within(Duration::from_secs(5)));
        }
        // An abrupt helper exit is the same: the microphone is off until the
        // next attach, which works again.
        let (mut rig, helper) = Rig::recording();
        drop(helper);
        assert!(matches!(rig.wait_event(), ServerEvent::RdpeaiRecycle));
        rig.backend.on_close();
        let mut helper = rig.helper();
        assert_eq!(helper.record().0, available(PcmFormat::new(48_000, 1)));
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::InUse);
        assert!(helper.nothing_within(Duration::from_millis(100)));
        // No channel until the replacement negotiates, then capture resumes.
        assert!(rig.events().is_empty());
        rig.backend = rig.microphone.factory().build_backend();
        rig.negotiate(&OFFER);
        assert_open(rig.events().remove(0), 0, PcmFormat::new(48_000, 1), true);
    }

    #[test]
    fn attach_none_and_missing_sender_are_harmless() {
        let microphone = Microphone::with_clock(Arc::new(ManualClock(AtomicU64::new(0))), stats());
        microphone.attach(None);
        microphone.clear();
        // No server sender yet (the backend before set_sender): decisions are dropped.
        let backend_factory = microphone.factory();
        let mut backend = backend_factory.build_backend();
        let (frontend, helper) = UnixStream::pair().unwrap();
        microphone.attach(Some(frontend));
        let mut helper = Helper(helper);
        helper
            .0
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(helper.record().0.kind, wire::HELLO);
        helper.send(wire::HelperMessage::Hello);
        helper.send(wire::HelperMessage::InUse);
        backend.on_formats_negotiated(&OFFER.map(PcmFormat::audio_format));
        microphone.clear();
        assert!(helper.eof_within(Duration::from_secs(5)));
    }
}
