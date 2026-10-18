//! Bounded performance settings shared by capture and encoding.
use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Keep interactive graphics within a short delivery budget. Feedback includes
/// network transit and client decoding, so this is not an audio/video clock.
///
/// The target moves between a few quantized levels. FFmpeg's NVENC rate
/// reconfiguration emits an IDR, so every level change costs a key frame:
/// decreases still react within a second, increases need sustained health.
pub(crate) struct AdaptiveBitrate {
    ceiling: u32,
    levels: Vec<u32>,
    level: usize,
    sent: HashMap<u32, Instant>,
    suspended: bool,
    last_change: Instant,
    healthy_since: Option<Instant>,
    baseline: Option<Duration>,
    baseline_candidate: Option<(Instant, Duration, Duration, u32)>,
    fps_ceiling: u32,
    window: u32,
    last_window_change: Instant,
    last_window_reduction: Option<Instant>,
    last_queue: u32,
    last_ack: Option<Instant>,
    last_send: Option<Instant>,
    joint: bool,
    diagnostics: Telemetry,
    last_audio_congestion: Option<Instant>,
}

impl Default for AdaptiveBitrate {
    fn default() -> Self {
        Self::new(12_000_000, Instant::now())
    }
}

impl AdaptiveBitrate {
    const BUDGET: Duration = Duration::from_millis(150);
    /// Minimum spacing between bitrate level decreases.
    const REDUCE_INTERVAL: Duration = Duration::from_secs(1);
    /// The credit window keeps its established reduction pace.
    const WINDOW_REDUCE_INTERVAL: Duration = Duration::from_millis(500);
    const RECOVER_INTERVAL: Duration = Duration::from_secs(3);
    /// Healthy acknowledgements required before raising the bitrate a level.
    const INCREASE_HOLD: Duration = Duration::from_secs(10);
    const LEVELS: u32 = 6;
    const FLOOR: u32 = 1_000_000;

    pub(crate) fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            ceiling,
            levels: Self::levels(ceiling),
            level: 0,
            sent: HashMap::new(),
            suspended: false,
            last_change: now,
            healthy_since: None,
            baseline: None,
            baseline_candidate: None,
            fps_ceiling: 60,
            window: 2,
            last_window_change: now,
            last_window_reduction: None,
            last_queue: 0,
            last_ack: None,
            last_send: None,
            joint: false,
            diagnostics: Telemetry::from_env(),
            last_audio_congestion: None,
        }
    }
    /// Geometric steps from the ceiling to the 1 Mbps floor, rounded to
    /// 100 kbps. Equal neighbours collapse, so a low ceiling has fewer levels.
    fn levels(ceiling: u32) -> Vec<u32> {
        let floor = Self::FLOOR.min(ceiling);
        let ratio = (f64::from(floor) / f64::from(ceiling)).powf(1.0 / f64::from(Self::LEVELS - 1));
        let mut levels: Vec<u32> = (0..Self::LEVELS)
            .map(|level| match level {
                0 => ceiling,
                level if level == Self::LEVELS - 1 => floor,
                level => {
                    let value = f64::from(ceiling) * ratio.powi(level as i32);
                    ((value / 100_000.0).round() as u32 * 100_000).clamp(floor, ceiling)
                }
            })
            .collect();
        levels.dedup();
        levels
    }
    /// The highest level not above `bitrate`, or the floor.
    fn level_at_most(&self, bitrate: u32) -> usize {
        self.levels
            .iter()
            .position(|&level| level <= bitrate)
            .unwrap_or(self.levels.len() - 1)
    }
    pub(crate) fn ceiling(&self) -> u32 {
        self.ceiling
    }
    pub(crate) fn current(&self) -> u32 {
        self.levels[self.level]
    }
    pub(crate) fn set_frame_rate_ceiling(&mut self, fps: u32) {
        self.fps_ceiling = fps.clamp(1, 60);
    }
    pub(crate) fn enable_joint_adaptation(&mut self, enabled: bool) {
        self.joint = enabled;
    }
    /// Scale cadence with available graphics capacity, keeping useful per-frame
    /// quality instead of spending a reduced rate on the original 60 frames.
    pub(crate) fn frame_rate(&self) -> u32 {
        if !self.joint {
            return self.fps_ceiling;
        }
        (u64::from(self.fps_ceiling) * u64::from(self.current()) / u64::from(self.ceiling)).clamp(
            u64::from(self.fps_ceiling.min(10)),
            u64::from(self.fps_ceiling),
        ) as u32
    }
    pub(crate) fn max_frames_in_flight(&self) -> u32 {
        self.window
    }
    pub(crate) fn permits_refinement(&self, now: Instant) -> bool {
        self.last_queue == 0
            && self.sent.is_empty()
            && self.current() >= self.ceiling / 2
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
            && (self.suspended
                || self
                    .last_ack
                    .is_some_and(|at| now.saturating_duration_since(at) < Self::RECOVER_INTERVAL))
    }
    pub(crate) fn reset_frames(&mut self) {
        self.sent.clear();
        self.suspended = false;
        self.healthy_since = None;
        self.baseline = None;
        self.baseline_candidate = None;
        self.last_ack = None;
        self.last_queue = 0;
        self.window = 2;
        self.last_window_reduction = None;
        self.last_send = None;
    }
    pub(crate) fn audio_congested(&mut self, now: Instant) {
        self.last_audio_congestion = Some(
            self.last_audio_congestion
                .map_or(now, |previous| previous.max(now)),
        );
        // Sound can be queued even when graphics ACKs are healthy or suspended.
        // Use the same reduction cooldown, but keep recovery tied to both streams.
        self.congested(now);
    }
    pub(crate) fn clear_audio_congestion(&mut self) {
        self.last_audio_congestion = None;
        self.healthy_since = None;
    }
    pub(crate) fn inherit_audio_congestion(&mut self, previous: &Self, now: Instant) {
        if let Some(at) = previous.last_audio_congestion
            && now.saturating_duration_since(at) < Self::RECOVER_INTERVAL
        {
            // Sound negotiation can precede creation of the graphics context.
            // Preserve its pressure and target without carrying old frame IDs.
            self.last_audio_congestion = Some(at);
            self.level = self.level.max(self.level_at_most(previous.current()));
            self.last_change = previous.last_change.min(now);
        }
    }
    fn delivery_budget(&self) -> Duration {
        // Transit time alone is not congestion. Detect delay above the fastest
        // observed ACK, while still reacting if a new connection never ACKs.
        self.baseline
            .map_or(Duration::from_millis(500), |baseline| {
                (baseline + Duration::from_millis(100)).max(Self::BUDGET)
            })
    }
    pub(crate) fn sent(&mut self, id: u32, now: Instant) {
        if let Some(previous) = self.last_send {
            self.diagnostics
                .sample("send_cadence", now.saturating_duration_since(previous));
        }
        self.last_send = Some(now);
        self.diagnostics.count("sent_frames", 1);
        self.diagnostics
            .count("window_samples", u64::from(self.window));
        connection().record_window(self.window);
        if !self.suspended {
            // Track the same bounded window configured in the protocol server.
            if self.sent.len() >= self.window as usize {
                let oldest = *self.sent.iter().min_by_key(|(_, at)| **at).unwrap().0;
                self.sent.remove(&oldest);
            }
            self.sent.insert(id, now);
        }
    }
    fn congested(&mut self, now: Instant) {
        self.healthy_since = None;
        if self.window > 2
            && now.saturating_duration_since(self.last_window_change)
                >= Self::WINDOW_REDUCE_INTERVAL
        {
            self.window -= 1;
            self.last_window_change = now;
            self.last_window_reduction = Some(now);
        }
        if now.saturating_duration_since(self.last_change) >= Self::REDUCE_INTERVAL
            && self.level + 1 < self.levels.len()
        {
            self.level += 1;
            self.last_change = now;
            self.diagnostics.count("bitrate_level_decreases", 1);
        }
    }
    pub(crate) fn check_stall(&mut self, now: Instant) {
        if self
            .sent
            .values()
            .any(|at| now.duration_since(*at) > self.delivery_budget())
        {
            self.congested(now);
        }
    }
    pub(crate) fn acknowledged(&mut self, id: u32, queue_depth: u32, now: Instant) {
        // MS-RDPEGFX's suspension sentinel is a protocol mode, not a huge queue.
        if queue_depth == u32::MAX {
            self.sent.clear();
            self.suspended = true;
            self.healthy_since = None;
            self.baseline_candidate = None;
            self.last_queue = 0;
            self.last_ack = Some(now);
            return;
        }
        self.suspended = false;
        let Some(sent) = self.sent.remove(&id) else {
            return;
        };
        let elapsed = now.saturating_duration_since(sent);
        self.diagnostics.sample("graphics_send_ack", elapsed);
        self.diagnostics.count("ack_frames", 1);
        self.diagnostics
            .count("queued_graphics_bytes", u64::from(queue_depth));
        self.diagnostics
            .count("outstanding_frames", self.sent.len() as u64);
        self.diagnostics.report("graphics_feedback", now);
        self.last_ack = Some(now);
        self.last_queue = queue_depth;
        // A lifetime minimum mistakes a lasting transit change for congestion.
        // Relearn only from a narrow, empty-queue delay plateau lasting >=3s
        // and >=8 valid ACKs. Growing decoder/network queues, jitter and audio
        // pressure never raise the baseline. A quicker path relearns immediately.
        if queue_depth == 0
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            if self
                .baseline
                .is_some_and(|baseline| elapsed > baseline + Duration::from_millis(100))
            {
                let (since, low, high, samples) = self
                    .baseline_candidate
                    .get_or_insert((now, elapsed, elapsed, 0));
                *low = (*low).min(elapsed);
                *high = (*high).max(elapsed);
                *samples += 1;
                if *high - *low > (*low / 8).max(Duration::from_millis(10)) {
                    self.baseline_candidate = Some((now, elapsed, elapsed, 1));
                } else if *samples >= 8
                    && now.saturating_duration_since(*since) >= Self::RECOVER_INTERVAL
                {
                    self.baseline = Some(*low);
                    self.baseline_candidate = None;
                }
            } else {
                self.baseline_candidate = None;
            }
        } else {
            self.baseline_candidate = None;
        }
        self.baseline = Some(
            self.baseline
                .map_or(elapsed, |baseline| baseline.min(elapsed)),
        );
        let budget = self.delivery_budget();
        // QueueDepth is queued graphics bytes, not a count of frames. Permit
        // at most 150ms of payload at the current target rate.
        let queue_budget = u64::from(self.current()) * Self::BUDGET.as_millis() as u64 / 8_000;
        if u64::from(queue_depth) > queue_budget || elapsed > budget {
            self.congested(now);
            return;
        }
        let healthy = *self.healthy_since.get_or_insert(now);
        // Allow enough credits for the measured round trip at the target
        // cadence, but never more than eight. Two credits cap delivery at two
        // frames per ACK delay, about 20fps on a 100ms WAN path to mstsc. The
        // measured send rate is itself window-limited, so size from the target
        // FPS. Grow at once on a healthy link; after congestion shrank the
        // window, wait for a stable interval. No capture queue is introduced.
        let desired = (elapsed.as_micros() * u128::from(self.frame_rate()) / 1_000_000 + 2)
            .clamp(2, 8) as u32;
        if desired > self.window
            && self.last_window_reduction.is_none_or(|at| {
                now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL
                    && now.saturating_duration_since(healthy) >= Self::RECOVER_INTERVAL
            })
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            self.window = desired;
            self.last_window_change = now;
        }
        if self.level > 0
            && now.saturating_duration_since(healthy) >= Self::INCREASE_HOLD
            && now.saturating_duration_since(self.last_change) >= Self::INCREASE_HOLD
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            self.level -= 1;
            self.last_change = now;
            self.diagnostics.count("bitrate_level_increases", 1);
        }
    }
}
/// Applies controller targets to the live encoder. Each NVENC rate change
/// costs an IDR: decreases apply at once, while an increase waits for an IDR
/// that is due anyway (refresh, resume, dropped frame, resize, cadence change)
/// or until `defer` has passed since the last IDR, when it replaces one.
pub(crate) struct RateApplier {
    applied: Option<(u32, u32)>,
    last_idr: Option<Instant>,
    defer: Duration,
}
impl RateApplier {
    pub(crate) fn new(defer: Duration) -> Self {
        Self {
            applied: None,
            last_idr: None,
            defer,
        }
    }
    /// The bitrate to configure for the next encoded frame at `fps`.
    pub(crate) fn select(&mut self, target: u32, fps: u32, idr_due: bool, now: Instant) -> u32 {
        let bitrate = match self.applied {
            Some((applied, applied_fps))
                if target > applied
                    && applied_fps == fps
                    && !idr_due
                    && self
                        .last_idr
                        .is_some_and(|at| now.saturating_duration_since(at) < self.defer) =>
            {
                applied
            }
            _ => target,
        };
        self.applied = Some((bitrate, fps));
        bitrate
    }
    pub(crate) fn idr_sent(&mut self, now: Instant) {
        self.last_idr = Some(now);
    }
}

fn setting(name: &str, value: Option<&str>, default: u32, maximum: u32) -> anyhow::Result<u32> {
    let result = match value {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("{name} must be an integer within 1..{maximum}"))?,
        None => default,
    };
    anyhow::ensure!(
        (1..=maximum).contains(&result),
        "{name} must be an integer within 1..{maximum}"
    );
    Ok(result)
}

fn environment(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("{name} must contain valid Unicode"),
    }
}

/// The headless output refreshes at 60 Hz. Slower links can request fewer frames.
pub fn frame_rate() -> anyhow::Result<u32> {
    let default = if std::env::var("NIRI_RDP_ENCODER").as_deref() == Ok("nvenc") {
        60
    } else {
        30
    };
    setting(
        "NIRI_RDP_FPS",
        environment("NIRI_RDP_FPS")?.as_deref(),
        default,
        60,
    )
}

/// Adaptive H.264 bitrate ceiling in Mbps, bounded before crossing the ABI.
pub fn bitrate() -> anyhow::Result<u32> {
    let ceiling = setting(
        "NIRI_RDP_BITRATE_MBPS",
        environment("NIRI_RDP_BITRATE_MBPS")?.as_deref(),
        12,
        100,
    )? * 1_000_000;
    let link = environment("NIRI_RDP_LINK_MBPS")?;
    let link = link
        .as_deref()
        .map(|value| setting("NIRI_RDP_LINK_MBPS", Some(value), 100, 100))
        .transpose()?;
    effective_bitrate(ceiling, link)
}

fn effective_bitrate(ceiling: u32, link: Option<u32>) -> anyhow::Result<u32> {
    // 48kHz stereo S16LE payload is 1.536Mbps. Reserve 1.7Mbps for the
    // accepted PCM path plus framing instead of counting it as video capacity.
    if let Some(link) = link {
        anyhow::ensure!(
            (3..=100).contains(&link),
            "NIRI_RDP_LINK_MBPS must be within 3..100 to reserve PCM audio capacity"
        );
        Ok(ceiling.min(link * 1_000_000 - 1_700_000))
    } else {
        Ok(ceiling)
    }
}

/// Defaults retain the measured release profile; additional profiles are opt-in
/// until matched text/scroll/video measurements justify promoting a new default.
pub(crate) fn quality_profile() -> anyhow::Result<(u32, bool)> {
    match environment("NIRI_RDP_VIDEO_PROFILE")?
        .as_deref()
        .unwrap_or("speed")
    {
        "speed" => Ok((1, false)),
        "balanced" => Ok((4, true)),
        "quality" => Ok((5, true)),
        _ => anyhow::bail!("NIRI_RDP_VIDEO_PROFILE must be speed, balanced or quality"),
    }
}
pub(crate) fn avc444_requested() -> anyhow::Result<bool> {
    match environment("NIRI_RDP_VIDEO_CODEC")?
        .as_deref()
        .unwrap_or("avc420")
    {
        "avc420" => Ok(false),
        "auto" | "avc444" => Ok(true),
        _ => anyhow::bail!("NIRI_RDP_VIDEO_CODEC must be avc420, auto or avc444"),
    }
}
/// NVENC defaults to `auto`: DMA-BUF import is used only when capture, GPU and
/// encoder support it, and SHM remains the fallback. Bitmap mode never encodes.
pub(crate) fn gpu_capture_requested() -> anyhow::Result<bool> {
    gpu_capture_setting(
        environment("NIRI_RDP_GPU_CAPTURE")?.as_deref(),
        std::env::var("NIRI_RDP_ENCODER").as_deref() == Ok("nvenc"),
    )
}
fn gpu_capture_setting(value: Option<&str>, nvenc: bool) -> anyhow::Result<bool> {
    match value.unwrap_or(if nvenc { "auto" } else { "shm" }) {
        "auto" => Ok(true),
        "shm" => Ok(false),
        _ => anyhow::bail!("NIRI_RDP_GPU_CAPTURE must be auto or shm"),
    }
}
/// Seconds between periodic IDRs. Every recovery point already forces one,
/// so this is only a safety net over reliable RDPGFX transport.
pub(crate) const DEFAULT_GOP_SECONDS: u32 = 30;
pub(crate) fn gop_seconds() -> anyhow::Result<u32> {
    setting(
        "NIRI_RDP_GOP_SECONDS",
        environment("NIRI_RDP_GOP_SECONDS")?.as_deref(),
        DEFAULT_GOP_SECONDS,
        3600,
    )
}
pub(crate) fn joint_adaptation_requested() -> anyhow::Result<bool> {
    match environment("NIRI_RDP_ADAPTIVE_GRAPHICS")?
        .as_deref()
        .unwrap_or("0")
    {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => anyhow::bail!("NIRI_RDP_ADAPTIVE_GRAPHICS must be 0 or 1"),
    }
}

/// Encoder settings, parsed and validated once at startup instead of per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoConfig {
    pub(crate) fps: u32,
    pub(crate) bitrate: u32,
    pub(crate) preset: u32,
    pub(crate) spatial_aq: bool,
    pub(crate) avc444: bool,
    pub(crate) gpu_capture: bool,
    pub(crate) joint_adaptation: bool,
    pub(crate) gop_seconds: u32,
}
impl Default for VideoConfig {
    /// The NVENC defaults with no environment overrides.
    fn default() -> Self {
        Self {
            fps: 60,
            bitrate: 12_000_000,
            preset: 1,
            spatial_aq: false,
            avc444: false,
            gpu_capture: true,
            joint_adaptation: false,
            gop_seconds: DEFAULT_GOP_SECONDS,
        }
    }
}
impl VideoConfig {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let (preset, spatial_aq) = quality_profile()?;
        Ok(Self {
            fps: frame_rate()?,
            bitrate: bitrate()?,
            preset,
            spatial_aq,
            avc444: avc444_requested()?,
            gpu_capture: gpu_capture_requested()?,
            joint_adaptation: joint_adaptation_requested()?,
            gop_seconds: gop_seconds()?,
        })
    }
    /// How long a pending bitrate increase may wait for an IDR due anyway.
    pub(crate) fn rate_increase_defer(&self) -> Duration {
        Duration::from_secs(u64::from(self.gop_seconds.min(30)))
    }
}

/// Always-on per-connection counters for the one disconnect summary line.
/// Counts and sizes only: never desktop pixels, input, clipboard content,
/// credentials or user names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    VideoFrames,
    VideoIdrFrames,
    VideoBytes,
    AudioWaves,
    AudioDiscardedBlocks,
    AudioCreditWaits,
    AudioCodecRecoveries,
    /// Audible 20 ms PCM blocks dropped by the periodic clock-drift catch-up.
    AudioDriftSkips,
    ClipboardToClient,
    ClipboardFromClient,
    ClipboardTimeouts,
    ClipboardOversize,
    /// Valid AUDIO_INPUT Data PDUs and their payload bytes.
    MicPackets,
    MicBytes,
    /// 20 ms microphone frames discarded in the frontend: queue overflow,
    /// expiry, or audio arriving while nothing records.
    MicDropped,
    /// Successful AUDIO_INPUT Opens.
    MicOpens,
    /// Rejected packets, failed Opens and helper connection failures.
    MicErrors,
}
impl Counter {
    const COUNT: usize = 17;
    /// Diagnostics counters that also feed the summary, so the opt-in
    /// telemetry and the always-on line share one increment site.
    fn from_telemetry(name: &str) -> Option<Self> {
        Some(match name {
            "delivered_frames" => Self::VideoFrames,
            "idr_frames" => Self::VideoIdrFrames,
            "h264_payload_bytes" => Self::VideoBytes,
            "waves_sent" => Self::AudioWaves,
            "discarded_blocks" => Self::AudioDiscardedBlocks,
            "credit_waits" => Self::AudioCreditWaits,
            "codec_recoveries" => Self::AudioCodecRecoveries,
            "drift_skips" => Self::AudioDriftSkips,
            _ => return None,
        })
    }
}

/// The negotiated RDPSND codec, if sound started on this connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioCodec {
    #[default]
    None,
    Pcm,
    Aac,
    Adpcm,
}
impl AudioCodec {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pcm => "pcm",
            Self::Aac => "aac",
            Self::Adpcm => "adpcm",
        }
    }
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Pcm,
            2 => Self::Aac,
            3 => Self::Adpcm,
            _ => Self::None,
        }
    }
}

/// Lock-free counters for the connection being served. The frontend serves
/// one connection at a time; [`ConnectionStats::take`] at disconnect starts
/// the next connection from zero.
pub struct ConnectionStats {
    counters: [AtomicU64; Counter::COUNT],
    audio_codec: AtomicU8,
    /// Last opened microphone format: rate << 8 | channels, 0 for none.
    microphone: AtomicU32,
    /// Client core Frame Acknowledge capability plus one, 0 when absent.
    client_frame_ack: AtomicU32,
    /// Graphics credit window: largest, and sum/count over sent frames.
    window_max: AtomicU32,
    window_sum: AtomicU64,
    window_sends: AtomicU64,
}
impl Default for ConnectionStats {
    fn default() -> Self {
        Self::new()
    }
}
impl ConnectionStats {
    pub const fn new() -> Self {
        Self {
            counters: [const { AtomicU64::new(0) }; Counter::COUNT],
            audio_codec: AtomicU8::new(0),
            microphone: AtomicU32::new(0),
            client_frame_ack: AtomicU32::new(0),
            window_max: AtomicU32::new(0),
            window_sum: AtomicU64::new(0),
            window_sends: AtomicU64::new(0),
        }
    }
    pub fn add(&self, counter: Counter, value: u64) {
        // u64 counts and bytes cannot wrap within a connection's lifetime.
        self.counters[counter as usize].fetch_add(value, Ordering::Relaxed);
    }
    pub fn get(&self, counter: Counter) -> u64 {
        self.counters[counter as usize].load(Ordering::Relaxed)
    }
    pub fn set_audio_codec(&self, codec: AudioCodec) {
        self.audio_codec.store(codec as u8, Ordering::Relaxed);
    }
    /// Record the PCM format a microphone capture opened with.
    pub fn set_microphone_format(&self, rate: u32, channels: u8) {
        self.microphone
            .store(rate << 8 | u32::from(channels), Ordering::Relaxed);
    }
    /// Record the client's advertised core Frame Acknowledge capability
    /// (MS-RDPRFX maxUnacknowledgedFrameCount). It does not cap RDPGFX.
    pub fn set_client_frame_ack(&self, advertised: Option<u32>) {
        self.client_frame_ack.store(
            advertised.map_or(0, |count| count.saturating_add(1)),
            Ordering::Relaxed,
        );
    }
    /// Record the graphics credit window in force for one sent frame.
    pub fn record_window(&self, window: u32) {
        self.window_max.fetch_max(window, Ordering::Relaxed);
        self.window_sum
            .fetch_add(u64::from(window), Ordering::Relaxed);
        self.window_sends.fetch_add(1, Ordering::Relaxed);
    }
    /// Read and reset every counter for the next connection.
    pub fn take(&self) -> ConnectionSummary {
        ConnectionSummary {
            counters: std::array::from_fn(|i| self.counters[i].swap(0, Ordering::Relaxed)),
            audio_codec: AudioCodec::from_u8(self.audio_codec.swap(0, Ordering::Relaxed)),
            microphone: self.microphone.swap(0, Ordering::Relaxed),
            client_frame_ack: self.client_frame_ack.swap(0, Ordering::Relaxed),
            window_max: self.window_max.swap(0, Ordering::Relaxed),
            window_sum: self.window_sum.swap(0, Ordering::Relaxed),
            window_sends: self.window_sends.swap(0, Ordering::Relaxed),
        }
    }
}

static CONNECTION: ConnectionStats = ConnectionStats::new();
/// The process-wide counters of the connection currently being served.
pub fn connection() -> &'static ConnectionStats {
    &CONNECTION
}

/// A snapshot of one connection's counters, formatted at disconnect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnectionSummary {
    counters: [u64; Counter::COUNT],
    audio_codec: AudioCodec,
    microphone: u32,
    client_frame_ack: u32,
    window_max: u32,
    window_sum: u64,
    window_sends: u64,
}
impl ConnectionSummary {
    pub fn get(&self, counter: Counter) -> u64 {
        self.counters[counter as usize]
    }
    /// One payload-free line. `avc444` reports whether the graphics context
    /// negotiated AVC444 with it enabled; H.264 is reported only when frames
    /// were delivered, otherwise the connection used bitmap updates.
    pub fn line(
        &self,
        peer: std::net::SocketAddr,
        duration: Duration,
        failed: bool,
        avc444: bool,
    ) -> String {
        let frames = self.get(Counter::VideoFrames);
        let (codec, mode) = match (frames, avc444) {
            (0, _) => ("bitmap", "none"),
            (_, false) => ("h264", "avc420"),
            (_, true) => ("h264", "avc444"),
        };
        let millis = duration.as_millis().max(1);
        let kbps = u128::from(self.get(Counter::VideoBytes)) * 8 / millis;
        format!(
            "RDP connection summary peer={peer} duration_s={:.1} result={} \
             video_codec={codec} video_mode={mode} frames={frames} idr_frames={} \
             video_avg_kbps={kbps} audio_codec={} audio_waves={} audio_discarded_blocks={} \
             audio_credit_waits={} audio_codec_recoveries={} audio_drift_skips={} \
             clipboard_to_client={} clipboard_from_client={} clipboard_timeouts={} clipboard_oversize_drops={} \
             mic={} mic_opens={} mic_packets={} mic_bytes={} mic_dropped={} mic_errors={} \
             client_frame_ack_limit={} window_max={} window_avg={:.1}",
            duration.as_secs_f64(),
            if failed { "error" } else { "ok" },
            self.get(Counter::VideoIdrFrames),
            self.audio_codec.name(),
            self.get(Counter::AudioWaves),
            self.get(Counter::AudioDiscardedBlocks),
            self.get(Counter::AudioCreditWaits),
            self.get(Counter::AudioCodecRecoveries),
            self.get(Counter::AudioDriftSkips),
            self.get(Counter::ClipboardToClient),
            self.get(Counter::ClipboardFromClient),
            self.get(Counter::ClipboardTimeouts),
            self.get(Counter::ClipboardOversize),
            self.microphone_name(),
            self.get(Counter::MicOpens),
            self.get(Counter::MicPackets),
            self.get(Counter::MicBytes),
            self.get(Counter::MicDropped),
            self.get(Counter::MicErrors),
            match self.client_frame_ack {
                0 => "none".into(),
                value => (value - 1).to_string(),
            },
            self.window_max,
            self.window_sum as f64 / self.window_sends.max(1) as f64,
        )
    }
    fn microphone_name(&self) -> String {
        match (self.microphone >> 8, self.microphone & 0xff) {
            (0, _) => "none".into(),
            (rate, 1) => format!("pcm_{rate}_mono"),
            (rate, _) => format!("pcm_{rate}_stereo"),
        }
    }
}

/// Whether the opt-in `NIRI_RDP_DIAGNOSTICS=1` receipts and detail lines are on.
pub fn diagnostics_enabled() -> bool {
    std::env::var("NIRI_RDP_DIAGNOSTICS").as_deref() == Ok("1")
}

/// Optional bounded server-side timing receipts. These are durations/counters,
/// never desktop pixels, input text, clipboard contents or credentials. ACKs
/// include transport and decoding and do not measure physical presentation.
/// A few counters also feed the always-on [`ConnectionStats`] summary.
#[derive(Default)]
pub struct Telemetry {
    enabled: bool,
    began: Option<Instant>,
    samples: HashMap<&'static str, VecDeque<u64>>,
    counters: HashMap<&'static str, u64>,
}
impl Telemetry {
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn from_env() -> Self {
        Self {
            enabled: diagnostics_enabled(),
            ..Self::default()
        }
    }
    pub fn sample(&mut self, stage: &'static str, duration: Duration) {
        if !self.enabled || (self.samples.len() >= 32 && !self.samples.contains_key(stage)) {
            return;
        }
        self.began.get_or_insert_with(Instant::now);
        let samples = self.samples.entry(stage).or_default();
        if samples.len() == 256 {
            samples.pop_front();
        }
        samples.push_back(duration.as_micros().min(u128::from(u64::MAX)) as u64);
    }
    pub fn count(&mut self, name: &'static str, value: u64) {
        if let Some(counter) = Counter::from_telemetry(name) {
            connection().add(counter, value);
        }
        if !self.enabled || (self.counters.len() >= 32 && !self.counters.contains_key(name)) {
            return;
        }
        self.began.get_or_insert_with(Instant::now);
        let counter = self.counters.entry(name).or_default();
        *counter = counter.saturating_add(value);
    }
    pub fn report(&mut self, component: &'static str, now: Instant) {
        let Some(began) = self.began else {
            return;
        };
        let elapsed = now.saturating_duration_since(began);
        if elapsed < Duration::from_secs(5) {
            return;
        }
        let mut timings = Vec::new();
        for (stage, samples) in &self.samples {
            let mut sorted: Vec<_> = samples.iter().copied().collect();
            sorted.sort_unstable();
            if sorted.is_empty() {
                continue;
            }
            timings.push(format!(
                "\"{stage}\":{{\"samples\":{},\"p50_us\":{},\"p95_us\":{},\"max_us\":{}}}",
                sorted.len(),
                sorted[(sorted.len() - 1) / 2],
                sorted[(sorted.len() - 1) * 95 / 100],
                sorted.last().unwrap()
            ));
        }
        let counters: Vec<_> = self
            .counters
            .iter()
            .map(|(name, value)| format!("\"{name}\":{value}"))
            .collect();
        eprintln!(
            "RDP performance {{\"component\":\"{component}\",\"interval_ms\":{},\"server_timings\":{{{}}},\"counters\":{{{}}},\"physical_presentation_measured\":false}}",
            elapsed.as_millis(),
            timings.join(","),
            counters.join(",")
        );
        self.samples.clear();
        self.counters.clear();
        self.began = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_summary_formats_every_counter_and_resets() {
        let stats = ConnectionStats::new();
        let counters = [
            (Counter::VideoFrames, 3600),
            (Counter::VideoIdrFrames, 4),
            (Counter::VideoBytes, 7_500_000),
            (Counter::AudioWaves, 3000),
            (Counter::AudioDiscardedBlocks, 5),
            (Counter::AudioCreditWaits, 6),
            (Counter::AudioCodecRecoveries, 7),
            (Counter::AudioDriftSkips, 17),
            (Counter::ClipboardToClient, 8),
            (Counter::ClipboardFromClient, 9),
            (Counter::ClipboardTimeouts, 10),
            (Counter::ClipboardOversize, 11),
            (Counter::MicPackets, 12),
            (Counter::MicBytes, 13),
            (Counter::MicDropped, 14),
            (Counter::MicOpens, 15),
            (Counter::MicErrors, 16),
        ];
        for (counter, value) in counters {
            stats.add(counter, value);
        }
        stats.set_audio_codec(AudioCodec::Pcm);
        stats.set_microphone_format(48_000, 1);
        stats.set_client_frame_ack(Some(2));
        for window in [2, 4, 8, 8] {
            stats.record_window(window);
        }
        let summary = stats.take();
        let peer = "192.0.2.10:50123".parse().unwrap();
        assert_eq!(
            summary.line(peer, Duration::from_secs(60), false, false),
            "RDP connection summary peer=192.0.2.10:50123 duration_s=60.0 result=ok \
             video_codec=h264 video_mode=avc420 frames=3600 idr_frames=4 video_avg_kbps=1000 \
             audio_codec=pcm audio_waves=3000 audio_discarded_blocks=5 audio_credit_waits=6 \
             audio_codec_recoveries=7 audio_drift_skips=17 clipboard_to_client=8 \
             clipboard_from_client=9 clipboard_timeouts=10 clipboard_oversize_drops=11 mic=pcm_48000_mono mic_opens=15 \
             mic_packets=12 mic_bytes=13 mic_dropped=14 mic_errors=16 \
             client_frame_ack_limit=2 window_max=8 window_avg=5.5"
        );
        assert!(
            summary
                .line(peer, Duration::from_secs(60), true, true)
                .contains(" result=error video_codec=h264 video_mode=avc444 ")
        );
        // Take starts the next connection from zero, including the codec.
        let next = stats.take();
        assert_eq!(next, ConnectionSummary::default());
        let line = next.line(peer, Duration::ZERO, false, true);
        assert!(line.contains(" video_codec=bitmap video_mode=none frames=0 "));
        assert!(line.contains(" video_avg_kbps=0 audio_codec=none "));
        assert!(
            line.contains(" audio_codec_recoveries=0 audio_drift_skips=0 clipboard_to_client=0 ")
        );
        assert!(line.ends_with(
            " mic=none mic_opens=0 mic_packets=0 mic_bytes=0 mic_dropped=0 mic_errors=0 \
             client_frame_ack_limit=none window_max=0 window_avg=0.0"
        ));
        stats.set_client_frame_ack(Some(0));
        assert!(
            stats
                .take()
                .line(peer, Duration::ZERO, false, false)
                .contains(" client_frame_ack_limit=0 ")
        );
        stats.set_microphone_format(44_100, 2);
        assert!(
            stats
                .take()
                .line(peer, Duration::ZERO, false, false)
                .contains(" mic=pcm_44100_stereo ")
        );
    }

    #[test]
    fn telemetry_feeds_summary_counters_even_when_diagnostics_are_off() {
        let mut telemetry = Telemetry::default();
        assert!(!telemetry.enabled());
        let names = [
            ("delivered_frames", Counter::VideoFrames),
            ("idr_frames", Counter::VideoIdrFrames),
            ("h264_payload_bytes", Counter::VideoBytes),
            ("waves_sent", Counter::AudioWaves),
            ("discarded_blocks", Counter::AudioDiscardedBlocks),
            ("credit_waits", Counter::AudioCreditWaits),
            ("codec_recoveries", Counter::AudioCodecRecoveries),
            ("drift_skips", Counter::AudioDriftSkips),
        ];
        // Other tests may count concurrently; this connection never resets.
        for (name, counter) in names {
            let before = connection().get(counter);
            telemetry.count(name, 2);
            assert!(connection().get(counter) >= before + 2, "{name}");
        }
        assert_eq!(Counter::from_telemetry("ack_frames"), None);
        assert!(telemetry.counters.is_empty(), "diagnostics stay off");
    }
    #[test]
    fn bitrate_reduces_on_delay_or_decoder_backlog_and_recovers_slowly() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(30));
        rate.sent(1, start + Duration::from_millis(1000));
        rate.acknowledged(1, 0, start + Duration::from_millis(1200));
        assert_eq!(rate.current(), 7_300_000, "delay drops one level");
        rate.sent(2, start + Duration::from_millis(1210));
        rate.acknowledged(2, 1_000_000, start + Duration::from_millis(1240));
        assert_eq!(rate.current(), 7_300_000, "reductions have a cooldown");
        rate.sent(3, start + Duration::from_millis(2300));
        rate.acknowledged(3, 1_000_000, start + Duration::from_millis(2330));
        assert_eq!(rate.current(), 4_400_000, "backlog drops one level");
        let healthy = |ids: std::ops::RangeInclusive<u32>, rate: &mut AdaptiveBitrate| {
            for id in ids {
                let at = start + Duration::from_millis(u64::from(id) * 100);
                rate.sent(id, at);
                rate.acknowledged(id, 0, at + Duration::from_millis(30));
            }
        };
        // Healthy since 2430 ms: no increase (and so no IDR) for 10 s.
        healthy(24..=123, &mut rate);
        assert_eq!(rate.current(), 4_400_000, "increases need 10 s of health");
        healthy(124..=124, &mut rate);
        assert_eq!(rate.current(), 7_300_000, "recovery rises one level");
        healthy(125..=223, &mut rate);
        assert_eq!(rate.current(), 7_300_000, "each increase holds 10 s");
        healthy(224..=300, &mut rate);
        assert_eq!(rate.current(), 12_000_000);
    }

    #[test]
    fn bitrate_levels_are_quantized_bounded_and_descending() {
        let levels = AdaptiveBitrate::levels(12_000_000);
        assert_eq!(
            levels,
            [
                12_000_000, 7_300_000, 4_400_000, 2_700_000, 1_600_000, 1_000_000
            ]
        );
        for ceiling in [1_000_000, 1_300_000, 3_300_000, 12_000_000, 100_000_000] {
            let levels = AdaptiveBitrate::levels(ceiling);
            assert_eq!(levels[0], ceiling);
            assert_eq!(*levels.last().unwrap(), 1_000_000);
            assert!(levels.len() <= 6);
            assert!(levels.windows(2).all(|pair| pair[0] > pair[1]));
        }
        assert_eq!(AdaptiveBitrate::levels(1_000_000), [1_000_000]);
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(1_000_000, start);
        rate.audio_congested(start + Duration::from_secs(2));
        assert_eq!(rate.current(), 1_000_000, "a single level cannot move");
        let rate = AdaptiveBitrate::new(12_000_000, start);
        assert_eq!(rate.level_at_most(7_300_000), 1);
        assert_eq!(rate.level_at_most(7_299_999), 2);
        assert_eq!(rate.level_at_most(1), 5);
    }

    #[test]
    fn rate_increases_wait_for_an_idr_due_anyway_but_decreases_apply_at_once() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut applier = RateApplier::new(Duration::from_secs(30));
        assert_eq!(applier.select(12_000_000, 60, true, at(0)), 12_000_000);
        applier.idr_sent(at(0));
        assert_eq!(applier.select(7_300_000, 60, false, at(1000)), 7_300_000);
        applier.idr_sent(at(1000));
        for ms in [11_000, 20_000, 30_999] {
            assert_eq!(
                applier.select(12_000_000, 60, false, at(ms)),
                7_300_000,
                "no extra IDR for an increase within the hold time"
            );
        }
        assert_eq!(
            applier.select(4_400_000, 60, false, at(31_000)),
            4_400_000,
            "a pending increase never delays a decrease"
        );
        applier.idr_sent(at(31_000));
        assert_eq!(applier.select(12_000_000, 60, true, at(32_000)), 12_000_000);
        applier.idr_sent(at(32_000));
        assert_eq!(applier.select(7_300_000, 60, false, at(33_000)), 7_300_000);
        applier.idr_sent(at(33_000));
        assert_eq!(
            applier.select(12_000_000, 30, false, at(34_000)),
            12_000_000,
            "a cadence change recreates the encoder anyway"
        );
        applier.idr_sent(at(34_000));
        assert_eq!(applier.select(7_300_000, 30, false, at(35_000)), 7_300_000);
        applier.idr_sent(at(35_000));
        assert_eq!(applier.select(12_000_000, 30, false, at(64_999)), 7_300_000);
        assert_eq!(
            applier.select(12_000_000, 30, false, at(65_000)),
            12_000_000,
            "the increase replaces the next periodic IDR"
        );
    }

    #[test]
    fn video_config_defaults_gop_and_gpu_capture() {
        let config = VideoConfig::default();
        assert_eq!(config.gop_seconds, 30);
        assert_eq!(config.rate_increase_defer(), Duration::from_secs(30));
        assert!(config.gpu_capture);
        let short = VideoConfig {
            gop_seconds: 5,
            ..config
        };
        assert_eq!(short.rate_increase_defer(), Duration::from_secs(5));
        let long = VideoConfig {
            gop_seconds: 3600,
            ..config
        };
        assert_eq!(long.rate_increase_defer(), Duration::from_secs(30));
        assert!(gpu_capture_setting(None, true).unwrap());
        assert!(!gpu_capture_setting(None, false).unwrap());
        assert!(!gpu_capture_setting(Some("shm"), true).unwrap());
        assert!(gpu_capture_setting(Some("auto"), false).unwrap());
        for value in ["", "AUTO", "gpu", " shm"] {
            assert!(gpu_capture_setting(Some(value), true).is_err());
        }
        for value in ["1", "30", "3600"] {
            assert!(setting("NIRI_RDP_GOP_SECONDS", Some(value), 30, 3600).is_ok());
        }
        for value in ["0", "3601", "-1", "infinite"] {
            assert!(setting("NIRI_RDP_GOP_SECONDS", Some(value), 30, 3600).is_err());
        }
    }

    #[test]
    fn stable_long_rtt_does_not_cause_bitrate_collapse() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        for id in 0..30 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.sent(id, at);
            rate.check_stall(at + Duration::from_millis(220));
            rate.acknowledged(id, 0, at + Duration::from_millis(250));
        }
        assert_eq!(rate.current(), 12_000_000);
        rate.sent(30, start + Duration::from_secs(15));
        rate.acknowledged(30, 0, start + Duration::from_millis(15400));
        assert_eq!(
            rate.current(),
            7_300_000,
            "delay above baseline reduces bitrate"
        );
    }

    #[test]
    fn sustained_empty_queue_path_change_relearns_and_recovers_both_directions() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(20));
        for id in 1..=150 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(250));
        }
        assert_eq!(rate.baseline, Some(Duration::from_millis(250)));
        assert_eq!(
            rate.current(),
            12_000_000,
            "stable changed path must recover useful quality"
        );
        rate.sent(151, start + Duration::from_secs(76));
        rate.acknowledged(151, 0, start + Duration::from_millis(76020));
        assert_eq!(rate.baseline, Some(Duration::from_millis(20)));
        rate.sent(152, start + Duration::from_secs(77));
        rate.acknowledged(152, 0, start + Duration::from_millis(77250));
        assert!(
            rate.current() < rate.ceiling(),
            "renewed excess delay must remain visible"
        );
    }

    #[test]
    fn backlog_jitter_and_audio_cannot_raise_the_baseline() {
        for queued in [true, false] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.sent(0, start);
            rate.acknowledged(0, 0, start + Duration::from_millis(20));
            for id in 1..40 {
                let at = start + Duration::from_millis(u64::from(id) * 500);
                rate.sent(id, at);
                let delay = if queued || id % 2 == 0 { 250 } else { 450 };
                rate.acknowledged(
                    id,
                    if queued { 100_000 } else { 0 },
                    at + Duration::from_millis(delay),
                );
            }
            assert_eq!(rate.baseline, Some(Duration::from_millis(20)));
            assert_eq!(rate.current(), 1_000_000);
        }
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(20));
        for id in 1..30 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.audio_congested(at);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(250));
        }
        assert_eq!(rate.baseline, Some(Duration::from_millis(20)));
        rate.reset_frames();
        assert!(rate.baseline.is_none());
        assert!(rate.last_audio_congestion.is_some());
    }

    #[test]
    fn cadence_and_window_are_bounded_and_follow_pressure() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(30);
        rate.enable_joint_adaptation(true);
        for id in 0..60 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(100));
            assert!(rate.sent.len() <= 8);
            assert!((2..=8).contains(&rate.max_frames_in_flight()));
        }
        assert_eq!(rate.frame_rate(), 30);
        assert_eq!(
            rate.max_frames_in_flight(),
            5,
            "credits cover the 100ms ACK delay at the 30fps target"
        );
        rate.audio_congested(start + Duration::from_secs(31));
        assert!(rate.frame_rate() < 30);
        assert!(!rate.permits_refinement(start + Duration::from_secs(31)));
        rate.reset_frames();
        assert_eq!(rate.max_frames_in_flight(), 2);
    }

    /// Model a fixed ACK delay at a 60fps source and return frames delivered
    /// in the final ten seconds of a 30-second run.
    fn delivered_over_wan(rate: &mut AdaptiveBitrate, start: Instant, ack_ms: u64) -> u32 {
        let mut flight = VecDeque::new();
        let (mut id, mut next, mut delivered) = (0, 0, 0);
        for ms in 0..30_000 {
            let now = start + Duration::from_millis(ms);
            while flight.front().is_some_and(|&(_, ack)| ack <= ms) {
                let (id, _) = flight.pop_front().unwrap();
                rate.acknowledged(id, 0, now);
                if ms >= 20_000 {
                    delivered += 1;
                }
            }
            rate.check_stall(now);
            if ms >= next && flight.len() < rate.max_frames_in_flight() as usize {
                rate.sent(id, now);
                flight.push_back((id, ms + ack_ms));
                id += 1;
                next = ms + 17;
            }
            assert!(flight.len() <= 8);
        }
        delivered
    }

    #[test]
    fn default_window_grows_past_two_credits_on_a_healthy_wan_path() {
        // Two fixed credits measured 18-24fps from Windows 10 mstsc at ~33ms
        // RTT. Without joint FPS adaptation, the window still follows the
        // ACK delay so delivery reaches the 60fps target.
        for ack_ms in [33, 60, 100] {
            let start = Instant::now();
            let mut rate = AdaptiveBitrate::new(12_000_000, start);
            rate.set_frame_rate_ceiling(60);
            let delivered = delivered_over_wan(&mut rate, start, ack_ms);
            assert!(rate.max_frames_in_flight() > 2, "{ack_ms}ms");
            assert!(
                delivered >= 550,
                "{ack_ms}ms ACK delay delivered only {delivered} frames in 10s"
            );
            assert_eq!(rate.frame_rate(), 60, "FPS coordination stays opt-in");
        }
    }

    #[test]
    fn default_window_grows_promptly_and_congestion_shrinks_it() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.set_frame_rate_ceiling(60);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(100));
        assert_eq!(rate.max_frames_in_flight(), 8, "first healthy ACK sizes it");
        // A stalled frame shrinks the window one step per reduction interval.
        rate.sent(1, start + Duration::from_millis(200));
        rate.check_stall(start + Duration::from_millis(800));
        assert_eq!(rate.max_frames_in_flight(), 7);
        rate.check_stall(start + Duration::from_millis(900));
        assert_eq!(rate.max_frames_in_flight(), 7, "reduction has a cooldown");
        rate.check_stall(start + Duration::from_millis(1300));
        assert_eq!(rate.max_frames_in_flight(), 6);
        // After congestion, a healthy ACK alone does not regrow it at once.
        rate.sent(2, start + Duration::from_millis(1400));
        rate.acknowledged(2, 0, start + Duration::from_millis(1500));
        assert_eq!(rate.max_frames_in_flight(), 6);
        for (id, ms) in (3..).zip((1600..=5000).step_by(100)) {
            rate.sent(id, start + Duration::from_millis(ms));
            rate.acknowledged(id, 0, start + Duration::from_millis(ms + 100));
        }
        assert_eq!(rate.max_frames_in_flight(), 8, "stable health regrows it");
        rate.reset_frames();
        assert_eq!(rate.max_frames_in_flight(), 2);
    }

    #[test]
    fn full_cadence_delivery_grows_a_bounded_window_without_a_capture_queue() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.enable_joint_adaptation(true);
        rate.set_frame_rate_ceiling(60);
        let mut flight = VecDeque::new();
        let (mut id, mut next, mut delivered) = (0, 0, 0);
        for ms in 0..30_000 {
            let now = start + Duration::from_millis(ms);
            while flight.front().is_some_and(|&(_, ack)| ack <= ms) {
                let (id, _) = flight.pop_front().unwrap();
                rate.acknowledged(id, 0, now);
                if ms >= 20_000 {
                    delivered += 1;
                }
            }
            rate.check_stall(now);
            if ms >= next && flight.len() < rate.max_frames_in_flight() as usize {
                rate.sent(id, now);
                flight.push_back((id, ms + 120));
                id += 1;
                next = ms + 17;
            }
            assert!(flight.len() <= 8);
            assert!(rate.sent.len() <= 8);
        }
        assert!(
            delivered >= 500,
            "100ms path must recover >50fps with fresh frames"
        );
        assert_eq!(rate.current(), 12_000_000);
        assert_eq!(rate.max_frames_in_flight(), 8);
    }

    #[test]
    fn audio_congestion_reduces_video_despite_healthy_graphics_and_recovers_afterward() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(20));
        for id in 1..=10 {
            let at = start + Duration::from_millis(u64::from(id) * 500);
            rate.audio_congested(at);
            let reduced = rate.current();
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(20));
            assert_eq!(
                rate.current(),
                reduced,
                "healthy graphics cannot undo sound pressure"
            );
        }
        assert_eq!(rate.current(), 1_000_000);
        let quiet = start + Duration::from_millis(5020);
        let acknowledge = |ids: std::ops::RangeInclusive<u32>, rate: &mut AdaptiveBitrate| {
            for id in ids {
                let at = quiet + Duration::from_millis(u64::from(id - 10) * 100);
                rate.sent(id, at);
                rate.acknowledged(id, 0, at + Duration::from_millis(20));
            }
        };
        acknowledge(11..=109, &mut rate);
        assert_eq!(rate.current(), 1_000_000, "recovery holds for 10 s");
        acknowledge(110..=130, &mut rate);
        assert_eq!(
            rate.current(),
            1_600_000,
            "recovery resumes one level after sound pressure ends"
        );
    }

    #[test]
    fn audio_pressure_applies_without_graphics_acks_and_shares_the_reduction_cooldown() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, u32::MAX, start + Duration::from_millis(20));
        rate.audio_congested(start + Duration::from_millis(500));
        assert_eq!(rate.current(), 12_000_000, "first reduction waits 1 s");
        rate.audio_congested(start + Duration::from_millis(1000));
        assert_eq!(rate.current(), 7_300_000);
        rate.audio_congested(start + Duration::from_millis(1001));
        rate.check_stall(start + Duration::from_millis(1900));
        assert_eq!(
            rate.current(),
            7_300_000,
            "audio reports do not bypass the cooldown"
        );
        for half_second in 4..30 {
            rate.audio_congested(start + Duration::from_millis(half_second * 500));
        }
        assert_eq!(rate.current(), 1_000_000);
        assert!(
            rate.sent.is_empty(),
            "audio pressure needs no graphics frame sample"
        );
        rate.reset_frames();
        assert!(
            rate.last_audio_congestion.is_some(),
            "graphics reactivation retains audio pressure"
        );
    }

    #[test]
    fn replacement_controllers_inherit_only_recent_audio_pressure() {
        let start = Instant::now();
        let mut previous = AdaptiveBitrate::new(12_000_000, start);
        previous.sent(99, start);
        previous.audio_congested(start + Duration::from_millis(1000));
        let now = start + Duration::from_millis(1100);
        let mut replacement = AdaptiveBitrate::new(12_000_000, now);
        replacement.inherit_audio_congestion(&previous, now);
        assert_eq!(replacement.current(), 7_300_000);
        assert!(replacement.last_audio_congestion.is_some());
        assert!(replacement.sent.is_empty());
        assert!(replacement.baseline.is_none());
        let stale = start + Duration::from_secs(4);
        let mut replacement = AdaptiveBitrate::new(12_000_000, stale);
        replacement.inherit_audio_congestion(&previous, stale);
        assert_eq!(replacement.current(), 12_000_000);
        assert!(replacement.last_audio_congestion.is_none());
        previous.clear_audio_congestion();
        let mut replacement = AdaptiveBitrate::new(12_000_000, now);
        replacement.inherit_audio_congestion(&previous, now);
        assert_eq!(replacement.current(), 12_000_000);
    }

    #[test]
    fn missing_acknowledgements_reduce_to_floor_with_bounded_tracking() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        for half_second in 1..30 {
            rate.check_stall(start + Duration::from_millis(half_second * 500 + 1));
        }
        assert_eq!(rate.current(), 1_000_000);
        for id in 1..100 {
            rate.sent(id, start + Duration::from_secs(16));
        }
        assert_eq!(rate.sent.len(), 2);
        rate.reset_frames();
        rate.check_stall(start + Duration::from_secs(30));
        assert!(rate.sent.is_empty());
    }

    #[test]
    fn suspension_and_unknown_acknowledgements_do_not_create_congestion_samples() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, u32::MAX, start + Duration::from_secs(1));
        for id in 1..100 {
            rate.sent(id, start + Duration::from_secs(2));
        }
        rate.check_stall(start + Duration::from_secs(5));
        assert_eq!(rate.current(), 12_000_000);
        assert!(rate.sent.is_empty());
        rate.acknowledged(99, 0, start + Duration::from_secs(6));
        rate.acknowledged(1000, 99, start + Duration::from_secs(7));
        assert_eq!(rate.current(), 12_000_000);
        rate.sent(100, start + Duration::from_secs(7));
        assert_eq!(rate.sent.len(), 1, "resume enables tracking again");
    }
    #[test]
    fn performance_settings_are_bounded_and_strict() {
        for maximum in [60, 100] {
            assert_eq!(setting("test", None, 30, maximum).unwrap(), 30);
            assert_eq!(setting("test", Some("1"), 30, maximum).unwrap(), 1);
            assert_eq!(
                setting("test", Some(&maximum.to_string()), 30, maximum).unwrap(),
                maximum
            );
            for value in ["0", "-1", "1.5", "", " 30", "4294967296"] {
                assert!(setting("test", Some(value), 30, maximum).is_err());
            }
            assert!(setting("test", Some(&(maximum + 1).to_string()), 30, maximum).is_err());
        }
    }
    #[test]
    fn explicit_link_budget_reserves_pcm_without_raising_user_video_ceiling() {
        assert_eq!(effective_bitrate(12_000_000, Some(5)).unwrap(), 3_300_000);
        assert_eq!(effective_bitrate(1_000_000, Some(5)).unwrap(), 1_000_000);
        assert_eq!(effective_bitrate(12_000_000, None).unwrap(), 12_000_000);
        for link in [0, 1, 2, 101, u32::MAX] {
            assert!(effective_bitrate(12_000_000, Some(link)).is_err());
        }
    }
}
