//! Bounded performance settings shared by capture and encoding.
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

/// Target H.264 bitrate in megabits per second, bounded before crossing the ABI.
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
