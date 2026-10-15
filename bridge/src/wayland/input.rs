//! Virtual keyboard and pointer input: inherited keymaps, committed Unicode
//! text and wheel notches.
use super::state::State;
use crate::evdev_key;
use crate::keyboard::UnicodeInput;
use ironrdp_server::{DesktopSize, KeyboardEvent, MouseButton, MouseEvent};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsFd,
    time::Instant,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use xkbcommon::xkb;

fn unicode_keymap(character: char) -> String {
    let symbol = match character {
        '\u{8}' => "BackSpace".into(),
        '\t' => "Tab".into(),
        '\r' | '\n' => "Return".into(),
        '\u{1b}' => "Escape".into(),
        '\u{7f}' => "Delete".into(),
        _ => format!("0x{:08x}", 0x01000000 | character as u32),
    };
    // A one-level map commits exact text, independently of Shift or CapsLock.
    // The caller restores the physical keymap and modifiers after the tap.
    format!(
        r#"xkb_keymap {{
        xkb_keycodes "niri_rdp_unicode" {{ minimum = 8; maximum = 255; <TEXT> = 255; }};
        xkb_types {{ type "ONE_LEVEL" {{ modifiers = None; level_name[Level1] = "Any"; }}; }};
        xkb_compatibility {{ }};
        xkb_symbols {{ key <TEXT> {{ type = "ONE_LEVEL", [ {symbol} ] }}; }};
    }};"#
    )
}

pub(super) fn read_keymap(mut file: File, size: u32) -> anyhow::Result<String> {
    anyhow::ensure!(
        (1..=1024 * 1024).contains(&size),
        "invalid compositor keyboard map size"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(u64::from(size)).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() == size as usize && bytes.pop() == Some(0),
        "invalid compositor keyboard map data"
    );
    Ok(String::from_utf8(bytes)?)
}

pub(super) struct PhysicalKeymap {
    pub(super) file: File,
    pub(super) size: u32,
    text: String,
    state: xkb::State,
}

impl PhysicalKeymap {
    pub(super) fn new(text: String) -> anyhow::Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_string(
            &context,
            text.clone(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("invalid compositor XKB keymap"))?;
        let mut file = tempfile::tempfile()?;
        file.write_all(text.as_bytes())?;
        file.write_all(&[0])?;
        Ok(Self {
            file,
            size: text.len() as u32 + 1,
            text,
            state: xkb::State::new(&map),
        })
    }
}

/// `wl_keyboard.keymap_format.no_keymap`.
const NO_KEYMAP: u32 = 0;

/// Make the physical-key device type with the seat's current keymap. Niri
/// then follows layout reloads and `switch-layout` binds without the bridge
/// re-sending a map. The format needs a valid fd but ignores it and the size.
pub(super) fn follow_seat_keymap(keyboard: &ZwpVirtualKeyboardV1, keymap: &PhysicalKeymap) {
    keyboard.keymap(NO_KEYMAP, keymap.file.as_fd(), 0);
}

/// RDP (and Windows) report wheel rotation in WHEEL_DELTA units per notch.
const WHEEL_DELTA: i32 = 120;
/// libinput reports 15 logical pixels per wheel notch; match local scrolling.
const WHEEL_NOTCH_PIXELS: f64 = 15.0;

/// One axis update for the virtual pointer: the continuous scroll distance and
/// the whole notches completed by it, which Niri receives as `discrete * 120`.
#[derive(Debug, PartialEq)]
struct WheelStep {
    value: f64,
    discrete: i32,
}

/// Per-axis remainder of RDP wheel units that has not yet completed a notch.
/// High-resolution wheels and touchpads send fractions of WHEEL_DELTA; the
/// distance is forwarded immediately, while the notch is reported only once
/// the accumulated rotation reaches it, so wheel binds see whole steps.
#[derive(Debug, Default)]
pub(super) struct WheelAccumulator {
    /// Index 0 is vertical, index 1 is horizontal, in Wayland directions.
    pending: [i32; 2],
}
impl WheelAccumulator {
    fn push(&mut self, axis: usize, units: i32) -> WheelStep {
        let pending = &mut self.pending[axis];
        // A reversal starts a new notch; stale rotation must not cancel it.
        if pending.signum() * units.signum() < 0 {
            *pending = 0;
        }
        *pending = pending.saturating_add(units);
        let discrete = *pending / WHEEL_DELTA;
        *pending -= discrete * WHEEL_DELTA;
        WheelStep {
            value: f64::from(units) * WHEEL_NOTCH_PIXELS / f64::from(WHEEL_DELTA),
            discrete,
        }
    }
    fn reset(&mut self) {
        self.pending = [0; 2];
    }
}

pub(super) struct Input {
    pub(super) keyboard: ZwpVirtualKeyboardV1,
    pub(super) text_keyboard: ZwpVirtualKeyboardV1,
    pub(super) physical_keymap: PhysicalKeymap,
    pub(super) unicode: UnicodeInput,
    pub(super) pointer: ZwlrVirtualPointerV1,
    pub(super) keys: BTreeSet<u32>,
    pub(super) buttons: BTreeSet<u32>,
    pub(super) point: (u32, u32),
    pub(super) size: (u32, u32),
    pub(super) target: DesktopSize,
    pub(super) scale: f64,
    pub(super) wheel: WheelAccumulator,
    pub(super) started: Instant,
}
impl Input {
    pub(super) fn update_keymap(&mut self, s: &mut State) -> anyhow::Result<()> {
        if let Some(error) = s.keymap_error.take() {
            anyhow::bail!(error);
        }
        if let Some(text) = s.seat_keymap.take()
            && text != self.physical_keymap.text
        {
            let mut next = PhysicalKeymap::new(text)?;
            // Reapply held keys when the user reloads the desktop layout.
            for key in &self.keys {
                next.state
                    .update_key((key + 8).into(), xkb::KeyDirection::Down);
            }
            let old = &self.physical_keymap.state;
            let old_map = old.get_keymap();
            let next_map = next.state.get_keymap();
            let old_locked = old.serialize_mods(xkb::STATE_MODS_LOCKED);
            let mut locked = 0;
            for index in 0..old_map.num_mods().min(32) {
                if old_locked & (1 << index) != 0 {
                    let next_index = next_map.mod_get_index(old_map.mod_get_name(index));
                    if next_index < 32 {
                        locked |= 1 << next_index;
                    }
                }
            }
            next.state.update_mask(
                next.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                0,
                locked,
                0,
                0,
                0,
            );
            self.physical_keymap = next;
        }
        Ok(())
    }
    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }
    fn modifiers(&self) {
        self.keyboard.modifiers(
            self.physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_LATCHED),
            self.physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_LOCKED),
            self.physical_keymap
                .state
                .serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        );
    }
    fn key(&mut self, key: u32, pressed: bool) {
        let changed = if pressed {
            self.keys.insert(key)
        } else {
            self.keys.remove(&key)
        };
        if changed {
            self.keyboard.key(self.time(), key, u32::from(pressed));
            self.physical_keymap.state.update_key(
                (key + 8).into(),
                if pressed {
                    xkb::KeyDirection::Down
                } else {
                    xkb::KeyDirection::Up
                },
            );
            // No modifier sync here: Niri runs these keys through the seat's
            // XKB state, and the bridge's group could undo a `switch-layout`
            // bind. Synchronize and committed text still send masks.
        }
    }
    pub(super) fn key_event(&mut self, e: KeyboardEvent) {
        match e {
            KeyboardEvent::Pressed { code, extended } => {
                self.unicode = UnicodeInput::default();
                if let Some(k) = evdev_key(code, extended) {
                    self.key(k, true)
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(k) = evdev_key(code, extended) {
                    self.key(k, false)
                }
            }
            KeyboardEvent::Synchronize(flags) => {
                let km = self.physical_keymap.state.get_keymap();
                let mut locked = 0;
                // RDP synchronize bits: Scroll=1, Num=2, Caps=4, Kana=8.
                for (bit, name) in [(1, "ScrollLock"), (2, "NumLock"), (4, "Lock")] {
                    let i = km.mod_get_index(name);
                    if flags.bits() & bit != 0 && i < 32 {
                        locked |= 1 << i;
                    }
                }
                self.physical_keymap.state.update_mask(
                    self.physical_keymap
                        .state
                        .serialize_mods(xkb::STATE_MODS_DEPRESSED),
                    0,
                    locked,
                    0,
                    0,
                    0,
                );
                self.modifiers();
            }
            KeyboardEvent::UnicodePressed(unit) => {
                if let Some(character) = self.unicode.press(unit)
                    && let Err(error) = self.text(character)
                {
                    eprintln!("cannot inject Unicode keyboard input: {error}");
                }
            }
            // Unicode input describes committed text, rather than a held physical
            // key. Each completed scalar is sent as a balanced tap immediately.
            KeyboardEvent::UnicodeReleased(_) => {}
        }
    }
    fn text(&self, character: char) -> anyhow::Result<()> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_string(
            &ctx,
            unicode_keymap(character),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("Unicode XKB keymap unavailable"))?;
        let mut file = tempfile::tempfile()?;
        let mut text = km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        text.push('\0');
        file.write_all(text.as_bytes())?;
        self.text_keyboard
            .keymap(1, file.as_fd(), text.len() as u32);
        self.text_keyboard.modifiers(0, 0, 0, 0);
        self.text_keyboard.key(self.time(), 247, 1);
        self.text_keyboard.key(self.time(), 247, 0);
        // Committed Unicode uses this device's temporary map without changing
        // the seat. Restore its map and masks for subsequent protocol events.
        self.text_keyboard.keymap(
            1,
            self.physical_keymap.file.as_fd(),
            self.physical_keymap.size,
        );
        self.modifiers();
        Ok(())
    }
    fn move_to(&mut self, x: u32, y: u32) {
        let x = x.min(u32::from(self.target.width) - 1);
        let y = y.min(u32::from(self.target.height) - 1);
        self.point = (x, y);
        self.pointer.motion_absolute(
            self.time(),
            x * self.size.0 / u32::from(self.target.width),
            y * self.size.1 / u32::from(self.target.height),
            self.size.0,
            self.size.1,
        );
    }
    pub(super) fn relative(&mut self, x: i32, y: i32) {
        let x = x.clamp(-i32::from(self.target.width), i32::from(self.target.width));
        let y = y.clamp(
            -i32::from(self.target.height),
            i32::from(self.target.height),
        );
        self.point = (
            (i64::from(self.point.0) + i64::from(x)).clamp(0, i64::from(self.target.width) - 1)
                as u32,
            (i64::from(self.point.1) + i64::from(y)).clamp(0, i64::from(self.target.height) - 1)
                as u32,
        );
        // Relative input follows the compositor's actual pointer, including
        // application warps/grabs. RDP deltas are physical pixels; Wayland's
        // relative motion is in compositor logical coordinates.
        self.pointer.motion(
            self.time(),
            f64::from(x) / self.scale,
            f64::from(y) / self.scale,
        );
    }

    fn button(&mut self, b: MouseButton, down: bool) {
        let code = match b {
            MouseButton::Left => 272,
            MouseButton::Right => 273,
            MouseButton::Middle => 274,
            MouseButton::X1 => 275,
            MouseButton::X2 => 276,
            _ => return,
        };
        let changed = if down {
            self.buttons.insert(code)
        } else {
            self.buttons.remove(&code)
        };
        if changed {
            self.pointer.button(
                self.time(),
                code,
                if down {
                    wl_seat_button_pressed()
                } else {
                    wayland_client::protocol::wl_pointer::ButtonState::Released
                },
            );
        }
    }
    /// Scroll by RDP wheel units (WHEEL_DELTA per notch) in Wayland directions.
    /// Completed notches use axis_discrete, which Niri turns into v120 for
    /// wheel binds, overview/MRU navigation and clients' axis_value120.
    fn scroll(&mut self, x: i32, y: i32) {
        use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
        self.pointer.axis_source(AxisSource::Wheel);
        for (axis, index, units) in [(Axis::VerticalScroll, 0, y), (Axis::HorizontalScroll, 1, x)] {
            if units == 0 {
                continue;
            }
            let step = self.wheel.push(index, units);
            if step.discrete != 0 {
                self.pointer
                    .axis_discrete(self.time(), axis, step.value, step.discrete);
            } else {
                self.pointer.axis(self.time(), axis, step.value);
            }
        }
    }
    pub(super) fn mouse_event(&mut self, e: MouseEvent) {
        match e {
            MouseEvent::Move { x, y } => self.move_to(x.into(), y.into()),
            MouseEvent::RelMove { x, y } => self.relative(x, y),
            MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                self.move_to(x.into(), y.into());
                self.button(button, pressed);
            }
            MouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                self.relative(x, y);
                self.button(button, pressed);
            }
            // RDP positive vertical rotation is away from the user (up).
            MouseEvent::VerticalScroll { value } => self.scroll(0, -i32::from(value)),
            MouseEvent::HorizontalScroll { value } => self.scroll(i32::from(value), 0),
            // FreeRDP's ainput carries the same signed rotation units in x/y.
            // Bound them like the core PDU so Niri's `discrete * 120` stays small.
            MouseEvent::Scroll { x, y } => self.scroll(
                x.clamp(-i32::from(i16::MAX), i32::from(i16::MAX)),
                -y.clamp(-i32::from(i16::MAX), i32::from(i16::MAX)),
            ),
            _ => {}
        }
        self.pointer.frame();
    }
    pub(super) fn release(&mut self) {
        self.unicode = UnicodeInput::default();
        // Disconnect or focus loss must not carry a partial notch forward.
        self.wheel.reset();
        for k in self.keys.clone() {
            self.key(k, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.pointer.button(
                self.time(),
                b,
                wayland_client::protocol::wl_pointer::ButtonState::Released,
            );
        }
        self.pointer.frame();
    }
}
fn wl_seat_button_pressed() -> wayland_client::protocol::wl_pointer::ButtonState {
    wayland_client::protocol::wl_pointer::ButtonState::Pressed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixStream, time::Duration};
    use wayland_client::{Connection, Proxy, protocol::wl_seat};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
    #[test]
    fn jp106_keymap_preserves_jis_punctuation_and_extra_keys() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "jp106",
            "jp",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let inherited = PhysicalKeymap::new(km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
        let mut state = inherited.state;
        for (evdev, character) in [
            (26u32, "@"),
            (27, "["),
            (39, ";"),
            (40, ":"),
            (43, "]"),
            (89, "\\"),
            (124, "\\"),
        ] {
            assert_eq!(state.key_get_utf8((evdev + 8).into()), character);
        }
        state.update_key(50u32.into(), xkb::KeyDirection::Down);
        for (evdev, character) in [(3u32, "\""), (89, "_"), (124, "|")] {
            assert_eq!(state.key_get_utf8((evdev + 8).into()), character);
        }
    }
    #[test]
    fn inherited_compositor_keymaps_preserve_swedish_us_and_multiple_groups() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for (layout, expected) in [
            ("se", [(26u32, "å"), (39, "ö"), (40, "ä")]),
            ("us", [(26u32, "["), (39, ";"), (40, "'")]),
        ] {
            let map = xkb::Keymap::new_from_names(
                &context,
                "evdev",
                "pc105",
                layout,
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let inherited =
                PhysicalKeymap::new(map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
            let text = read_keymap(inherited.file.try_clone().unwrap(), inherited.size).unwrap();
            let restored = PhysicalKeymap::new(text).unwrap();
            for (key, character) in expected {
                assert_eq!(restored.state.key_get_utf8((key + 8).into()), character);
            }
        }
        let map = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us,se",
            "",
            Some("grp:alt_shift_toggle".into()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut inherited =
            PhysicalKeymap::new(map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap();
        assert_eq!(inherited.state.get_keymap().num_layouts(), 2);
        inherited.state.update_mask(0, 0, 0, 0, 0, 1);
        assert_eq!(inherited.state.key_get_utf8(34u32.into()), "å");
        inherited.state.update_mask(0, 0, 0, 0, 0, 0);
        assert_eq!(inherited.state.key_get_utf8(34u32.into()), "[");
    }
    #[test]
    fn compositor_keymap_reader_rejects_truncated_and_unbounded_data() {
        for (bytes, size) in [
            (b"abc".as_slice(), 4),
            (b"abc".as_slice(), 3),
            (b"abc\0".as_slice(), 1024 * 1024 + 1),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            assert!(read_keymap(file, size).is_err());
        }
    }
    #[test]
    fn unicode_input_decodes_utf16_and_recovers_from_malformed_pairs() {
        let mut input = UnicodeInput::default();
        assert_eq!(input.press(0x3042), Some('あ'));
        assert_eq!(input.press(0xd83d), None);
        assert_eq!(input.press(0xde00), Some('😀'));
        assert_eq!(input.press(0xde00), None);
        assert_eq!(input.press(0xd83d), None);
        assert_eq!(input.press(b'a' as u16), Some('a'));
        assert_eq!(input.press(0xde00), None);
        assert_eq!(input.press(0xd800), None);
        assert_eq!(input.press(0xd83d), None);
        assert_eq!(input.press(0xde00), Some('😀'));
        assert_eq!(input.press(0), None);
    }
    #[test]
    fn unicode_keymap_produces_exact_text_without_modifiers() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for character in ['a', 'A', '@', 'あ', '漢', '😀'] {
            let km = xkb::Keymap::new_from_string(
                &context,
                unicode_keymap(character),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            assert!(
                km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
                    .contains("xkb_keycodes \"niri_rdp_unicode\"")
            );
            let state = xkb::State::new(&km);
            assert_eq!(state.key_get_utf8(255u32.into()), character.to_string());
            assert_eq!(state.serialize_mods(xkb::STATE_MODS_EFFECTIVE), 0);
        }
        for (character, symbol) in [
            ('\t', "Tab"),
            ('\r', "Return"),
            ('\n', "Return"),
            ('\u{8}', "BackSpace"),
            ('\u{1b}', "Escape"),
            ('\u{7f}', "Delete"),
        ] {
            let km = xkb::Keymap::new_from_string(
                &context,
                unicode_keymap(character),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            let state = xkb::State::new(&km);
            assert_eq!(
                xkb::keysym_get_name(state.key_get_one_sym(255u32.into())),
                symbol
            );
        }
    }
    #[test]
    fn wheel_accumulator_reports_whole_notches_and_forwards_distance() {
        let mut wheel = WheelAccumulator::default();
        let step = |value: f64, discrete: i32| WheelStep { value, discrete };
        // One notch is one discrete step at libinput's 15 logical pixels.
        assert_eq!(wheel.push(0, 120), step(15.0, 1));
        assert_eq!(wheel.push(0, -120), step(-15.0, -1));
        // High-resolution halves move immediately; the second completes it.
        assert_eq!(wheel.push(0, 60), step(7.5, 0));
        assert_eq!(wheel.push(0, 60), step(7.5, 1));
        assert_eq!(wheel.pending, [0, 0]);
        // Several notches and remainders in one event.
        assert_eq!(wheel.push(0, 240), step(30.0, 2));
        assert_eq!(wheel.push(0, 200), step(25.0, 1));
        assert_eq!(wheel.pending[0], 80);
        assert_eq!(wheel.push(0, 40), step(5.0, 1));
        // Axes accumulate independently.
        assert_eq!(wheel.push(1, 90), step(11.25, 0));
        assert_eq!(wheel.push(0, 90), step(11.25, 0));
        assert_eq!(wheel.pending, [90, 90]);
        // A reversal drops the stale partial notch instead of cancelling it.
        assert_eq!(wheel.push(0, -30), step(-3.75, 0));
        assert_eq!(wheel.pending, [-30, 90]);
        assert_eq!(wheel.push(0, -90), step(-11.25, -1));
        assert_eq!(wheel.push(1, 30), step(3.75, 1));
        assert_eq!(wheel.pending, [0, 0]);
        // Disconnect/release cleanup forgets partial notches on both axes.
        wheel.push(0, 100);
        wheel.push(1, -100);
        wheel.reset();
        assert_eq!(wheel.push(0, 20), step(2.5, 0));
        assert_eq!(wheel.push(1, -20), step(-2.5, 0));
        // Extreme input saturates rather than overflowing.
        let mut wheel = WheelAccumulator {
            pending: [119, -119],
        };
        assert_eq!(wheel.push(0, i32::MAX).discrete, i32::MAX / 120);
        assert_eq!(wheel.push(1, i32::MIN).discrete, i32::MIN / 120);
    }
    /// An `Input` whose requests reach `compositor` as raw wire bytes; the
    /// setup requests are already drained.
    fn wired_input() -> (UnixStream, Connection, Input) {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        compositor
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let events = connection.new_event_queue::<State>();
        let handle = events.handle();
        let registry = connection.display().get_registry(&handle, ());
        let seat: wl_seat::WlSeat = registry.bind(1, 7, &handle, ());
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = registry.bind(2, 1, &handle, ());
        let pointer_manager: ZwlrVirtualPointerManagerV1 = registry.bind(3, 2, &handle, ());
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let input = Input {
            keyboard: keyboard_manager.create_virtual_keyboard(&seat, &handle, ()),
            text_keyboard: keyboard_manager.create_virtual_keyboard(&seat, &handle, ()),
            physical_keymap: PhysicalKeymap::new(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
                .unwrap(),
            unicode: UnicodeInput::default(),
            pointer: pointer_manager.create_virtual_pointer(Some(&seat), &handle, ()),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            point: (0, 0),
            size: (640, 480),
            target: DesktopSize {
                width: 640,
                height: 480,
            },
            scale: 1.0,
            wheel: WheelAccumulator::default(),
            started: Instant::now(),
        };
        connection.flush().unwrap();
        let mut wire = [0u8; 4096];
        assert!(compositor.read(&mut wire).unwrap() > 0);
        (compositor, connection, input)
    }
    /// Flush and split the requests into (object, opcode, argument words).
    fn requests(connection: &Connection, compositor: &mut UnixStream) -> Vec<(u32, u16, Vec<u32>)> {
        connection.flush().unwrap();
        let mut wire = vec![0u8; 1 << 16];
        let size = compositor.read(&mut wire).unwrap();
        let word = |at: usize| u32::from_ne_bytes(wire[at..at + 4].try_into().unwrap());
        let mut requests = Vec::new();
        let mut offset = 0;
        while offset + 8 <= size {
            let header = word(offset + 4);
            let length = (header >> 16) as usize;
            let arguments = (offset + 8..offset + length).step_by(4).map(word).collect();
            requests.push((word(offset), (header & 0xffff) as u16, arguments));
            offset += length;
        }
        requests
    }
    // zwp_virtual_keyboard_v1 request opcodes.
    const KEYMAP: u16 = 0;
    const KEY: u16 = 1;
    const MODIFIERS: u16 = 2;
    #[test]
    fn physical_keyboard_follows_the_seat_keymap() {
        let (mut compositor, connection, input) = wired_input();
        follow_seat_keymap(&input.keyboard, &input.physical_keymap);
        let keyboard = input.keyboard.id().protocol_id();
        // keymap(format = no_keymap, fd in ancillary data, size = 0)
        assert_eq!(
            requests(&connection, &mut compositor),
            vec![(keyboard, KEYMAP, vec![0, 0])]
        );
    }
    #[test]
    fn key_events_send_no_modifier_sync() {
        let (mut compositor, connection, mut input) = wired_input();
        for event in [
            // Shift, a, release a, release Shift.
            KeyboardEvent::Pressed {
                code: 0x2a,
                extended: false,
            },
            KeyboardEvent::Pressed {
                code: 0x1e,
                extended: false,
            },
            KeyboardEvent::Released {
                code: 0x1e,
                extended: false,
            },
            KeyboardEvent::Released {
                code: 0x2a,
                extended: false,
            },
        ] {
            input.key_event(event);
        }
        // Disconnect cleanup releases held keys without a sync either.
        input.key_event(KeyboardEvent::Pressed {
            code: 0x1d,
            extended: false,
        });
        input.release();
        let keyboard = input.keyboard.id().protocol_id();
        let sent: Vec<_> = requests(&connection, &mut compositor)
            .into_iter()
            .filter(|(object, _, _)| *object == keyboard)
            .map(|(_, opcode, arguments)| {
                assert_eq!(opcode, KEY, "only key requests reach the keyboard");
                (arguments[1], arguments[2])
            })
            .collect();
        assert_eq!(
            sent,
            vec![(42, 1), (30, 1), (30, 0), (42, 0), (29, 1), (29, 0)]
        );
        // The bridge still tracks the modifiers for later synchronization.
        assert_eq!(
            input
                .physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_DEPRESSED),
            0
        );
    }
    #[test]
    fn synchronize_and_unicode_text_still_sync_modifiers() {
        let (mut compositor, connection, mut input) = wired_input();
        // Caps Lock and Num Lock on.
        input.key_event(KeyboardEvent::Synchronize(
            ironrdp_pdu::input::fast_path::SynchronizeFlags::from_bits_truncate(2 | 4),
        ));
        let keyboard = input.keyboard.id().protocol_id();
        let text = input.text_keyboard.id().protocol_id();
        // Caps Lock is the real Lock modifier; Num Lock serializes as Mod2.
        let locked = (1u32 << 1) | (1 << 4);
        assert_eq!(
            input
                .physical_keymap
                .state
                .serialize_mods(xkb::STATE_MODS_LOCKED),
            locked
        );
        assert_eq!(
            requests(&connection, &mut compositor),
            vec![(keyboard, MODIFIERS, vec![0, 0, locked, 0])]
        );
        input.key_event(KeyboardEvent::UnicodePressed(0x3042));
        let sent: Vec<_> = requests(&connection, &mut compositor)
            .into_iter()
            .map(|(object, opcode, _)| (object, opcode))
            .collect();
        assert_eq!(
            sent,
            vec![
                (text, KEYMAP),
                (text, MODIFIERS),
                (text, KEY),
                (text, KEY),
                (text, KEYMAP),
                (keyboard, MODIFIERS),
            ]
        );
    }
    #[test]
    fn rdp_wheel_reaches_virtual_pointer_as_discrete_steps() {
        let (mut compositor, connection, mut input) = wired_input();
        let mut wire = [0u8; 4096];
        for event in [
            // RDP positive vertical rotation scrolls up (negative in Wayland).
            MouseEvent::VerticalScroll { value: 60 },
            MouseEvent::VerticalScroll { value: 60 },
            MouseEvent::VerticalScroll { value: -120 },
            MouseEvent::HorizontalScroll { value: 90 },
            // FreeRDP ainput wheel uses the same rotation units.
            MouseEvent::Scroll { x: 0, y: -240 },
        ] {
            input.mouse_event(event);
        }
        // Disconnect cleanup drops the pending 90 horizontal units.
        input.release();
        input.mouse_event(MouseEvent::HorizontalScroll { value: 30 });
        connection.flush().unwrap();
        let size = compositor.read(&mut wire).unwrap();
        let pointer = input.pointer.id().protocol_id();
        let word = |at: usize| i32::from_ne_bytes(wire[at..at + 4].try_into().unwrap());
        let mut axis_events = Vec::new();
        let mut offset = 0;
        while offset + 8 <= size {
            let object = word(offset) as u32;
            let header = word(offset + 4) as u32;
            if object == pointer {
                match header & 0xffff {
                    // axis(time, axis, value)
                    3 => axis_events.push((word(offset + 12), word(offset + 16), None)),
                    // axis_discrete(time, axis, value, discrete)
                    7 => axis_events.push((
                        word(offset + 12),
                        word(offset + 16),
                        Some(word(offset + 20)),
                    )),
                    _ => {}
                }
            }
            offset += (header >> 16) as usize;
        }
        // (axis: 0 vertical / 1 horizontal, wl_fixed value, discrete steps)
        assert_eq!(
            axis_events,
            vec![
                (0, -1920, None),
                (0, -1920, Some(-1)),
                (0, 3840, Some(1)),
                (1, 2880, None),
                (0, 7680, Some(2)),
                (1, 960, None),
            ]
        );
    }
}
