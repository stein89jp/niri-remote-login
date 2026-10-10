//! Probe the real NVENC path without a display or an RDP listener.
use ironrdp_server::DesktopSize;
use niri_rdp_bridge::{bitmap, video::Encoder};
use std::{
    io::Write,
    time::{Duration, Instant},
};
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: encoder-probe OUTPUT.h264"))?;
    anyhow::ensure!(args.next().is_none(), "usage: encoder-probe OUTPUT.h264");
    let mut encoder = Encoder::nvenc(1920, 1080)?;
    // Refuse to overwrite an existing recording.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut encoding_time = Duration::ZERO;
    for frame in 0..30u32 {
        let mut pixels = vec![0; 1920 * 1080 * 4];
        for y in 0..1080u32 {
            for x in 0..1920u32 {
                let i = ((y * 1920 + x) * 4) as usize;
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
            1920,
            1080,
            7680,
            false,
            DesktopSize {
                width: 1920,
                height: 1080,
            },
            false,
        )?;
        let started = Instant::now();
        let encoded = encoder.encode(&image, frame == 0)?;
        encoding_time += started.elapsed();
        file.write_all(&encoded)?;
    }
    println!("NVENC: encoded 30 changing 1920x1080 desktop frames (1920x1088 H.264 bitstream)");
    println!(
        "Synthetic encoding: {:.2} ms/frame ({:.1} frames/s); excludes capture and transport",
        encoding_time.as_secs_f64() * 1000.0 / 30.0,
        30.0 / encoding_time.as_secs_f64()
    );
    Ok(())
}
