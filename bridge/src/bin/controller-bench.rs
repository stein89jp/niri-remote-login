//! Deterministic delivery-model measurements of the actual graphics controller.
//! Synthetic ACK/serialization timing is not a network or presentation result.
#[allow(dead_code)]
#[path = "../performance.rs"]
mod performance;
use performance::AdaptiveBitrate;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
struct Frame {
    id: u32,
    sent: u64,
    ack: u64,
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 5,
        "usage: controller-bench rtt_ms link_mbps joint_0_or_1 stall_ms"
    );
    let rtt: u64 = args[1].parse()?;
    let link: u64 = args[2].parse()?;
    let joint: u32 = args[3].parse()?;
    let stall: u64 = args[4].parse()?;
    anyhow::ensure!(
        (1..=2000).contains(&rtt) && (3..=100).contains(&link) && joint <= 1 && stall <= 5000,
        "invalid model settings"
    );
    let start = Instant::now();
    let ceiling = ((link * 1_000_000 - 1_700_000).min(12_000_000)) as u32;
    let mut controller = AdaptiveBitrate::new(ceiling, start);
    controller.set_frame_rate_ceiling(60);
    controller.enable_joint_adaptation(joint == 1);
    let mut flight = VecDeque::<Frame>::new();
    let (mut id, mut next_send, mut wire_free, mut peak) = (0, 0, 0, 0);
    let mut ages = Vec::new();
    let mut final_frames = 0;
    for us in (0..30_000_000u64).step_by(1000) {
        let now = start + Duration::from_micros(us);
        while flight.front().is_some_and(|frame| frame.ack <= us) {
            let frame = flight.pop_front().unwrap();
            controller.acknowledged(frame.id, 0, now);
            ages.push(us - frame.sent);
            if us >= 20_000_000 {
                final_frames += 1;
            }
        }
        controller.check_stall(now);
        if us < next_send || flight.len() >= controller.max_frames_in_flight() as usize {
            continue;
        }
        let fps = controller.frame_rate();
        let serialized = u64::from(controller.current()) / u64::from(fps) * 1_000_000
            / (link * 1_000_000 - 1_700_000);
        let mut wire_start = us.max(wire_free);
        if stall > 0 && wire_start >= 12_000_000 && wire_start < 12_000_000 + stall * 1000 {
            wire_start = 12_000_000 + stall * 1000;
        }
        wire_free = wire_start + serialized;
        let ack = wire_free + rtt * 1000 + 3000;
        flight.push_back(Frame { id, sent: us, ack });
        controller.sent(id, now);
        id += 1;
        peak = peak.max(flight.len());
        next_send = us + 1_000_000 / u64::from(fps);
    }
    ages.sort_unstable();
    anyhow::ensure!(!ages.is_empty(), "model delivered no frames");
    println!(
        "{{\"scope\":\"controller_delivery_model\",\"physical_presentation_measured\":false,\"rtt_ms\":{rtt},\"link_mbps_including_pcm\":{link},\"pcm_reserve_mbps\":1.7,\"joint_adaptation\":{},\"stall_ms\":{stall},\"delivered_frames\":{},\"last_10s_fps\":{},\"frame_age_p50_ms\":{},\"frame_age_p95_ms\":{},\"frame_age_max_ms\":{},\"peak_outstanding_frames\":{peak},\"final_target_bps\":{},\"final_target_fps\":{},\"final_window\":{}}}",
        joint == 1,
        ages.len(),
        f64::from(final_frames) / 10.0,
        ages[ages.len() / 2] as f64 / 1000.0,
        ages[ages.len() * 95 / 100] as f64 / 1000.0,
        ages.last().unwrap().to_owned() as f64 / 1000.0,
        controller.current(),
        controller.frame_rate(),
        controller.max_frames_in_flight()
    );
    Ok(())
}
