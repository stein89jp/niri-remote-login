//! Bounded performance settings shared by capture and encoding.
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

/// Keep interactive graphics within a short delivery budget. Feedback includes
/// network transit and client decoding, so this is not an audio/video clock.
pub(crate) struct AdaptiveBitrate {
    ceiling: u32,
    current: u32,
    sent: HashMap<u32, Instant>,
    suspended: bool,
    last_change: Instant,
    healthy_since: Option<Instant>,
    baseline: Option<Duration>,
    baseline_candidate: Option<(Instant, Duration, Duration, u32)>,
    fps_ceiling: u32,
    window: u32,
    last_window_change: Instant,
    last_queue: u32,
    last_ack: Option<Instant>,
    last_send: Option<Instant>,
    send_interval: Option<Duration>,
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
    const REDUCE_INTERVAL: Duration = Duration::from_millis(500);
    const RECOVER_INTERVAL: Duration = Duration::from_secs(3);

    pub(crate) fn new(ceiling: u32, now: Instant) -> Self {
        Self {
            ceiling,
            current: ceiling,
            sent: HashMap::new(),
            suspended: false,
            last_change: now,
            healthy_since: None,
            baseline: None,
            baseline_candidate: None,
            fps_ceiling: 60,
            window: 2,
            last_window_change: now,
            last_queue: 0,
            last_ack: None,
            last_send: None,
            send_interval: None,
            joint: false,
            diagnostics: Telemetry::from_env(),
            last_audio_congestion: None,
        }
    }
    pub(crate) fn ceiling(&self) -> u32 {
        self.ceiling
    }
    pub(crate) fn current(&self) -> u32 {
        self.current
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
        (u64::from(self.fps_ceiling) * u64::from(self.current) / u64::from(self.ceiling)).clamp(
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
            && self.current >= self.ceiling / 2
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
        self.last_send = None;
        self.send_interval = None;
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
            self.current = self.current.min(previous.current);
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
            let interval = now.saturating_duration_since(previous);
            if !interval.is_zero() {
                self.send_interval = Some(
                    self.send_interval
                        .map_or(interval, |previous| previous * 3 / 4 + interval / 4),
                );
            }
            self.diagnostics
                .sample("send_cadence", now.saturating_duration_since(previous));
        }
        self.last_send = Some(now);
        self.diagnostics.count("sent_frames", 1);
        self.diagnostics
            .count("window_samples", u64::from(self.window));
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
        if self.joint
            && self.window > 2
            && now.saturating_duration_since(self.last_window_change) >= Self::REDUCE_INTERVAL
        {
            self.window -= 1;
            self.last_window_change = now;
        }
        if now.duration_since(self.last_change) >= Self::REDUCE_INTERVAL {
            self.current = (self.current * 3 / 4).max(1_000_000).min(self.ceiling);
            self.last_change = now;
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
        let queue_budget = u64::from(self.current) * Self::BUDGET.as_millis() as u64 / 8_000;
        if u64::from(queue_depth) > queue_budget || elapsed > budget {
            self.congested(now);
            return;
        }
        let healthy = *self.healthy_since.get_or_insert(now);
        // Allow enough credits for the measured round trip, but never more than
        // eight. Increase one at a time after a stable interval; congestion
        // reduces the window and FPS together. No capture queue is introduced.
        let measured_fps = self
            .send_interval
            .filter(|duration| !duration.is_zero())
            .map_or(self.frame_rate(), |duration| {
                ((1_000_000 / duration.as_micros().max(1)).clamp(1, u128::from(self.frame_rate())))
                    as u32
            });
        let desired =
            (elapsed.as_micros() * u128::from(measured_fps) / 1_000_000 + 2).clamp(2, 8) as u32;
        if self.joint
            && now.saturating_duration_since(healthy) >= Self::RECOVER_INTERVAL
            && now.saturating_duration_since(self.last_window_change) >= Self::RECOVER_INTERVAL
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            self.window = self.window.saturating_add(1).min(desired);
            self.last_window_change = now;
        }
        if now.duration_since(healthy) >= Self::RECOVER_INTERVAL
            && now.duration_since(self.last_change) >= Self::RECOVER_INTERVAL
            && self
                .last_audio_congestion
                .is_none_or(|at| now.saturating_duration_since(at) >= Self::RECOVER_INTERVAL)
        {
            self.current = self
                .current
                .saturating_add((self.current / 10).max(250_000))
                .min(self.ceiling);
            self.last_change = now;
        }
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
pub(crate) fn gpu_capture_requested() -> anyhow::Result<bool> {
    match environment("NIRI_RDP_GPU_CAPTURE")?
        .as_deref()
        .unwrap_or("shm")
    {
        "auto" => Ok(true),
        "shm" => Ok(false),
        _ => anyhow::bail!("NIRI_RDP_GPU_CAPTURE must be auto or shm"),
    }
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

/// Optional bounded server-side timing receipts. These are durations/counters,
/// never desktop pixels, input text, clipboard contents or credentials. ACKs
/// include transport and decoding and do not measure physical presentation.
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
            enabled: std::env::var("NIRI_RDP_DIAGNOSTICS").as_deref() == Ok("1"),
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
    fn bitrate_reduces_on_delay_or_decoder_backlog_and_recovers_slowly() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, 0, start + Duration::from_millis(30));
        rate.sent(1, start + Duration::from_millis(500));
        rate.acknowledged(1, 0, start + Duration::from_millis(700));
        assert_eq!(rate.current(), 9_000_000);
        rate.sent(2, start + Duration::from_millis(710));
        rate.acknowledged(2, 1_000_000, start + Duration::from_millis(740));
        assert_eq!(rate.current(), 9_000_000, "reductions have a cooldown");
        rate.sent(3, start + Duration::from_millis(1300));
        rate.acknowledged(3, 1_000_000, start + Duration::from_millis(1330));
        assert_eq!(rate.current(), 6_750_000);
        for id in 4..100 {
            let at = start + Duration::from_millis(u64::from(id) * 100);
            // Preserve monotonic time after the congestion samples above.
            if at < start + Duration::from_millis(1400) {
                continue;
            }
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(30));
        }
        assert!(rate.current() > 6_750_000);
        assert!(rate.current() <= 12_000_000);
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
            9_000_000,
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
            2,
            "sparse source frames need no larger credit window"
        );
        rate.audio_congested(start + Duration::from_secs(31));
        assert!(rate.frame_rate() < 30);
        assert!(!rate.permits_refinement(start + Duration::from_secs(31)));
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
        for id in 11..=40 {
            let at = quiet + Duration::from_millis(u64::from(id - 10) * 100);
            rate.sent(id, at);
            rate.acknowledged(id, 0, at + Duration::from_millis(20));
        }
        assert_eq!(
            rate.current(),
            1_250_000,
            "recovery resumes after sound pressure ends"
        );
    }

    #[test]
    fn audio_pressure_applies_without_graphics_acks_and_shares_the_reduction_cooldown() {
        let start = Instant::now();
        let mut rate = AdaptiveBitrate::new(12_000_000, start);
        rate.sent(0, start);
        rate.acknowledged(0, u32::MAX, start + Duration::from_millis(20));
        rate.audio_congested(start + Duration::from_millis(500));
        assert_eq!(rate.current(), 9_000_000);
        rate.audio_congested(start + Duration::from_millis(501));
        rate.check_stall(start + Duration::from_millis(900));
        assert_eq!(
            rate.current(),
            9_000_000,
            "audio reports do not bypass the cooldown"
        );
        for half_second in 2..30 {
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
        previous.audio_congested(start + Duration::from_millis(500));
        let now = start + Duration::from_millis(600);
        let mut replacement = AdaptiveBitrate::new(12_000_000, now);
        replacement.inherit_audio_congestion(&previous, now);
        assert_eq!(replacement.current(), 9_000_000);
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
