//! Isolated transport seam: no compositor, OS input devices, or host credentials.
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_server::{
    BitmapUpdate, ConnectionHandler, DesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent,
    PixelFormat, PostConnectionAction, RdpServerDisplay, RdpServerDisplayUpdates,
    RdpServerInputHandler,
};

pub const WIDTH: u16 = 640;
pub const HEIGHT: u16 = 480;
pub const LISTEN_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 33990);

#[derive(Default, Debug)]
pub struct InputState {
    pub keys: BTreeSet<(u8, bool)>,
    pub unicode: BTreeSet<u16>,
    pub buttons: BTreeSet<u8>,
    pub pointer: (u16, u16),
    pub events: u64,
    pub generation: u64,
}

impl InputState {
    pub fn release_all(&mut self) {
        self.keys.clear();
        self.unicode.clear();
        self.buttons.clear();
        self.pointer = (0, 0);
        self.events = 0;
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Clone, Default)]
pub struct SyntheticBackend(pub Arc<Mutex<InputState>>);

impl RdpServerInputHandler for SyntheticBackend {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let mut state = self.0.lock().unwrap();
        state.events = state.events.wrapping_add(1);
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                state.keys.insert((code, extended));
            }
            KeyboardEvent::Released { code, extended } => {
                state.keys.remove(&(code, extended));
            }
            KeyboardEvent::UnicodePressed(code) => {
                state.unicode.insert(code);
            }
            KeyboardEvent::UnicodeReleased(code) => {
                state.unicode.remove(&code);
            }
            KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let mut state = self.0.lock().unwrap();
        state.events = state.events.wrapping_add(1);
        let button = match event {
            MouseEvent::LeftPressed => Some((1, true)),
            MouseEvent::LeftReleased => Some((1, false)),
            MouseEvent::RightPressed => Some((2, true)),
            MouseEvent::RightReleased => Some((2, false)),
            MouseEvent::MiddlePressed => Some((3, true)),
            MouseEvent::MiddleReleased => Some((3, false)),
            MouseEvent::Button4Pressed => Some((4, true)),
            MouseEvent::Button4Released => Some((4, false)),
            MouseEvent::Button5Pressed => Some((5, true)),
            MouseEvent::Button5Released => Some((5, false)),
            MouseEvent::Move { x, y } => {
                state.pointer = (x.min(WIDTH - 1), y.min(HEIGHT - 1));
                None
            }
            MouseEvent::RelMove { x, y } => {
                state.pointer = (
                    (i64::from(state.pointer.0) + i64::from(x)).clamp(0, i64::from(WIDTH - 1))
                        as u16,
                    (i64::from(state.pointer.1) + i64::from(y)).clamp(0, i64::from(HEIGHT - 1))
                        as u16,
                );
                None
            }
            MouseEvent::VerticalScroll { .. } | MouseEvent::Scroll { .. } => None,
        };
        if let Some((button, down)) = button {
            if down {
                state.buttons.insert(button);
            } else {
                state.buttons.remove(&button);
            }
        }
    }
}

impl ConnectionHandler for SyntheticBackend {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        if !peer.ip().is_loopback() {
            return false;
        }
        self.0.lock().unwrap().release_all();
        true
    }

    fn on_disconnected(
        &mut self,
        _: SocketAddr,
        _: Duration,
        _: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        self.0.lock().unwrap().release_all();
        PostConnectionAction::Continue
    }
}

/// Deterministic BGRA test card; a cyan cross follows the synthetic pointer,
/// and the top band changes when a key/button is held. No input values are logged.
pub fn frame(state: &InputState) -> BitmapUpdate {
    let stride = usize::from(WIDTH) * 4;
    let mut data = vec![0; stride * usize::from(HEIGHT)];
    let pressed = !state.keys.is_empty() || !state.unicode.is_empty() || !state.buttons.is_empty();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let offset = usize::from(y) * stride + usize::from(x) * 4;
            let cross = x.abs_diff(state.pointer.0) <= 2 || y.abs_diff(state.pointer.1) <= 2;
            let pixel = if cross {
                [240, 240, 20, 255]
            } else if y < 32 && pressed {
                [30, 210, 30, 255]
            } else {
                [((x / 40 + y / 40) % 2 * 80 + 35) as u8, 35, 55, 255]
            };
            data[offset..offset + 4].copy_from_slice(&pixel);
        }
    }
    BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(WIDTH).unwrap(),
        height: NonZeroU16::new(HEIGHT).unwrap(),
        format: PixelFormat::BgrA32,
        data: data.into(),
        stride: NonZeroUsize::new(stride).unwrap(),
    }
}

struct Updates {
    backend: SyntheticBackend,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(Some(DisplayUpdate::Bitmap(frame(
            &self.backend.0.lock().unwrap(),
        ))))
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for SyntheticBackend {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: WIDTH,
            height: HEIGHT,
        }
    }
    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(Updates {
            backend: self.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_listener_cannot_overlap_gnome_or_be_public() {
        assert_eq!(LISTEN_ADDR.to_string(), "127.0.0.1:33990");
        let mut backend = SyntheticBackend::default();
        assert!(!backend.on_accept("192.0.2.1:50000".parse().unwrap()));
        assert!(backend.on_accept("127.0.0.1:50000".parse().unwrap()));
    }

    #[test]
    fn frame_has_correct_layout_and_changes_on_input() {
        let mut backend = SyntheticBackend::default();
        let before = frame(&backend.0.lock().unwrap());
        assert_eq!(before.stride.get(), 640 * 4);
        assert_eq!(before.data.len(), 640 * 480 * 4);
        assert!(before.data.chunks_exact(4).all(|pixel| pixel[3] == 255));
        backend.keyboard(KeyboardEvent::Pressed {
            code: 30,
            extended: false,
        });
        let after = frame(&backend.0.lock().unwrap());
        assert_ne!(before.data, after.data);
    }

    #[test]
    fn extreme_pointer_values_are_bounded_without_overflow() {
        let mut backend = SyntheticBackend::default();
        backend.mouse(MouseEvent::Move {
            x: u16::MAX,
            y: u16::MAX,
        });
        assert_eq!(backend.0.lock().unwrap().pointer, (WIDTH - 1, HEIGHT - 1));
        backend.mouse(MouseEvent::RelMove {
            x: i32::MIN,
            y: i32::MAX,
        });
        assert_eq!(backend.0.lock().unwrap().pointer, (0, HEIGHT - 1));
    }

    #[test]
    fn disconnect_on_error_releases_all_input_before_reconnect() {
        let mut backend = SyntheticBackend::default();
        backend.keyboard(KeyboardEvent::Pressed {
            code: 29,
            extended: false,
        });
        backend.keyboard(KeyboardEvent::Pressed {
            code: 29,
            extended: true,
        });
        backend.keyboard(KeyboardEvent::UnicodePressed(0x3042));
        backend.mouse(MouseEvent::LeftPressed);
        backend.mouse(MouseEvent::Button5Pressed);
        assert_eq!(backend.0.lock().unwrap().keys.len(), 2);
        let err = anyhow::anyhow!("synthetic transport failure");
        backend.on_disconnected(LISTEN_ADDR, Duration::ZERO, Some(&err));
        let state = backend.0.lock().unwrap();
        assert!(state.keys.is_empty() && state.unicode.is_empty() && state.buttons.is_empty());
        assert_eq!(state.generation, 1);
        drop(state);
        assert!(backend.on_accept(LISTEN_ADDR));
        assert_eq!(backend.0.lock().unwrap().generation, 2);
    }

    #[test]
    fn matching_releases_preserve_other_keys() {
        let mut backend = SyntheticBackend::default();
        backend.keyboard(KeyboardEvent::Pressed {
            code: 29,
            extended: false,
        });
        backend.keyboard(KeyboardEvent::Pressed {
            code: 29,
            extended: true,
        });
        backend.keyboard(KeyboardEvent::Released {
            code: 29,
            extended: false,
        });
        assert_eq!(backend.0.lock().unwrap().keys, BTreeSet::from([(29, true)]));
    }
}
