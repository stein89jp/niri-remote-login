//! Disposable foreground validation: explicit socket, real capture/input.
use ironrdp_server::{
    DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent, RdpServerDisplay,
    RdpServerInputHandler,
};
use niri_rdp_bridge::wayland::Desktop;
use std::{
    os::unix::net::UnixStream,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
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
    let scale: u32 = std::env::var("NIRI_RDP_PROBE_SCALE_PERCENT")
        .unwrap_or_else(|_| "100".into())
        .parse()?;
    anyhow::ensure!((100..=500).contains(&scale), "probe scale outside 100..500");
    if scale != 100 {
        d.request_layout(
            ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout::new_single_primary_monitor(
                640,
                480,
                Some(scale),
                None,
            )?,
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    if std::env::var("NIRI_RDP_PROBE_CURSOR").as_deref() == Ok("1") {
        d.set_cursor_max_size(96);
        d.mouse(MouseEvent::Move { x: 320, y: 240 });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        let mut updates = d.updates_send();
        let (mut shape, mut position) = (None, false);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while shape.is_none() || !position {
                    match updates.next_update().await? {
                        Some(DisplayUpdate::RGBAPointer(cursor)) => {
                            shape = Some((cursor.width, cursor.height, cursor.hot_x, cursor.hot_y))
                        }
                        Some(DisplayUpdate::PointerPosition(point)) => {
                            position = point.x == 320 && point.y == 240
                        }
                        None => anyhow::bail!("capture ended while waiting for cursor metadata"),
                        _ => {}
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            Ok::<_, anyhow::Error>(())
        })?;
        for (x, y) in [(0, 0), (639, 0), (639, 479), (0, 479), (320, 240)] {
            d.mouse(MouseEvent::Move { x, y });
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if let Some(DisplayUpdate::PointerPosition(point)) =
                            updates.next_update().await?
                            && point.x == x
                            && point.y == y
                        {
                            break;
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await??;
                Ok::<_, anyhow::Error>(())
            })?;
        }
        let (width, height, hot_x, hot_y) = shape.unwrap();
        std::thread::sleep(Duration::from_millis(100));
        println!(
            "{{\"probe\":\"cursor\",\"scale_percent\":{scale},\"width\":{width},\"height\":{height},\"hot_x\":{hot_x},\"hot_y\":{hot_y},\"position\":[320,240]}}"
        );
    }
    if let Ok(configuration) = std::env::var("NIRI_RDP_PROBE_RELOAD_CONFIG") {
        let contents = std::fs::read_to_string(&configuration)?;
        anyhow::ensure!(
            contents.contains("scale 1;"),
            "private reload fixture missing initial scale"
        );
        std::fs::write(configuration, contents.replacen("scale 1;", "scale 2;", 1))?;
        std::thread::sleep(Duration::from_secs(2));
        anyhow::ensure!(d.alive(), "capture failed after output config reload");
        println!("{{\"probe\":\"config_reload\",\"scale_percent\":{scale},\"alive\":true}}");
    }
    if std::env::var("NIRI_RDP_PROBE_IDLE_SECONDS").is_ok() {
        std::thread::sleep(Duration::from_secs(1));
    }
    let first = hash(&d)?;
    println!("capture ready: 640x480 BGRA, frame_hash={first:016x}");
    let idle_seconds: u64 = std::env::var("NIRI_RDP_PROBE_IDLE_SECONDS")
        .unwrap_or_else(|_| "0".into())
        .parse()?;
    anyhow::ensure!(idle_seconds <= 30, "idle probe exceeds 30 seconds");
    std::thread::sleep(if idle_seconds > 0 {
        Duration::from_secs(idle_seconds)
    } else {
        Duration::from_millis(500)
    });
    let second = hash(&d)?;
    if idle_seconds == 0 && std::env::var("NIRI_RDP_PROBE_INPUT_LOG").is_err() {
        anyhow::ensure!(
            first != second,
            "application pixels did not change; run animated test app first"
        );
    } else if idle_seconds > 0 {
        anyhow::ensure!(d.alive(), "quiet desktop disconnected");
        anyhow::ensure!(first == second, "idle desktop unexpectedly changed");
        println!(
            "{{\"probe\":\"damage_idle\",\"seconds\":{idle_seconds},\"alive\":true,\"unchanged\":true}}"
        );
    }
    let before_input = d.snapshot().unwrap();
    let input_started = Instant::now();
    d.keyboard(KeyboardEvent::Pressed {
        code: 0x1e,
        extended: false,
    });
    d.keyboard(KeyboardEvent::Released {
        code: 0x1e,
        extended: false,
    });
    if let Ok(log) = std::env::var("NIRI_RDP_PROBE_INPUT_LOG") {
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("KEY 30 0")
        {
            anyhow::ensure!(
                input_started.elapsed() < Duration::from_millis(250),
                "input missed the 250 ms one-FPS deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let before_next_capture = Arc::ptr_eq(&before_input, &d.snapshot().unwrap());
        anyhow::ensure!(
            before_next_capture,
            "input acknowledgement waited for a fresh capture"
        );
        println!(
            "{{\"probe\":\"input_wake\",\"application_ack_us\":{},\"before_next_capture\":true}}",
            input_started.elapsed().as_micros()
        );
    }
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
    if std::env::var_os("NIRI_RDP_PROBE_RELOAD_CONFIG").is_some() {
        d.mouse(MouseEvent::ButtonRel {
            x: 30,
            y: 0,
            button: MouseButton::Right,
            pressed: true,
        });
        std::thread::sleep(Duration::from_millis(100));
        d.mouse(MouseEvent::ButtonRel {
            x: 0,
            y: 0,
            button: MouseButton::Right,
            pressed: false,
        });
    }
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
