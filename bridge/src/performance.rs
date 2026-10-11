//! Bounded performance settings shared by capture and encoding.
use std::{
    collections::HashMap,
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
        }
    }
    pub(crate) fn ceiling(&self) -> u32 {
        self.ceiling
    }
    pub(crate) fn current(&self) -> u32 {
        self.current
    }
    pub(crate) fn reset_frames(&mut self) {
        self.sent.clear();
        self.suspended = false;
        self.healthy_since = None;
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
        if !self.suspended {
            // Graphics flow control permits two outstanding frames. Keep this
            // bound even if a client later suspends or sends malformed ACKs.
            if self.sent.len() == 2 {
                let oldest = *self.sent.iter().min_by_key(|(_, at)| **at).unwrap().0;
                self.sent.remove(&oldest);
            }
            self.sent.insert(id, now);
        }
    }
    fn congested(&mut self, now: Instant) {
        self.healthy_since = None;
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
            return;
        }
        self.suspended = false;
        let Some(sent) = self.sent.remove(&id) else {
            return;
        };
        let elapsed = now.duration_since(sent);
        let budget = self.delivery_budget();
        self.baseline = Some(
            self.baseline
                .map_or(elapsed, |baseline| baseline.min(elapsed)),
        );
        // QueueDepth is queued graphics bytes, not a count of frames. Permit
        // at most 150ms of payload at the current target rate.
        let queue_budget = u64::from(self.current) * Self::BUDGET.as_millis() as u64 / 8_000;
        if u64::from(queue_depth) > queue_budget || elapsed > budget {
            self.congested(now);
            return;
        }
        let healthy = *self.healthy_since.get_or_insert(now);
        if now.duration_since(healthy) >= Self::RECOVER_INTERVAL
            && now.duration_since(self.last_change) >= Self::RECOVER_INTERVAL
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
    Ok(setting(
        "NIRI_RDP_BITRATE_MBPS",
        environment("NIRI_RDP_BITRATE_MBPS")?.as_deref(),
        12,
        100,
    )? * 1_000_000)
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
}
