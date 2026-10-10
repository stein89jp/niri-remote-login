//! Disposable foreground validation: explicit socket, real capture/input.
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent, RdpServerInputHandler,
};
use niri_rdp_bridge::wayland::Desktop;
use std::{os::unix::net::UnixStream, path::Path, time::Duration};
fn hash(d: &Desktop) -> anyhow::Result<u64> {
    let image = d.snapshot().ok_or_else(|| anyhow::anyhow!("no image"))?;
    let DisplayUpdate::Bitmap(bitmap) = image.as_ref() else {
        anyhow::bail!("not bitmap")
    };
    Ok(bitmap.data.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    }))
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 2 || args.len() == 3,
        "usage: capture-probe /absolute/private/wayland-socket [output]"
    );
    anyhow::ensure!(
        Path::new(&args[1]).is_absolute(),
        "explicit absolute socket required"
    );
    let mut d = Desktop::start(
        UnixStream::connect(&args[1])?,
        args.get(2).cloned().unwrap_or_else(|| "HEADLESS-1".into()),
        DesktopSize {
            width: 640,
            height: 480,
        },
        None,
    )?;
    let first = hash(&d)?;
    println!("capture ready: 640x480 BGRA, frame_hash={first:016x}");
    std::thread::sleep(Duration::from_millis(500));
    let second = hash(&d)?;
    anyhow::ensure!(
        first != second,
        "application pixels did not change; run animated test app first"
    );
    d.keyboard(KeyboardEvent::Pressed {
        code: 0x1e,
        extended: false,
    });
    d.keyboard(KeyboardEvent::Released {
        code: 0x1e,
        extended: false,
    });
    d.mouse(MouseEvent::Move { x: 320, y: 240 });
    std::thread::sleep(Duration::from_millis(150));
    d.mouse(MouseEvent::Button {
        x: 320,
        y: 240,
        button: MouseButton::Left,
        pressed: true,
    });
    std::thread::sleep(Duration::from_millis(100));
    d.mouse(MouseEvent::Button {
        x: 320,
        y: 240,
        button: MouseButton::Left,
        pressed: false,
    });
    d.keyboard(KeyboardEvent::Pressed {
        code: 0x1d,
        extended: true,
    });
    for (x, y) in [
        (0, 0),
        (639, 0),
        (639, 479),
        (0, 479),
        (320, 240),
        (u16::MAX, u16::MAX),
    ] {
        d.mouse(MouseEvent::Move { x, y });
    }
    d.keyboard(KeyboardEvent::Released {
        code: 0x1d,
        extended: true,
    });
    d.release_all();
    std::thread::sleep(Duration::from_millis(250));
    anyhow::ensure!(d.alive(), "capture/input worker failed");
    println!(
        "changing application pixels: {first:016x} -> {second:016x}; virtual key/pointer/release requests flushed"
    );
    println!(
        "Confirm key/button observations in the compositor test application's log; this probe does not establish Windows interoperability."
    );
    Ok(())
}
