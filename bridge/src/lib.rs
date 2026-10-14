//! Real desktop adapter. IronRDP owns every RDP wire operation.
pub mod audio;
pub mod broker;
pub mod clipboard;
mod cursor_protocol;
mod gpu_capture;
pub(crate) mod keyboard;
pub mod login;
mod login_compose;
pub mod network;
pub mod performance;
pub mod video;
pub mod wayland;

use ironrdp_server::{BitmapUpdate, DesktopSize, PixelFormat, ServerError, ServerErrorKind};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU16, NonZeroUsize},
};

pub const LISTEN_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 33991);
pub const DISPOSABLE_LISTEN_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 33991);
pub const MAX_DIMENSION: u32 = 4096;

/// Broker sessions accept remote clients; disposable test sessions stay local.
pub fn listen_addr(mode: &str, configured: Option<&str>) -> anyhow::Result<SocketAddr> {
    let default = match mode {
        "broker" => LISTEN_ADDR,
        "disposable" => DISPOSABLE_LISTEN_ADDR,
        _ => anyhow::bail!("NIRI_RDP_MODE must be broker or disposable"),
    };
    let addr = match configured {
        Some(value) => value.parse::<SocketAddr>().map_err(|_| {
            anyhow::anyhow!("NIRI_RDP_LISTEN must be a numeric IP address and port, such as 0.0.0.0:33991 or [::]:33991")
        })?,
        None => default,
    };
    anyhow::ensure!(addr.port() != 0, "NIRI_RDP_LISTEN port must not be zero");
    anyhow::ensure!(
        !addr.ip().is_multicast(),
        "NIRI_RDP_LISTEN must not be a multicast address"
    );
    anyhow::ensure!(
        mode != "disposable" || addr.ip().is_loopback(),
        "disposable mode requires a loopback NIRI_RDP_LISTEN address"
    );
    Ok(addr)
}

pub fn server_error(message: impl Into<String>) -> ServerError {
    ServerError::new("niri-rdp bridge", ServerErrorKind::Reason(message.into()))
}

/// RDP set-1 scancodes become Linux evdev keycodes; E0 is not an extra key.
pub fn evdev_key(code: u8, extended: bool) -> Option<u32> {
    if !extended {
        return match code {
            1..=83 | 86..=88 => Some(u32::from(code)),
            0x70 => Some(93),  // Hiragana/Katakana
            0x73 => Some(89),  // JIS Ro
            0x79 => Some(92),  // Henkan
            0x7b => Some(94),  // Muhenkan
            0x7d => Some(124), // JIS Yen
            _ => None,
        };
    }
    Some(match code {
        0x1c => 96,
        0x1d => 97,
        0x35 => 98,
        0x37 => 99,
        0x38 => 100,
        0x47 => 102,
        0x48 => 103,
        0x49 => 104,
        0x4b => 105,
        0x4d => 106,
        0x4f => 107,
        0x50 => 108,
        0x51 => 109,
        0x52 => 110,
        0x53 => 111,
        0x5b => 125,
        0x5c => 126,
        0x5d => 127,
        _ => return None,
    })
}

/// Validate compositor-provided memory layout before allocating or reading.
pub fn buffer_length(width: u32, height: u32, stride: u32) -> anyhow::Result<usize> {
    anyhow::ensure!(
        (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height),
        "capture dimensions outside 1..4096"
    );
    anyhow::ensure!(
        stride >= width * 4 && stride <= MAX_DIMENSION * 4,
        "invalid capture stride"
    );
    (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("capture overflow"))
}

/// Native BGR SHM already has the wire/encoder pixel order. Its unused alpha
/// byte can remain untouched when the bitmap declares BgrX32.
pub(crate) fn capture_bitmap(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
    target: DesktopSize,
    abgr: bool,
) -> anyhow::Result<BitmapUpdate> {
    if !inverted
        && !abgr
        && width == u32::from(target.width)
        && height == u32::from(target.height)
        && stride == width * 4
    {
        anyhow::ensure!(
            bytes.len() == buffer_length(width, height, stride)?,
            "SHM length mismatch"
        );
        return Ok(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(target.width).unwrap(),
            height: NonZeroU16::new(target.height).unwrap(),
            format: PixelFormat::BgrX32,
            data: bytes.to_vec().into(),
            stride: NonZeroUsize::new(stride as usize).unwrap(),
        });
    }
    bitmap(bytes, width, height, stride, inverted, target, abgr)
}

/// Strip row padding, normalize upside-down SHM and scale the desktop contract.
pub fn bitmap(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    inverted: bool,
    target: DesktopSize,
    abgr: bool,
) -> anyhow::Result<BitmapUpdate> {
    anyhow::ensure!(
        bytes.len() == buffer_length(width, height, stride)?,
        "SHM length mismatch"
    );
    anyhow::ensure!(
        (1..=MAX_DIMENSION as u16).contains(&target.width)
            && (1..=MAX_DIMENSION as u16).contains(&target.height),
        "RDP dimensions outside 1..4096"
    );
    let out_stride = usize::from(target.width) * 4;
    // Every output byte is supplied below; avoid zero-filling a second full
    // desktop allocation before copying the compositor's pixels into it.
    let mut data = Vec::with_capacity(out_stride * usize::from(target.height));
    for y in 0..u32::from(target.height) {
        let sy = y * height / u32::from(target.height);
        let sy = if inverted { height - 1 - sy } else { sy };
        if u32::from(target.width) == width {
            let src = (sy * stride) as usize;
            let dst = data.len();
            data.extend_from_slice(&bytes[src..src + out_stride]);
            let row = &mut data[dst..];
            for pixel in row.as_chunks_mut::<4>().0 {
                if abgr {
                    pixel.swap(0, 2);
                }
                pixel[3] = 255;
            }
            continue;
        }
        for x in 0..u32::from(target.width) {
            let sx = x * width / u32::from(target.width);
            let src = (sy * stride + sx * 4) as usize;
            let dst = data.len();
            data.extend_from_slice(&bytes[src..src + 4]);
            if abgr {
                data.swap(dst, dst + 2);
            }
            data[dst + 3] = 255;
        }
    }
    Ok(BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(target.width).unwrap(),
        height: NonZeroU16::new(target.height).unwrap(),
        format: PixelFormat::BgrA32,
        data: data.into(),
        stride: NonZeroUsize::new(out_stride).unwrap(),
    })
}

/// Compare complete retained desktops, so skipped capture notifications cannot
/// lose damage. At most 32 rectangular tiles are returned; broad damage uses a
/// full-frame update. Subregions share immutable Bytes without pixel copies.
pub(crate) fn bitmap_damage(
    previous: Option<&BitmapUpdate>,
    image: &BitmapUpdate,
) -> Vec<BitmapUpdate> {
    let Some(previous) = previous.filter(|old| {
        old.width == image.width
            && old.height == image.height
            && old.format == image.format
            && old.stride == image.stride
    }) else {
        return vec![image.clone()];
    };
    if previous.data == image.data {
        return Vec::new();
    }
    let width = usize::from(image.width.get());
    let height = usize::from(image.height.get());
    let bpp = usize::from(image.format.bytes_per_pixel());
    let mut regions: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut area = 0usize;
    for y in (0..height).step_by(64) {
        let h = (height - y).min(64);
        let mut run: Option<(usize, usize)> = None;
        for x in (0..width).step_by(64).chain(std::iter::once(width)) {
            let w = (width - x).min(64);
            let changed = x != width
                && (y..y + h).any(|row| {
                    let start = row * image.stride.get() + x * bpp;
                    previous.data[start..start + w * bpp] != image.data[start..start + w * bpp]
                });
            if changed {
                area += w * h;
                if area * 2 >= width * height {
                    return vec![image.clone()];
                }
                run = Some(run.map_or((x, w), |(start, length)| (start, length + w)));
            } else if let Some((start, length)) = run.take() {
                if let Some(rect) = regions
                    .iter_mut()
                    .rev()
                    .find(|rect| rect.0 == start && rect.2 == length && rect.1 + rect.3 == y)
                {
                    rect.3 += h;
                } else {
                    if regions.len() == 32 {
                        return vec![image.clone()];
                    }
                    regions.push((start, y, length, h));
                }
            }
        }
    }
    regions
        .into_iter()
        .map(|(x, y, width, height)| {
            image
                .sub(
                    x as u16,
                    y as u16,
                    NonZeroU16::new(width as u16).unwrap(),
                    NonZeroU16::new(height as u16).unwrap(),
                )
                .unwrap()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_bitmap_damage_accumulates_skipped_and_overlapping_changes() {
        let target = DesktopSize {
            width: 256,
            height: 256,
        };
        let mut pixels = vec![0u8; 256 * 256 * 4];
        let initial = capture_bitmap(&pixels, 256, 256, 1024, false, target, false).unwrap();
        assert!(bitmap_damage(Some(&initial), &initial).is_empty());
        pixels[4] = 1;
        // An intermediate capture changes one tile but is never delivered.
        let skipped = capture_bitmap(&pixels, 256, 256, 1024, false, target, false).unwrap();
        pixels[4] = 2;
        pixels[(130 * 256 + 130) * 4] = 3;
        let current = capture_bitmap(&pixels, 256, 256, 1024, false, target, false).unwrap();
        let damage = bitmap_damage(Some(&initial), &current);
        assert_eq!(damage.len(), 2);
        assert_eq!(
            (
                damage[0].x,
                damage[0].y,
                damage[0].width.get(),
                damage[0].height.get()
            ),
            (0, 0, 64, 64)
        );
        assert_eq!((damage[1].x, damage[1].y), (128, 128));
        assert_eq!(damage[0].data[4], 2);
        assert_eq!(skipped.data[4], 1);
        assert_eq!(damage[1].data[(2 * 256 + 2) * 4], 3);
        let mut broad = pixels.clone();
        broad.fill(99);
        let full = capture_bitmap(&broad, 256, 256, 1024, false, target, false).unwrap();
        let fallback = bitmap_damage(Some(&current), &full);
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].width.get(), 256);
        assert_eq!(fallback[0].height.get(), 256);
    }
    #[test]
    fn scancode_extended_keys_are_distinct_and_unknown_is_rejected() {
        assert_eq!(evdev_key(0x1d, false), Some(29));
        assert_eq!(evdev_key(0x1d, true), Some(97));
        assert_eq!(evdev_key(0x48, true), Some(103));
        assert_eq!(evdev_key(0xff, true), None);
        assert_eq!(evdev_key(0, false), None);
    }
    #[test]
    fn japanese_scancodes_reach_distinct_evdev_keys() {
        for (scan, evdev) in [(0x70, 93), (0x73, 89), (0x79, 92), (0x7b, 94), (0x7d, 124)] {
            assert_eq!(evdev_key(scan, false), Some(evdev));
            assert_eq!(evdev_key(scan, true), None);
        }
    }
    #[test]
    fn shm_bounds_reject_malicious_dimensions_and_stride() {
        for (w, h, s) in [
            (0, 1, 4),
            (4097, 1, 16388),
            (1, 0, 4),
            (4, 1, 15),
            (1, 1, u32::MAX),
        ] {
            assert!(buffer_length(w, h, s).is_err());
        }
        assert_eq!(buffer_length(1920, 1080, 7680).unwrap(), 8_294_400);
    }
    #[test]
    fn bitmap_normalizes_padding_inversion_and_abgr() {
        let bytes = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, 7, 8, 9, 0, 10, 11, 12, 0, 8, 8, 8, 8,
        ];
        let result = bitmap(
            &bytes,
            2,
            2,
            12,
            true,
            DesktopSize {
                width: 2,
                height: 2,
            },
            true,
        )
        .unwrap();
        assert_eq!(
            &result.data[..],
            &[9, 8, 7, 255, 12, 11, 10, 255, 3, 2, 1, 255, 6, 5, 4, 255]
        );
    }
    #[test]
    fn native_capture_preserves_pixels_and_declares_unused_alpha() {
        let bytes = [1, 2, 3, 0, 4, 5, 6, 17];
        let target = DesktopSize {
            width: 2,
            height: 1,
        };
        let native = capture_bitmap(&bytes, 2, 1, 8, false, target, false).unwrap();
        assert_eq!(native.format, PixelFormat::BgrX32);
        assert_eq!(&native.data[..], &bytes);
        // Snapshot ownership must survive the reusable SHM buffer's next write.
        let mut shm = bytes.to_vec();
        let owned = capture_bitmap(&shm, 2, 1, 8, false, target, false).unwrap();
        shm.fill(99);
        assert_eq!(&owned.data[..], &bytes);
        let padded = [1, 2, 3, 0, 4, 5, 6, 17, 99, 99, 99, 99];
        let normalized = capture_bitmap(&padded, 2, 1, 12, false, target, false).unwrap();
        assert_eq!(normalized.format, PixelFormat::BgrA32);
        assert_eq!(&normalized.data[..], &[1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(capture_bitmap(&bytes[..4], 2, 1, 8, false, target, false).is_err());
    }
    #[test]
    fn bitmap_row_copy_and_scaled_paths_preserve_pixel_contract() {
        let bytes = [1, 2, 3, 0, 4, 5, 6, 17, 99, 99, 99, 99];
        let direct = bitmap(
            &bytes,
            2,
            1,
            12,
            false,
            DesktopSize {
                width: 2,
                height: 1,
            },
            false,
        )
        .unwrap();
        assert_eq!(&direct.data[..], &[1, 2, 3, 255, 4, 5, 6, 255]);
        let scaled = bitmap(
            &bytes,
            2,
            1,
            12,
            false,
            DesktopSize {
                width: 4,
                height: 2,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            &scaled.data[..],
            &[
                1, 2, 3, 255, 1, 2, 3, 255, 4, 5, 6, 255, 4, 5, 6, 255, 1, 2, 3, 255, 1, 2, 3, 255,
                4, 5, 6, 255, 4, 5, 6, 255
            ]
        );
    }
    #[test]
    fn broker_listener_defaults_to_direct_connections_and_accepts_custom_addresses() {
        assert_eq!(
            listen_addr("broker", None).unwrap().to_string(),
            "0.0.0.0:33991"
        );
        for address in ["192.168.1.20:3389", "127.0.0.1:33991", "[::]:33991"] {
            assert_eq!(
                listen_addr("broker", Some(address)).unwrap().to_string(),
                address
            );
        }
    }
    #[test]
    fn disposable_listener_never_accepts_non_loopback_configuration() {
        assert_eq!(
            listen_addr("disposable", None).unwrap().to_string(),
            "127.0.0.1:33991"
        );
        for address in ["127.0.0.1:33992", "[::1]:33992"] {
            assert!(listen_addr("disposable", Some(address)).is_ok());
        }
        for address in ["0.0.0.0:33991", "192.168.1.20:33991", "[::]:33991"] {
            assert!(listen_addr("disposable", Some(address)).is_err());
        }
    }
    #[test]
    fn listener_rejects_invalid_configuration() {
        assert!(listen_addr("unknown", None).is_err());
        for address in [
            "",
            "localhost:33991",
            "0.0.0.0",
            "0.0.0.0:0",
            "0.0.0.0:65536",
            "224.0.0.1:33991",
            "[ff02::1]:33991",
        ] {
            assert!(listen_addr("broker", Some(address)).is_err());
        }
    }
}
