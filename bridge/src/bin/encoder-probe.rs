//! Probe the real NVENC path without a display or an RDP listener.
use ironrdp_server::DesktopSize;
use niri_rdp_bridge::{
    bitmap,
    video::{Encoder, h264_has_idr},
};
use std::{
    io::Write,
    time::{Duration, Instant},
};
fn setting(name: &str, default: u32, maximum: u32) -> anyhow::Result<u32> {
    let value = match std::env::var(name) {
        Ok(value) => value.parse::<u32>()?,
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        (1..=maximum).contains(&value),
        "{name} must be within 1..{maximum}"
    );
    Ok(value)
}
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: encoder-probe OUTPUT.h264"))?;
    anyhow::ensure!(args.next().is_none(), "usage: encoder-probe OUTPUT.h264");
    let width = setting("NIRI_RDP_PROBE_WIDTH", 1920, 4096)?;
    let height = setting("NIRI_RDP_PROBE_HEIGHT", 1080, 4096)?;
    let frames = setting("NIRI_RDP_PROBE_FRAMES", 30, 10000)?;
    let mut encoder = Encoder::nvenc(width as u16, height as u16)?;
    // Refuse to overwrite an existing recording.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut encoding_time = Duration::ZERO;
    let mut samples = Vec::with_capacity(frames as usize);
    let mut bytes = 0usize;
    // Only frame 0 is forced; any other IDR comes from the encoder's GOP.
    let mut idr_frames = Vec::new();
    let mut pixels = vec![0; (width * height * 4) as usize];
    for frame in 0..frames {
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                pixels[i..i + 4].copy_from_slice(&[
                    (x / 8) as u8,
                    (y / 4) as u8,
                    (frame * 8) as u8,
                    255,
                ]);
            }
        }
        let image = bitmap(
            &pixels,
            width,
            height,
            width * 4,
            false,
            DesktopSize {
                width: width as u16,
                height: height as u16,
            },
            false,
        )?;
        let started = Instant::now();
        let encoded = encoder.encode(&image, frame == 0)?;
        let elapsed = started.elapsed();
        encoding_time += elapsed;
        samples.push(elapsed);
        bytes += encoded.len();
        if h264_has_idr(&encoded) {
            idr_frames.push(frame);
        }
        file.write_all(&encoded)?;
    }
    println!(
        "NVENC: encoded {frames} changing {width}x{height} desktop frames ({}x{} H.264 bitstream)",
        width.div_ceil(16) * 16,
        height.div_ceil(16) * 16
    );
    println!(
        "Synthetic encoding: {:.2} ms/frame ({:.1} frames/s); excludes capture and transport",
        encoding_time.as_secs_f64() * 1000.0 / f64::from(frames),
        f64::from(frames) / encoding_time.as_secs_f64()
    );
    println!(
        "IDR frames: {} at {:?} (only frame 0 is forced)",
        idr_frames.len(),
        idr_frames
    );
    samples.sort_unstable();
    println!(
        "Encoding latency: median {:.2} ms, p95 {:.2} ms, max {:.2} ms; {bytes} bytes",
        samples[samples.len() / 2].as_secs_f64() * 1000.0,
        samples[(samples.len() * 95 / 100).min(samples.len() - 1)].as_secs_f64() * 1000.0,
        samples.last().unwrap().as_secs_f64() * 1000.0
    );
    Ok(())
}
