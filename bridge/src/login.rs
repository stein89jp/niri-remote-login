//! Restricted TLS login screen. No Wayland connection exists before PAM accepts.
use crate::{bitmap, broker, video::Video, wayland::Desktop};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BoundConnection, DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent,
    RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

// Avoid retaining submitted passwords after a failed attempt or disconnect.
struct Password(String);
impl Default for Password {
    fn default() -> Self {
        Self(String::with_capacity(512))
    }
}
impl Password {
    fn clear(&mut self) {
        // Zero bytes remain valid UTF-8 and are immediately removed.
        unsafe { self.0.as_bytes_mut().fill(0) };
        self.0.clear();
    }
}
impl Drop for Password {
    fn drop(&mut self) {
        self.clear();
    }
}
struct State {
    size: DesktopSize,
    username: String,
    password: Password,
    password_focus: bool,
    shift: [bool; 2],
    caps: bool,
    unicode: crate::keyboard::UnicodeInput,
    synchronize: Option<KeyboardEvent>,
    control: [bool; 2],
    alt: [bool; 2],
    busy: bool,
    failed: bool,
    attempts: u8,
    retry_at: Instant,
    stopped: bool,
    desktop: Option<Desktop>,
}
#[derive(Clone)]
pub struct Login {
    state: Arc<Mutex<State>>,
    changed: tokio::sync::watch::Sender<u64>,
    path: PathBuf,
    output: String,
    video: Option<Video>,
}
impl Login {
    pub fn new(
        path: PathBuf,
        output: String,
        video: Option<Video>,
        size: DesktopSize,
        username: &str,
    ) -> Self {
        let (changed, _) = tokio::sync::watch::channel(0);
        let username = if username.len() <= 64 && !username.chars().any(char::is_control) {
            username.to_owned()
        } else {
            String::new()
        };
        Self {
            state: Arc::new(Mutex::new(State {
                size,
                password_focus: !username.is_empty(),
                username,
                password: Password::default(),
                shift: [false; 2],
                caps: false,
                unicode: Default::default(),
                synchronize: None,
                control: [false; 2],
                alt: [false; 2],
                busy: false,
                failed: false,
                attempts: 0,
                retry_at: Instant::now(),
                stopped: false,
                desktop: None,
            })),
            changed,
            path,
            output,
            video,
        }
    }
    pub fn bound(&self) -> BoundConnection {
        BoundConnection {
            display: Box::new(self.clone()),
            input: Box::new(self.clone()),
        }
    }
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        state.password.clear();
        if let Some(desktop) = state.desktop.take() {
            desktop.stop();
        }
        drop(state);
        self.notify();
    }
    fn notify(&self) {
        self.changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
    fn submit(&self) {
        let (username, password, size) = {
            let mut state = self.state.lock().unwrap();
            if state.stopped
                || state.busy
                || state.desktop.is_some()
                || Instant::now() < state.retry_at
            {
                return;
            }
            if state.username.is_empty() || state.password.0.is_empty() {
                state.failed = true;
                drop(state);
                self.notify();
                return;
            }
            state.busy = true;
            state.failed = false;
            state.attempts += 1;
            let password = std::mem::take(&mut state.password);
            (state.username.clone(), password, state.size)
        };
        self.notify();
        let state = Arc::downgrade(&self.state);
        let path = self.path.clone();
        let output = self.output.clone();
        let changed = self.changed.clone();
        // PAM and compositor startup may block; neither runs in the RDP loop.
        std::thread::spawn(move || {
            let result = broker::open(&path, &username, &password.0).and_then(|session| {
                let Some(session) = session else {
                    return Ok(None);
                };
                if state
                    .upgrade()
                    .is_none_or(|state| state.lock().unwrap().stopped)
                {
                    return Ok(None);
                }
                Desktop::start(session.socket, output, size, Some(session.lease)).map(Some)
            });
            drop(password);
            if let Some(state) = state.upgrade() {
                let mut state = state.lock().unwrap();
                state.busy = false;
                match result {
                    Ok(Some(mut desktop)) if !state.stopped => {
                        // Preserve lock state and modifiers held while signing in.
                        let synchronized_caps =
                            if let Some(KeyboardEvent::Synchronize(flags)) = &state.synchronize {
                                desktop.keyboard(KeyboardEvent::Synchronize(*flags));
                                flags.bits() & 4 != 0
                            } else {
                                false
                            };
                        if state.caps != synchronized_caps {
                            desktop.keyboard(KeyboardEvent::Pressed {
                                code: 58,
                                extended: false,
                            });
                            desktop.keyboard(KeyboardEvent::Released {
                                code: 58,
                                extended: false,
                            });
                        }
                        for (held, code, extended) in [
                            (state.shift[0], 42, false),
                            (state.shift[1], 54, false),
                            (state.control[0], 29, false),
                            (state.control[1], 29, true),
                            (state.alt[0], 56, false),
                            (state.alt[1], 56, true),
                        ] {
                            if held {
                                desktop.keyboard(KeyboardEvent::Pressed { code, extended });
                            }
                        }
                        state.desktop = Some(desktop);
                    }
                    Ok(Some(desktop)) => desktop.stop(),
                    _ => {
                        state.failed = true;
                        state.retry_at = Instant::now() + Duration::from_secs(2);
                        if state.attempts >= 5 {
                            state.stopped = true;
                        }
                    }
                }
                drop(state);
                changed.send_modify(|generation| *generation = generation.wrapping_add(1));
            }
        });
    }
    fn character(&self, character: char) {
        if character.is_control() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.stopped || state.busy {
            return;
        }
        if state.password_focus {
            if state.password.0.len() + character.len_utf8() <= 512 {
                state.password.0.push(character);
            }
        } else if state.username.len() + character.len_utf8() <= 64 {
            state.username.push(character);
        }
        state.failed = false;
        drop(state);
        self.notify();
    }
}
impl RdpServerInputHandler for Login {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let desktop = self.state.lock().unwrap().desktop.clone();
        if let Some(mut desktop) = desktop {
            desktop.keyboard(event);
            return;
        }
        if let KeyboardEvent::Pressed { code, extended }
        | KeyboardEvent::Released { code, extended } = &event
        {
            if matches!(event, KeyboardEvent::Pressed { .. }) {
                self.state.lock().unwrap().unicode = Default::default();
            }
            if *code == 29 || *code == 56 {
                let down = matches!(event, KeyboardEvent::Pressed { .. });
                let mut state = self.state.lock().unwrap();
                if *code == 29 {
                    state.control[usize::from(*extended)] = down;
                } else {
                    state.alt[usize::from(*extended)] = down;
                }
                return;
            }
        }
        let (code, down) = match event {
            KeyboardEvent::Pressed {
                code,
                extended: false,
            } => (code, true),
            KeyboardEvent::Released {
                code,
                extended: false,
            } => (code, false),
            KeyboardEvent::UnicodePressed(value) => {
                let character = self.state.lock().unwrap().unicode.press(value);
                if let Some(character) = character {
                    match character {
                        '\r' | '\n' => self.submit(),
                        '\t' => self.keyboard(KeyboardEvent::Pressed {
                            code: 15,
                            extended: false,
                        }),
                        '\u{8}' => self.keyboard(KeyboardEvent::Pressed {
                            code: 14,
                            extended: false,
                        }),
                        _ => self.character(character),
                    }
                }
                return;
            }
            KeyboardEvent::Synchronize(flags) => {
                // CAPS_LOCK is bit 2 in the RDP synchronize flags.
                let mut state = self.state.lock().unwrap();
                state.caps = flags.bits() & 4 != 0;
                state.synchronize = Some(KeyboardEvent::Synchronize(flags));
                return;
            }
            _ => return,
        };
        if code == 42 || code == 54 {
            self.state.lock().unwrap().shift[usize::from(code == 54)] = down;
            return;
        }
        if !down {
            return;
        }
        if {
            let state = self.state.lock().unwrap();
            state
                .control
                .iter()
                .chain(state.alt.iter())
                .any(|held| *held)
        } {
            return;
        }
        if code == 28 {
            self.submit();
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.busy || state.stopped {
            return;
        }
        match code {
            15 => state.password_focus = !state.password_focus,
            14 => {
                if state.password_focus {
                    // Overwrite the removed character's bytes before truncating.
                    if let Some((index, _)) = state.password.0.char_indices().next_back() {
                        unsafe { state.password.0.as_bytes_mut()[index..].fill(0) };
                        state.password.0.truncate(index);
                    }
                } else {
                    state.username.pop();
                }
            }
            58 => state.caps = !state.caps,
            _ => {
                let character =
                    jp_character(code, state.shift.iter().any(|shift| *shift), state.caps);
                drop(state);
                if let Some(character) = character {
                    self.character(character);
                }
                return;
            }
        }
        drop(state);
        self.notify();
    }
    fn mouse(&mut self, event: MouseEvent) {
        let desktop = self.state.lock().unwrap().desktop.clone();
        if let Some(mut desktop) = desktop {
            desktop.mouse(event);
            return;
        }
        if let MouseEvent::Button {
            x,
            y,
            button: MouseButton::Left,
            pressed: true,
        } = event
        {
            let mut state = self.state.lock().unwrap();
            let (left, top, width) = form(state.size);
            if i32::from(x) < left || i32::from(x) > left + width {
                return;
            }
            let y = i32::from(y) - top;
            if (60..92).contains(&y) {
                state.password_focus = false;
            }
            if (122..154).contains(&y) {
                state.password_focus = true;
            }
            drop(state);
            self.notify();
            if (172..206).contains(&y) {
                self.submit();
            }
        }
    }
}
#[async_trait::async_trait]
impl RdpServerDisplay for Login {
    async fn size(&mut self) -> DesktopSize {
        let desktop = self.state.lock().unwrap().desktop.clone();
        match desktop {
            Some(mut desktop) => desktop.size().await,
            None => self.state.lock().unwrap().size,
        }
    }
    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let updates = Box::new(Updates {
            login: self.clone(),
            rx: self.changed.subscribe(),
            first: true,
            desktop: None,
            repaint: tokio::time::Instant::now() + Duration::from_secs(1),
        });
        // Use the negotiated graphics channel for the form as well as the
        // desktop. mstsc may stop presenting classic bitmaps once GFX opens.
        match &self.video {
            Some(video) => Ok(video.wrap_updates(updates)?),
            None => Ok(updates),
        }
    }
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        if let Some(mut desktop) = self.state.lock().unwrap().desktop.clone() {
            desktop.request_layout(layout);
        }
        // Keep the initial negotiated size while the login form is active.
    }
}
struct Updates {
    login: Login,
    rx: tokio::sync::watch::Receiver<u64>,
    first: bool,
    desktop: Option<Box<dyn RdpServerDisplayUpdates + Send>>,
    repaint: tokio::time::Instant,
}
#[async_trait::async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        loop {
            if self.login.state.lock().unwrap().stopped {
                return Ok(None);
            }
            if let Some(desktop) = &mut self.desktop {
                return desktop.next_update().await;
            }
            let desktop = self.login.state.lock().unwrap().desktop.clone();
            if let Some(desktop) = desktop {
                self.desktop = Some(desktop.updates_send());
                continue;
            }
            if self.first || self.rx.has_changed().unwrap_or(false) {
                self.first = false;
                self.rx.borrow_and_update();
                self.repaint = tokio::time::Instant::now() + Duration::from_secs(1);
                return Ok(Some(DisplayUpdate::Bitmap(
                    render(&self.login.state.lock().unwrap())
                        .map_err(|_| crate::server_error("login framebuffer dimensions invalid"))?,
                )));
            }
            // A client can initialize its surface or negotiate GFX after the
            // first bitmap, and refresh requests do not produce input events.
            // Keep a heartbeat so a static login form can recover as well.
            tokio::select! {
                changed = self.rx.changed() => {
                    if changed.is_err() {
                        return Ok(None);
                    }
                }
                _ = tokio::time::sleep_until(self.repaint) => {}
            }
            self.first = true;
        }
    }
}
fn jp_character(code: u8, shift: bool, caps: bool) -> Option<char> {
    let (normal, shifted): (&[u8], &[u8]) = match code {
        2..=13 => (b"1234567890-^", b"!\"#$%&'()~=~"),
        16..=27 => (b"qwertyuiop@[", b"QWERTYUIOP`{"),
        30..=40 => (b"asdfghjkl;:", b"ASDFGHJKL+*"),
        43 => (b"]".as_slice(), b"}".as_slice()),
        0x73 => return Some(if shift { '_' } else { '\\' }),
        0x7d => return Some(if shift { '|' } else { '\\' }),
        44..=53 => (b"zxcvbnm,./".as_slice(), b"ZXCVBNM<>?".as_slice()),
        57 => return Some(' '),
        _ => return None,
    };
    let base = match code {
        2..=13 => 2,
        16..=27 => 16,
        30..=40 => 30,
        43 => 43,
        _ => 44,
    };
    let index = usize::from(code - base);
    let uppercase = shift ^ (caps && normal[index].is_ascii_alphabetic());
    Some(char::from(if uppercase {
        shifted[index]
    } else {
        normal[index]
    }))
}
fn form(size: DesktopSize) -> (i32, i32, i32) {
    let width = 420.min(i32::from(size.width).saturating_sub(20)).max(1);
    (
        (i32::from(size.width) - width) / 2,
        (i32::from(size.height) - 260).max(0) / 2,
        width,
    )
}
fn render(state: &State) -> anyhow::Result<ironrdp_server::BitmapUpdate> {
    let size = state.size;
    let mut pixels = vec![0; usize::from(size.width) * usize::from(size.height) * 4];
    let (left, top, width) = form(size);
    let mut canvas = Canvas {
        pixels: &mut pixels,
        size,
    };
    canvas.rect(
        0,
        0,
        i32::from(size.width),
        i32::from(size.height),
        [35, 27, 22],
    );
    canvas.text(left, top, "Niri remote login", [240, 240, 240]);
    canvas.text(left, top + 40, "Linux username", [200, 200, 200]);
    canvas.rect(
        left,
        top + 60,
        width,
        32,
        if !state.password_focus {
            [100, 80, 40]
        } else {
            [65, 55, 45]
        },
    );
    canvas.text(left + 8, top + 68, &state.username, [255, 255, 255]);
    canvas.text(left, top + 102, "Password", [200, 200, 200]);
    canvas.rect(
        left,
        top + 122,
        width,
        32,
        if state.failed {
            [80, 80, 140]
        } else if state.password_focus {
            [100, 80, 40]
        } else {
            [65, 55, 45]
        },
    );
    // Always render the same mask: password text and length stay off the wire.
    canvas.text(
        left + 8,
        top + 130,
        if state.password.0.is_empty() {
            ""
        } else {
            "********"
        },
        [255, 255, 255],
    );
    canvas.rect(left, top + 172, width, 34, [140, 95, 40]);
    canvas.text(
        left + 8,
        top + 182,
        if state.busy {
            "Signing in..."
        } else {
            "Sign in (Enter)"
        },
        [255, 255, 255],
    );
    canvas.text(
        left,
        top + 222,
        if state.failed {
            "Login failed. Try again."
        } else {
            "Tab switches fields. JP106 keyboard."
        },
        [200, 200, 200],
    );
    Ok(bitmap(
        &pixels,
        u32::from(size.width),
        u32::from(size.height),
        u32::from(size.width) * 4,
        false,
        size,
        false,
    )?)
}
struct Canvas<'a> {
    pixels: &'a mut [u8],
    size: DesktopSize,
}
impl Canvas<'_> {
    fn rect(&mut self, x: i32, y: i32, width: i32, height: i32, color: [u8; 3]) {
        for y in y.max(0)..(y + height).min(i32::from(self.size.height)) {
            for x in x.max(0)..(x + width).min(i32::from(self.size.width)) {
                let offset = (y as usize * usize::from(self.size.width) + x as usize) * 4;
                self.pixels[offset..offset + 3].copy_from_slice(&color);
                self.pixels[offset + 3] = 255;
            }
        }
    }
    fn text(&mut self, mut x: i32, y: i32, text: &str, color: [u8; 3]) {
        for character in text.chars().take(64) {
            for (row, bits) in glyph(character).into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        self.rect(x + column * 2, y + row as i32 * 2, 2, 2, color);
                    }
                }
            }
            x += 12;
        }
    }
}
// Original 5x7 uppercase glyphs; form text uses no external font files.
fn glyph(character: char) -> [u8; 7] {
    match character {
        'a' => [0, 0, 14, 1, 15, 17, 15],
        'b' => [16, 16, 30, 17, 17, 17, 30],
        'c' => [0, 0, 14, 16, 16, 17, 14],
        'd' => [1, 1, 15, 17, 17, 17, 15],
        'e' => [0, 0, 14, 17, 31, 16, 14],
        'f' => [6, 9, 8, 28, 8, 8, 8],
        'g' => [0, 15, 17, 17, 15, 1, 14],
        'h' => [16, 16, 30, 17, 17, 17, 17],
        'i' => [4, 0, 12, 4, 4, 4, 14],
        'j' => [2, 0, 6, 2, 2, 18, 12],
        'k' => [16, 16, 18, 20, 24, 20, 18],
        'l' => [12, 4, 4, 4, 4, 4, 14],
        'm' => [0, 0, 26, 21, 21, 17, 17],
        'n' => [0, 0, 30, 17, 17, 17, 17],
        'o' => [0, 0, 14, 17, 17, 17, 14],
        'p' => [0, 30, 17, 17, 30, 16, 16],
        'q' => [0, 15, 17, 17, 15, 1, 1],
        'r' => [0, 0, 22, 25, 16, 16, 16],
        's' => [0, 0, 15, 16, 14, 1, 30],
        't' => [8, 8, 28, 8, 8, 9, 6],
        'u' => [0, 0, 17, 17, 17, 19, 13],
        'v' => [0, 0, 17, 17, 17, 10, 4],
        'w' => [0, 0, 17, 17, 21, 21, 10],
        'x' => [0, 0, 17, 10, 4, 10, 17],
        'y' => [0, 17, 17, 17, 15, 1, 14],
        'z' => [0, 0, 31, 2, 4, 8, 31],
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 2, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 27, 17],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        ' ' => [0; 7],
        '.' => [0, 0, 0, 0, 0, 6, 6],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        '*' => [0, 21, 14, 31, 14, 21, 0],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        _ => [14, 17, 1, 2, 4, 0, 4],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };

    fn login(username: &str) -> Login {
        Login::new(
            PathBuf::from("/nonexistent-niri-rdp-test.sock"),
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            username,
        )
    }
    fn key(login: &mut Login, code: u8) {
        login.keyboard(KeyboardEvent::Pressed {
            code,
            extended: false,
        });
        login.keyboard(KeyboardEvent::Released {
            code,
            extended: false,
        });
    }

    #[test]
    fn phone_unicode_login_preserves_text_surrogates_and_edit_keys() {
        let mut login = login("alice");
        for unit in "@\"日本😀".encode_utf16() {
            login.keyboard(KeyboardEvent::UnicodePressed(unit));
            login.keyboard(KeyboardEvent::UnicodeReleased(unit));
        }
        assert_eq!(login.state.lock().unwrap().password.0, "@\"日本😀");
        login.keyboard(KeyboardEvent::UnicodePressed(8));
        assert_eq!(login.state.lock().unwrap().password.0, "@\"日本");
        login.keyboard(KeyboardEvent::UnicodePressed(9));
        login.keyboard(KeyboardEvent::UnicodePressed(u16::from(b'x')));
        assert_eq!(login.state.lock().unwrap().username, "alicex");
        login.stop();
        assert!(login.state.lock().unwrap().password.0.is_empty());
    }
    #[test]
    fn japanese_login_punctuation_matches_jis_keys() {
        for (code, normal, shifted) in [
            (3, '2', '"'),
            (7, '6', '&'),
            (12, '-', '='),
            (13, '^', '~'),
            (26, '@', '`'),
            (27, '[', '{'),
            (39, ';', '+'),
            (40, ':', '*'),
            (43, ']', '}'),
            (0x73, '\\', '_'),
            (0x7d, '\\', '|'),
        ] {
            assert_eq!(jp_character(code, false, false), Some(normal));
            assert_eq!(jp_character(code, true, false), Some(shifted));
        }
        assert_eq!(jp_character(41, false, false), None);
    }
    #[test]
    fn form_handles_jp_keyboard_focus_mouse_and_bounded_secrets() {
        let mut login = login("");
        for code in [30, 38, 23, 46, 18, 15] {
            key(&mut login, code);
        }
        assert_eq!(login.state.lock().unwrap().username, "alice");
        key(&mut login, 30);
        login.keyboard(KeyboardEvent::Pressed {
            code: 42,
            extended: false,
        });
        key(&mut login, 2);
        login.keyboard(KeyboardEvent::Released {
            code: 42,
            extended: false,
        });
        key(&mut login, 58);
        key(&mut login, 48);
        assert_eq!(login.state.lock().unwrap().password.0, "a!B");
        key(&mut login, 14);
        assert_eq!(login.state.lock().unwrap().password.0, "a!");
        login.keyboard(KeyboardEvent::Pressed {
            code: 29,
            extended: true,
        });
        key(&mut login, 46);
        login.keyboard(KeyboardEvent::Released {
            code: 29,
            extended: true,
        });
        assert_eq!(login.state.lock().unwrap().password.0, "a!");
        for _ in 0..600 {
            login.character('x');
        }
        assert_eq!(login.state.lock().unwrap().password.0.len(), 512);
        let (x, y, _) = form(login.state.lock().unwrap().size);
        login.mouse(MouseEvent::Button {
            x: (x + 5) as u16,
            y: (y + 70) as u16,
            button: MouseButton::Left,
            pressed: true,
        });
        assert!(!login.state.lock().unwrap().password_focus);
        login.stop();
        assert!(login.state.lock().unwrap().password.0.is_empty());
    }

    #[test]
    fn password_content_and_length_never_change_framebuffer_mask() {
        let login = login("alice");
        let first = {
            let mut state = login.state.lock().unwrap();
            state.password.0.push_str("short");
            render(&state).unwrap().data
        };
        let second = {
            let mut state = login.state.lock().unwrap();
            state.password.clear();
            state
                .password
                .0
                .push_str("a-completely-different-long-password");
            render(&state).unwrap().data
        };
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn display_wakes_after_input_and_cancelled_wait_and_stops_on_disconnect() {
        let mut login = login("");
        let mut updates = login.updates().await.unwrap();
        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), updates.next_update())
                .await
                .is_err()
        );
        login.character('a');
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(50), updates.next_update())
                .await
                .unwrap()
                .unwrap(),
            Some(DisplayUpdate::Bitmap(_))
        ));
        login.stop();
        assert!(updates.next_update().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unchanged_login_repaints_after_client_surface_initialization() {
        let mut login = login("alice");
        let mut updates = login.updates().await.unwrap();
        let Some(DisplayUpdate::Bitmap(first)) = updates.next_update().await.unwrap() else {
            panic!("expected initial login bitmap");
        };
        let Some(DisplayUpdate::Bitmap(repaint)) =
            tokio::time::timeout(Duration::from_millis(1500), updates.next_update())
                .await
                .expect("static login must repaint without input")
                .unwrap()
        else {
            panic!("expected login repaint");
        };
        assert_eq!(first.data, repaint.data);
    }

    #[tokio::test]
    async fn rejected_pam_attempt_never_starts_desktop_and_parallel_submit_is_ignored() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (ready, seen) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut header = [0; 8];
            socket.read_exact(&mut header).unwrap();
            let length = usize::from(u16::from_be_bytes([header[4], header[5]]))
                + usize::from(u16::from_be_bytes([header[6], header[7]]));
            let mut credentials = vec![0; length];
            socket.read_exact(&mut credentials).unwrap();
            assert_eq!(credentials, b"aliceincorrect");
            credentials.fill(0);
            ready.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            socket.write_all(b"NRB1\x01").unwrap();
            listener.set_nonblocking(true).unwrap();
            assert!(listener.accept().is_err());
        });
        let mut login = Login::new(
            path,
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            "alice",
        );
        for character in "incorrect".chars() {
            login.character(character);
        }
        key(&mut login, 28);
        seen.recv_timeout(Duration::from_secs(2)).unwrap();
        key(&mut login, 28);
        assert!(login.state.lock().unwrap().busy);
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while login.state.lock().unwrap().busy {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let state = login.state.lock().unwrap();
        assert!(state.failed);
        assert!(state.password.0.is_empty());
        assert!(state.desktop.is_none());
        assert_eq!(state.attempts, 1);
        drop(state);
        worker.join().unwrap();
    }
}
