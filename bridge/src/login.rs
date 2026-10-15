//! Restricted TLS login screen. No Wayland connection exists before PAM accepts.
use crate::{
    broker::{self, LoginThrottle},
    video::Video,
    wayland::Desktop,
};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{
    BoundConnection, DesktopSize, DisplayUpdate, KeyboardEvent, MouseButton, MouseEvent,
    RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler, ServerResult,
};
use std::{
    cell::RefCell,
    net::IpAddr,
    num::{NonZeroU16, NonZeroUsize},
    ops::RangeInclusive,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use xkbcommon::xkb;

/// Bounds how long an unauthenticated peer may hold the single RDP slot on
/// the login screen. Both limits stop applying once a desktop is attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoginLimits {
    /// Disconnect after this long without a submitted login attempt.
    pub idle: Duration,
    /// Disconnect after this long on the login screen in total.
    pub total: Duration,
}
impl Default for LoginLimits {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(60),
            total: Duration::from_secs(120),
        }
    }
}
impl LoginLimits {
    pub const IDLE_SECS: RangeInclusive<u64> = 10..=600;
    pub const TOTAL_SECS: RangeInclusive<u64> = 30..=1800;

    /// Parse `NIRI_RDP_LOGIN_IDLE_SECS` and `NIRI_RDP_LOGIN_TIMEOUT_SECS`.
    pub fn from_env() -> anyhow::Result<Self> {
        fn variable(name: &str) -> anyhow::Result<Option<String>> {
            match std::env::var(name) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(_) => anyhow::bail!("{name} must contain valid Unicode"),
            }
        }
        Self::parse(
            variable("NIRI_RDP_LOGIN_IDLE_SECS")?.as_deref(),
            variable("NIRI_RDP_LOGIN_TIMEOUT_SECS")?.as_deref(),
        )
    }
    pub fn parse(idle: Option<&str>, total: Option<&str>) -> anyhow::Result<Self> {
        fn seconds(
            name: &str,
            value: Option<&str>,
            default: Duration,
            range: RangeInclusive<u64>,
        ) -> anyhow::Result<Duration> {
            let error = || {
                anyhow::anyhow!(
                    "{name} must be an integer within {}..{}",
                    range.start(),
                    range.end()
                )
            };
            let Some(value) = value else {
                return Ok(default);
            };
            let value = value.parse::<u64>().map_err(|_| error())?;
            anyhow::ensure!(range.contains(&value), error());
            Ok(Duration::from_secs(value))
        }
        let default = Self::default();
        let limits = Self {
            idle: seconds(
                "NIRI_RDP_LOGIN_IDLE_SECS",
                idle,
                default.idle,
                Self::IDLE_SECS,
            )?,
            total: seconds(
                "NIRI_RDP_LOGIN_TIMEOUT_SECS",
                total,
                default.total,
                Self::TOTAL_SECS,
            )?,
        };
        anyhow::ensure!(
            limits.idle <= limits.total,
            "NIRI_RDP_LOGIN_IDLE_SECS must not exceed NIRI_RDP_LOGIN_TIMEOUT_SECS"
        );
        Ok(limits)
    }
}

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
#[derive(Clone, Debug, PartialEq, Eq)]
enum LoginFailure {
    MissingCredentials,
    Rejected,
    DesktopUnavailable,
    DesktopInUse,
    LocalSession,
}
impl LoginFailure {
    fn message(&self) -> &'static str {
        match self {
            Self::MissingCredentials => "Enter your username and password.",
            Self::Rejected => "Login denied. Check account and password.",
            Self::DesktopUnavailable => "Desktop unavailable. Try again.",
            Self::DesktopInUse => "Desktop in use by another connection.",
            Self::LocalSession => "A local desktop session is still open.",
        }
    }
}
impl From<broker::Denial> for LoginFailure {
    fn from(denial: broker::Denial) -> Self {
        match denial {
            broker::Denial::Rejected => Self::Rejected,
            broker::Denial::AlreadyAttached => Self::DesktopInUse,
            broker::Denial::LocalSession => Self::LocalSession,
        }
    }
}
struct State {
    size: DesktopSize,
    username: String,
    password: Password,
    password_focus: bool,
    shift: [bool; 2],
    caps: bool,
    num_lock: bool,
    keymap: String,
    keyboard_layout: String,
    unicode: crate::keyboard::UnicodeInput,
    compose: crate::login_compose::ComposeInput,
    synchronize: Option<KeyboardEvent>,
    control: [bool; 2],
    alt: [bool; 2],
    busy: bool,
    failure: Option<LoginFailure>,
    attempts: u8,
    retry_at: Instant,
    opened: tokio::time::Instant,
    last_submit: tokio::time::Instant,
    stopped: bool,
    desktop: Option<Desktop>,
    pointer_max_size: u16,
    pointer_cache_size: u16,
    pending_layout: Option<DisplayControlMonitorLayout>,
    suppressed: bool,
    refresh_epoch: u64,
}
#[derive(Clone)]
pub struct Login {
    state: Arc<Mutex<State>>,
    changed: tokio::sync::watch::Sender<u64>,
    path: PathBuf,
    output: String,
    video: Option<Video>,
    audio: crate::audio::Audio,
    microphone: Option<crate::microphone::Microphone>,
    clipboard: crate::clipboard::Clipboard,
    limits: LoginLimits,
    source: Option<(LoginThrottle, IpAddr)>,
}
impl State {
    /// When the form disconnects unless a login attempt is submitted. Neither
    /// limit runs while PAM or desktop startup is in progress, nor afterwards.
    fn deadline(&self, limits: LoginLimits) -> Option<tokio::time::Instant> {
        if self.busy || self.desktop.is_some() {
            return None;
        }
        Some((self.last_submit + limits.idle).min(self.opened + limits.total))
    }
    /// Forward this activation's pointer ceiling and client cache size; the
    /// desktop's pointer cache stays disabled without the cache size.
    fn replay_pointer_capabilities(&self, desktop: &mut Desktop) {
        desktop.pointer_cache_capabilities(self.pointer_max_size, self.pointer_cache_size);
    }
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
        // The restricted form cannot connect to a user's compositor before PAM.
        // Its launcher supplies the same host XKB defaults as a remote session.
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap_or_else(|| {
            eprintln!("host login keyboard map unavailable; falling back to US");
            xkb::Keymap::new_from_names(
                &context,
                "evdev",
                "pc105",
                "us",
                "",
                Some(String::new()),
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .expect("fallback login keyboard map unavailable")
        });
        let keyboard_layout = map.layout_get_name(0).to_owned();
        let keymap = map.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        Self {
            state: Arc::new(Mutex::new(State {
                size,
                password_focus: !username.is_empty(),
                username,
                password: Password::default(),
                shift: [false; 2],
                caps: false,
                num_lock: false,
                keymap,
                keyboard_layout,
                unicode: Default::default(),
                compose: Default::default(),
                synchronize: None,
                control: [false; 2],
                alt: [false; 2],
                busy: false,
                failure: None,
                attempts: 0,
                retry_at: Instant::now(),
                opened: tokio::time::Instant::now(),
                last_submit: tokio::time::Instant::now(),
                stopped: false,
                desktop: None,
                pointer_max_size: 0,
                pointer_cache_size: 0,
                pending_layout: None,
                suppressed: false,
                refresh_epoch: 0,
            })),
            changed,
            path,
            output,
            video,
            audio: Default::default(),
            microphone: None,
            clipboard: Default::default(),
            limits: LoginLimits::default(),
            source: None,
        }
    }
    pub fn with_limits(mut self, limits: LoginLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Share the frontend's per-source failure backoff with this form.
    pub fn with_throttle(mut self, throttle: LoginThrottle, source: Option<IpAddr>) -> Self {
        self.source = source.map(|source| (throttle, source));
        self
    }
    pub fn with_audio(mut self, audio: crate::audio::Audio) -> Self {
        self.audio = audio;
        self
    }
    pub fn with_clipboard(mut self, clipboard: crate::clipboard::Clipboard) -> Self {
        self.clipboard = clipboard;
        self
    }
    /// `None` when the microphone is off: its descriptor is then dropped.
    pub fn with_microphone(mut self, microphone: Option<crate::microphone::Microphone>) -> Self {
        self.microphone = microphone;
        self
    }
    /// Open the form explaining why authenticated credentials opened no desktop.
    pub fn with_notice(self, denial: Option<broker::Denial>) -> Self {
        if let Some(denial) = denial {
            self.state.lock().unwrap().failure = Some(denial.into());
        }
        self
    }
    #[cfg(test)]
    pub(crate) fn failure_message(&self) -> Option<&'static str> {
        self.state
            .lock()
            .unwrap()
            .failure
            .as_ref()
            .map(LoginFailure::message)
    }
    #[cfg(test)]
    pub(crate) fn limits(&self) -> LoginLimits {
        self.limits
    }
    #[cfg(test)]
    pub(crate) fn source(&self) -> Option<IpAddr> {
        self.source.as_ref().map(|(_, source)| *source)
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
        self.audio.clear();
        if let Some(microphone) = &self.microphone {
            microphone.clear();
        }
        self.clipboard.clear();
        state.password.clear();
        state.compose.reset();
        state.unicode = Default::default();
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
            state.last_submit = tokio::time::Instant::now();
            if state.username.is_empty() || state.password.0.is_empty() {
                state.compose.reset();
                state.unicode = Default::default();
                state.failure = Some(LoginFailure::MissingCredentials);
                drop(state);
                self.notify();
                return;
            }
            // During a source backoff, answer like PAM would without asking
            // it: the same generic message, per-form attempt and retry pause.
            if self
                .source
                .as_ref()
                .is_some_and(|(throttle, source)| throttle.blocked(*source))
            {
                state.compose.reset();
                state.unicode = Default::default();
                state.password.clear();
                Self::reject(&mut state);
                drop(state);
                self.notify();
                return;
            }
            state.busy = true;
            state.failure = None;
            state.compose.reset();
            state.unicode = Default::default();
            let password = std::mem::take(&mut state.password);
            (state.username.clone(), password, state.size)
        };
        self.notify();
        let state = Arc::downgrade(&self.state);
        let path = self.path.clone();
        let output = self.output.clone();
        let changed = self.changed.clone();
        let audio = self.audio.clone();
        let microphone = self.microphone.clone();
        let clipboard = self.clipboard.clone();
        let source = self.source.clone();
        // PAM and compositor startup may block; neither runs in the RDP loop.
        std::thread::spawn(move || {
            let peer = source.as_ref().map(|(_, source)| *source);
            let opened = broker::open(&path, &username, &password.0, peer);
            if let Some((throttle, source)) = &source {
                match &opened {
                    Ok(Ok(_)) => throttle.record_success(*source),
                    // PAM accepted the password even though no desktop opened.
                    Ok(Err(denial)) if denial.authenticated() => throttle.record_success(*source),
                    Ok(Err(_)) => throttle.record_failure(*source),
                    // Broker or desktop failures say nothing about the password.
                    Err(_) => {}
                }
            }
            let result = opened.and_then(|session| {
                let session = match session {
                    Ok(session) => session,
                    Err(denial) => return Ok(Err(denial)),
                };
                if state
                    .upgrade()
                    .is_none_or(|state| state.lock().unwrap().stopped)
                {
                    return Ok(Ok(None));
                }
                let desktop = Desktop::start(session.socket, output, size, Some(session.lease))?;
                Ok(Ok(Some((desktop, session.audio, session.microphone))))
            });
            drop(password);
            if let Some(state) = state.upgrade() {
                let mut state = state.lock().unwrap();
                state.busy = false;
                // Time spent waiting for PAM does not count as idle form time.
                state.last_submit = tokio::time::Instant::now();
                match result {
                    Ok(Ok(Some((mut desktop, audio_socket, microphone_socket))))
                        if !state.stopped =>
                    {
                        audio.attach(audio_socket);
                        if let Some(microphone) = &microphone {
                            microphone.attach(microphone_socket);
                        }
                        desktop.attach_clipboard(clipboard.clone());
                        state.replay_pointer_capabilities(&mut desktop);
                        desktop.set_output_suppressed(state.suppressed);
                        if let Some(layout) = state.pending_layout.take() {
                            desktop.request_layout(layout);
                        }
                        // Preserve lock state and modifiers held while signing in.
                        let synchronized =
                            if let Some(KeyboardEvent::Synchronize(flags)) = &state.synchronize {
                                desktop.keyboard(KeyboardEvent::Synchronize(*flags));
                                flags.bits()
                            } else {
                                0
                            };
                        for (active, bit, code) in [(state.caps, 4, 58), (state.num_lock, 2, 69)] {
                            if active != (synchronized & bit != 0) {
                                desktop.keyboard(KeyboardEvent::Pressed {
                                    code,
                                    extended: false,
                                });
                                desktop.keyboard(KeyboardEvent::Released {
                                    code,
                                    extended: false,
                                });
                            }
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
                    Ok(Ok(Some((desktop, _, _)))) => desktop.stop(),
                    // The form closed while PAM ran.
                    Ok(Ok(None)) => {}
                    Ok(Err(broker::Denial::Rejected)) => Self::reject(&mut state),
                    // Not a password failure: no attempt is consumed.
                    Ok(Err(denial)) => {
                        state.failure = Some(denial.into());
                        state.retry_at = Instant::now() + Duration::from_secs(2);
                    }
                    Err(error) => {
                        eprintln!("remote desktop startup failed: {error}");
                        state.failure = Some(LoginFailure::DesktopUnavailable);
                        state.retry_at = Instant::now() + Duration::from_secs(2);
                    }
                }
                drop(state);
                changed.send_modify(|generation| *generation = generation.wrapping_add(1));
            }
        });
    }
    fn reject(state: &mut State) {
        state.failure = Some(LoginFailure::Rejected);
        state.attempts += 1;
        state.retry_at = Instant::now() + Duration::from_secs(2);
        if state.attempts >= 5 {
            state.stopped = true;
        }
    }
    /// Close the form once its idle or total deadline has passed.
    fn expire(&self) {
        eprintln!("RDP login screen closed: no successful login before its deadline");
        self.stop();
    }
    fn character(&self, character: char) {
        if character.is_control() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.stopped || state.busy {
            return;
        }
        let visible = VisibleState::from(&*state);
        if state.password_focus {
            if state.password.0.len() + character.len_utf8() <= 512 {
                state.password.0.push(character);
            }
        } else if state.username.len() + character.len_utf8() <= 64 {
            state.username.push(character);
        }
        state.failure = None;
        let changed = visible != VisibleState::from(&*state);
        drop(state);
        if changed {
            self.notify();
        }
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
            KeyboardEvent::Pressed {
                code: 0x1c,
                extended: true,
            } => (28, true),
            KeyboardEvent::Pressed {
                code: 0x35,
                extended: true,
            } => (98, true),
            KeyboardEvent::UnicodePressed(value) => {
                let character = {
                    let mut state = self.state.lock().unwrap();
                    // Unicode events are text already composed by the client.
                    state.compose.reset();
                    state.unicode.press(value)
                };
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
                state.num_lock = flags.bits() & 2 != 0;
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
        let shortcut = {
            let state = self.state.lock().unwrap();
            // Windows sends a synthetic left Ctrl alongside AltGr.
            state.control[1] || (state.control[0] && !state.alt[1]) || state.alt[0]
        };
        if shortcut {
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
        let visible = VisibleState::from(&*state);
        match code {
            15 => {
                state.compose.reset();
                state.password_focus = !state.password_focus;
            }
            14 => {
                state.compose.reset();
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
            69 => state.num_lock = !state.num_lock,
            _ => {
                let evdev = if code == 98 {
                    Some(98)
                } else {
                    crate::evdev_key(code, false)
                };
                let character = evdev.and_then(|key| {
                    let State {
                        keymap,
                        shift,
                        caps,
                        num_lock,
                        alt,
                        compose,
                        ..
                    } = &mut *state;
                    login_text(keymap, key, *shift, *caps, *num_lock, alt[1], compose)
                });
                drop(state);
                if let Some(text) = character {
                    for character in text.chars() {
                        self.character(character);
                    }
                }
                return;
            }
        }
        let changed = visible != VisibleState::from(&*state);
        drop(state);
        if changed {
            self.notify();
        }
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
            let previous_focus = state.password_focus;
            if (60..92).contains(&y) {
                state.password_focus = false;
            }
            if (122..154).contains(&y) {
                state.password_focus = true;
            }
            if state.password_focus != previous_focus {
                state.compose.reset();
                state.unicode = Default::default();
            }
            drop(state);
            if previous_focus != self.state.lock().unwrap().password_focus {
                self.notify();
            }
            if (172..206).contains(&y) {
                self.submit();
            }
        }
    }
}
#[async_trait::async_trait]
impl RdpServerDisplay for Login {
    fn request_refresh(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.refresh_epoch = state.refresh_epoch.wrapping_add(1);
        if let Some(mut desktop) = state.desktop.clone() {
            desktop.request_refresh();
        }
        drop(state);
        if let Some(video) = &self.video {
            video.request_refresh();
        }
        self.notify();
    }
    fn set_output_suppressed(&mut self, suppressed: bool) {
        let mut state = self.state.lock().unwrap();
        if state.suppressed && !suppressed {
            state.refresh_epoch = state.refresh_epoch.wrapping_add(1);
        }
        state.suppressed = suppressed;
        if let Some(mut desktop) = state.desktop.clone() {
            desktop.set_output_suppressed(suppressed);
        }
        drop(state);
        self.notify();
    }
    fn pointer_capabilities(&mut self, max_size: u16) {
        self.pointer_cache_capabilities(max_size, 0);
    }
    fn pointer_cache_capabilities(&mut self, max_size: u16, cache_size: u16) {
        let mut state = self.state.lock().unwrap();
        state.pointer_max_size = max_size;
        state.pointer_cache_size = cache_size;
        if let Some(mut desktop) = state.desktop.clone() {
            state.replay_pointer_capabilities(&mut desktop);
        }
    }
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
            cached: None,
            delivered_initial: false,
            refresh_epoch: 0,
        });
        // Use the negotiated graphics channel for the form as well as the
        // desktop. mstsc may stop presenting classic bitmaps once GFX opens.
        match &self.video {
            Some(video) => {
                let state = self.state.clone();
                Ok(video.wrap_updates_with_desktop_provider(
                    updates,
                    Arc::new(move || state.lock().unwrap().desktop.clone()),
                )?)
            }
            None => Ok(updates),
        }
    }
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let mut state = self.state.lock().unwrap();
        if let Some(mut desktop) = state.desktop.clone() {
            desktop.request_layout(layout);
        } else {
            state.pending_layout = Some(layout);
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
    cached: Option<LoginFramebuffer>,
    delivered_initial: bool,
    refresh_epoch: u64,
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
                self.desktop = Some(if self.login.video.is_some() {
                    desktop.updates_full_send()
                } else {
                    desktop.updates_send()
                });
                continue;
            }
            let (suppressed, deadline) = {
                let state = self.login.state.lock().unwrap();
                (state.suppressed, state.deadline(self.login.limits))
            };
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                self.login.expire();
                return Ok(None);
            }
            if suppressed && self.delivered_initial {
                tokio::select! {
                    changed = self.rx.changed() => {
                        if changed.is_err() {
                            return Ok(None);
                        }
                    }
                    _ = until(deadline) => {}
                }
                continue;
            }
            if self.first || self.rx.has_changed().unwrap_or(false) {
                self.first = false;
                self.rx.borrow_and_update();
                self.repaint = tokio::time::Instant::now() + Duration::from_secs(1);
                let state = self.login.state.lock().unwrap();
                let full = self.refresh_epoch != state.refresh_epoch;
                self.refresh_epoch = state.refresh_epoch;
                self.delivered_initial = true;
                let visible = VisibleState::from(&*state);
                let cached = self
                    .cached
                    .get_or_insert_with(|| LoginFramebuffer::new(&state));
                if cached.visible != visible {
                    cached.repaint(&state);
                    if self.login.video.is_none() && !full {
                        return Ok(Some(DisplayUpdate::Bitmap(cached.region())));
                    }
                }
                return Ok(Some(DisplayUpdate::Bitmap(cached.snapshot())));
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
                _ = until(deadline) => continue,
            }
            self.first = true;
        }
    }
}
async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
thread_local! {
    // XKB objects stay on the input thread. The cache holds the public keyboard
    // map only; password characters and compose state remain in the form.
    static TRANSLATION: RefCell<Option<(String, xkb::Context, xkb::Keymap)>> = const { RefCell::new(None) };
}

fn login_text(
    text: &str,
    evdev: u32,
    shift: [bool; 2],
    caps: bool,
    num_lock: bool,
    altgr: bool,
    compose: &mut crate::login_compose::ComposeInput,
) -> Option<String> {
    TRANSLATION.with(|cached| {
        let mut cached = cached.borrow_mut();
        if cached.as_ref().is_none_or(|(last, _, _)| last != text) {
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let map = xkb::Keymap::new_from_string(
                &context,
                text.to_owned(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )?;
            *cached = Some((text.to_owned(), context, map));
        }
        let (_, context, map) = cached.as_ref().unwrap();
        let mut state = xkb::State::new(map);
        let mut locked = 0;
        for (active, name) in [(caps, "Lock"), (num_lock, "NumLock")] {
            let index = map.mod_get_index(name);
            if active && index < 32 {
                locked |= 1 << index;
            }
        }
        state.update_mask(0, 0, locked, 0, 0, 0);
        for (held, key) in [(shift[0], 42u32), (shift[1], 54), (altgr, 100)] {
            if held {
                state.update_key((key + 8).into(), xkb::KeyDirection::Down);
            }
        }
        let code = (evdev + 8).into();
        let text = compose.press(
            context,
            state.key_get_one_sym(code),
            state.key_get_utf8(code),
        );
        (!text.is_empty()).then_some(text)
    })
}
#[cfg(test)]
fn login_character(
    text: &str,
    evdev: u32,
    shift: [bool; 2],
    caps: bool,
    num_lock: bool,
    altgr: bool,
) -> Option<char> {
    login_text(
        text,
        evdev,
        shift,
        caps,
        num_lock,
        altgr,
        &mut Default::default(),
    )?
    .chars()
    .next()
}
fn form(size: DesktopSize) -> (i32, i32, i32) {
    let width = 420.min(i32::from(size.width).saturating_sub(20)).max(1);
    (
        (i32::from(size.width) - width) / 2,
        (i32::from(size.height) - 260).max(0) / 2,
        width,
    )
}
// Only values that alter pixels belong here. In particular, the mask records
// emptiness, never the password or its length.
#[derive(PartialEq, Eq)]
struct VisibleState {
    width: u16,
    height: u16,
    username: String,
    password_present: bool,
    password_focus: bool,
    busy: bool,
    failure: Option<LoginFailure>,
    keyboard_layout: String,
}
impl From<&State> for VisibleState {
    fn from(state: &State) -> Self {
        Self {
            width: state.size.width,
            height: state.size.height,
            username: state.username.clone(),
            password_present: !state.password.0.is_empty(),
            password_focus: state.password_focus,
            busy: state.busy,
            failure: state.failure.clone(),
            keyboard_layout: state.keyboard_layout.clone(),
        }
    }
}
fn render(state: &State) -> anyhow::Result<ironrdp_server::BitmapUpdate> {
    let size = state.size;
    let mut pixels = vec![0; usize::from(size.width) * usize::from(size.height) * 4];
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
    paint_form(&mut canvas, state);
    // Canvas already produces the exact BGR wire order; avoid a second full
    // allocation and conversion just to force the unused alpha byte.
    Ok(ironrdp_server::BitmapUpdate {
        x: 0,
        y: 0,
        width: NonZeroU16::new(size.width).ok_or_else(|| anyhow::anyhow!("empty login width"))?,
        height: NonZeroU16::new(size.height)
            .ok_or_else(|| anyhow::anyhow!("empty login height"))?,
        format: ironrdp_server::PixelFormat::BgrX32,
        data: pixels.into(),
        stride: NonZeroUsize::new(usize::from(size.width) * 4).unwrap(),
    })
}
struct LoginFramebuffer {
    visible: VisibleState,
    pixels: Vec<u8>,
    snapshot: Option<ironrdp_server::BitmapUpdate>,
}
impl LoginFramebuffer {
    fn new(state: &State) -> Self {
        let bitmap = render(state).expect("negotiated login dimensions are nonzero");
        Self {
            visible: VisibleState::from(state),
            pixels: bitmap.data.to_vec(),
            snapshot: Some(bitmap),
        }
    }
    fn repaint(&mut self, state: &State) {
        if self.visible.width != state.size.width || self.visible.height != state.size.height {
            *self = Self::new(state);
            return;
        }
        let (left, top, _) = form(state.size);
        let mut canvas = Canvas {
            pixels: &mut self.pixels,
            size: state.size,
        };
        // The desktop background is allocated and filled once. Repaint only
        // the fields, form and status line when their visible state changes.
        canvas.rect(
            left,
            top,
            i32::from(state.size.width) - left,
            242,
            [35, 27, 22],
        );
        paint_form(&mut canvas, state);
        self.visible = VisibleState::from(state);
        self.snapshot = None;
    }
    fn snapshot(&mut self) -> ironrdp_server::BitmapUpdate {
        self.snapshot
            .get_or_insert_with(|| {
                framebuffer(
                    self.pixels.clone(),
                    DesktopSize {
                        width: self.visible.width,
                        height: self.visible.height,
                    },
                    0,
                    0,
                )
            })
            .clone()
    }
    fn region(&self) -> ironrdp_server::BitmapUpdate {
        let size = DesktopSize {
            width: self.visible.width,
            height: self.visible.height,
        };
        let (left, top, _) = form(size);
        let region = DesktopSize {
            width: size.width - left as u16,
            height: (size.height - top as u16).min(242),
        };
        let mut pixels =
            Vec::with_capacity(usize::from(region.width) * usize::from(region.height) * 4);
        for y in top as usize..top as usize + usize::from(region.height) {
            let offset = (y * usize::from(size.width) + left as usize) * 4;
            pixels.extend_from_slice(&self.pixels[offset..offset + usize::from(region.width) * 4]);
        }
        framebuffer(pixels, region, left as u16, top as u16)
    }
}
fn framebuffer(pixels: Vec<u8>, size: DesktopSize, x: u16, y: u16) -> ironrdp_server::BitmapUpdate {
    ironrdp_server::BitmapUpdate {
        x,
        y,
        width: NonZeroU16::new(size.width).unwrap(),
        height: NonZeroU16::new(size.height).unwrap(),
        format: ironrdp_server::PixelFormat::BgrX32,
        data: pixels.into(),
        stride: NonZeroUsize::new(usize::from(size.width) * 4).unwrap(),
    }
}
fn paint_form(canvas: &mut Canvas<'_>, state: &State) {
    let (left, top, width) = form(state.size);
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
    canvas.text(left, top + 102, "Password (length hidden)", [200, 200, 200]);
    canvas.rect(
        left,
        top + 122,
        width,
        32,
        if state.failure.is_some() {
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
    let keyboard_hint = format!("Tab switches fields. {} keyboard.", state.keyboard_layout);
    canvas.text(
        left,
        top + 222,
        state
            .failure
            .as_ref()
            .map_or(keyboard_hint.as_str(), |failure| failure.message()),
        [200, 200, 200],
    );
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
    use std::{io::Write, os::unix::net::UnixListener};

    #[tokio::test(flavor = "current_thread")]
    async fn display_wrappers_deliver_negotiated_pointer_cache_size_to_desktop() {
        use crate::wayland::lifecycle_tests::{fake_desktop, repeated_pointer_is_cached};
        let size = DesktopSize {
            width: 64,
            height: 64,
        };
        // Broker path: activation precedes sign-in; the attach replays it.
        let mut form = login("alice");
        form.pointer_cache_capabilities(96, 25);
        let (mut desktop, _frames) = fake_desktop(size);
        form.state
            .lock()
            .unwrap()
            .replay_pointer_capabilities(&mut desktop);
        assert!(repeated_pointer_is_cached(&desktop).await);
        // Reactivation after sign-in forwards at once, including a client
        // without a pointer cache.
        form.state.lock().unwrap().desktop = Some(desktop.clone());
        form.pointer_cache_capabilities(96, 0);
        assert!(!repeated_pointer_is_cached(&desktop).await);
        form.pointer_cache_capabilities(96, 25);
        assert!(repeated_pointer_is_cached(&desktop).await);
        // Direct path with RDPGFX: VideoDisplay wraps the desktop.
        let (desktop, _frames) = fake_desktop(size);
        let mut display = crate::video::Video::default().display(desktop.clone());
        display.pointer_cache_capabilities(96, 25);
        assert!(repeated_pointer_is_cached(&desktop).await);
    }

    fn keyboard_map(model: &str, layout: &str) -> String {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &context,
            "evdev",
            model,
            layout,
            "",
            Some(String::new()),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap()
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    }
    fn login(username: &str) -> Login {
        let login = Login::new(
            PathBuf::from("/nonexistent-niri-rdp-test.sock"),
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            username,
        );
        login.state.lock().unwrap().keymap = keyboard_map("jp106", "jp");
        login
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
    fn login_inherits_host_keyboard_defaults_before_authentication() {
        const CHILD: &str = "NIRI_RDP_LOGIN_KEYBOARD_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "login::tests::login_inherits_host_keyboard_defaults_before_authentication",
                ])
                .env(CHILD, "1")
                .env("XKB_DEFAULT_RULES", "evdev")
                .env("XKB_DEFAULT_MODEL", "jp106")
                .env("XKB_DEFAULT_LAYOUT", "jp")
                .env("XKB_DEFAULT_VARIANT", "")
                .env("XKB_DEFAULT_OPTIONS", "")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let mut login = Login::new(
            PathBuf::new(),
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            "alice",
        );
        for code in [26, 27, 40, 0x73, 0x7d] {
            key(&mut login, code);
        }
        assert_eq!(login.state.lock().unwrap().password.0, "@[:\\\\");
        assert_eq!(login.state.lock().unwrap().keyboard_layout, "Japanese");
        assert!(login.state.lock().unwrap().desktop.is_none());
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
    fn physical_login_composes_dead_keys_and_preserves_client_unicode() {
        let mut login = login("alice");
        login.state.lock().unwrap().keymap = keyboard_map("pc105", "de");
        key(&mut login, 13); // German acute accent is a dead key.
        assert!(login.state.lock().unwrap().password.0.is_empty());
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().password.0, "á");
        key(&mut login, 13);
        login.keyboard(KeyboardEvent::UnicodePressed('é' as u16));
        login.keyboard(KeyboardEvent::UnicodeReleased('é' as u16));
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().password.0, "áéa");
    }

    #[test]
    fn login_composition_resets_when_editing_switching_fields_and_submitting() {
        let mut login = login("alice");
        login.state.lock().unwrap().keymap = keyboard_map("pc105", "de");
        key(&mut login, 13);
        key(&mut login, 14); // Backspace cancels the pending accent.
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().password.0, "a");
        key(&mut login, 13);
        key(&mut login, 15); // A pending password accent cannot reach username.
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().username, "alicea");
        key(&mut login, 13);
        let (x, y, _) = form(login.state.lock().unwrap().size);
        login.mouse(MouseEvent::Button {
            x: (x + 10) as u16,
            y: (y + 130) as u16,
            button: MouseButton::Left,
            pressed: true,
        });
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().password.0, "aa");
        login.state.lock().unwrap().password.clear();
        key(&mut login, 13);
        login.submit(); // Missing credentials must also cancel composition.
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().password.0, "a");
        login.stop();
        assert!(login.state.lock().unwrap().password.0.is_empty());
    }

    #[test]
    fn pending_unicode_surrogates_cannot_cross_focus_or_submission() {
        let mut login = login("alice");
        login.keyboard(KeyboardEvent::UnicodePressed(0xd83d));
        let (x, y, _) = form(login.state.lock().unwrap().size);
        login.mouse(MouseEvent::Button {
            x: (x + 10) as u16,
            y: (y + 70) as u16,
            button: MouseButton::Left,
            pressed: true,
        });
        login.keyboard(KeyboardEvent::UnicodePressed(0xde00));
        assert_eq!(login.state.lock().unwrap().username, "alice");
        key(&mut login, 15);
        login.keyboard(KeyboardEvent::UnicodePressed(0xd83d));
        login.submit(); // The form is missing a complete password.
        login.keyboard(KeyboardEvent::UnicodePressed(0xde00));
        assert!(login.state.lock().unwrap().password.0.is_empty());
        login.keyboard(KeyboardEvent::UnicodePressed(u16::from(b'a')));
        assert_eq!(login.state.lock().unwrap().password.0, "a");
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
            let map = keyboard_map("jp106", "jp");
            let evdev = crate::evdev_key(code, false).unwrap();
            assert_eq!(
                login_character(&map, evdev, [false; 2], false, false, false),
                Some(normal)
            );
            assert_eq!(
                login_character(&map, evdev, [true, false], false, false, false),
                Some(shifted)
            );
        }
        let map = keyboard_map("jp106", "jp");
        assert_eq!(
            login_character(&map, 41, [false; 2], false, false, false),
            None
        );
        assert_eq!(
            login_character(&map, 11, [true, false], false, false, false),
            Some('~')
        );
    }
    #[test]
    fn login_uses_configured_layout_and_supports_altgr_and_keypad() {
        for (layout, normal, shifted) in [("us", '[', '{'), ("se", 'å', 'Å')] {
            let mut login = login("alice");
            login.state.lock().unwrap().keymap = keyboard_map("pc105", layout);
            key(&mut login, 26);
            login.keyboard(KeyboardEvent::Pressed {
                code: 42,
                extended: false,
            });
            key(&mut login, 26);
            login.keyboard(KeyboardEvent::Released {
                code: 42,
                extended: false,
            });
            assert_eq!(
                login.state.lock().unwrap().password.0,
                format!("{normal}{shifted}")
            );
        }
        let mut login = login("alice");
        login.state.lock().unwrap().keymap = keyboard_map("pc105", "se");
        login.keyboard(KeyboardEvent::Pressed {
            code: 29,
            extended: false,
        });
        login.keyboard(KeyboardEvent::Pressed {
            code: 56,
            extended: true,
        });
        key(&mut login, 3); // Windows AltGr (Ctrl+right Alt) + 2 is @.
        login.keyboard(KeyboardEvent::Released {
            code: 56,
            extended: true,
        });
        login.keyboard(KeyboardEvent::Released {
            code: 29,
            extended: false,
        });
        key(&mut login, 69); // Num Lock enables the keypad.
        key(&mut login, 79);
        login.keyboard(KeyboardEvent::Pressed {
            code: 0x35,
            extended: true,
        });
        assert_eq!(login.state.lock().unwrap().password.0, "@1/");
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
    fn each_key_press_adds_one_character_under_the_fixed_password_mask() {
        let mut login = login("alice");
        key(&mut login, 30);
        assert_eq!(login.state.lock().unwrap().password.0, "a");
        let single = render(&login.state.lock().unwrap()).unwrap();
        login.keyboard(KeyboardEvent::UnicodePressed(u16::from(b'b')));
        login.keyboard(KeyboardEvent::UnicodeReleased(u16::from(b'b')));
        assert_eq!(login.state.lock().unwrap().password.0, "ab");
        let double = render(&login.state.lock().unwrap()).unwrap();
        assert_eq!(single.data, double.data);
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

    #[test]
    fn cached_regions_reconstruct_login_and_idle_reuses_owned_pixels() {
        for size in [
            DesktopSize {
                width: 200,
                height: 200,
            },
            DesktopSize {
                width: 1920,
                height: 1080,
            },
            DesktopSize {
                width: 3840,
                height: 2160,
            },
        ] {
            let login = login("alice");
            let mut state = login.state.lock().unwrap();
            state.size = size;
            let mut cache = LoginFramebuffer::new(&state);
            let first = cache.snapshot();
            let idle = cache.snapshot();
            assert_eq!(first.data.as_ptr(), idle.data.as_ptr());
            let mut reconstructed = first.data.to_vec();
            for (username, focus, failure, busy) in [
                ("bob", false, None, false),
                ("b", true, Some(LoginFailure::Rejected), false),
                ("alice", true, None, true),
            ] {
                state.username = username.into();
                state.password_focus = focus;
                state.failure = failure;
                state.busy = busy;
                cache.repaint(&state);
                let region = cache.region();
                for row in 0..usize::from(region.height.get()) {
                    let offset = ((usize::from(region.y) + row) * usize::from(size.width)
                        + usize::from(region.x))
                        * 4;
                    let source = row * region.stride.get();
                    reconstructed[offset..offset + region.stride.get()]
                        .copy_from_slice(&region.data[source..source + region.stride.get()]);
                }
                assert_eq!(reconstructed, render(&state).unwrap().data);
                assert_eq!(cache.snapshot().data, reconstructed);
            }
        }
    }

    #[tokio::test]
    async fn password_edits_after_first_mask_do_not_notify_or_transmit_length() {
        let login = login("alice");
        let mut updates = login.clone().updates().await.unwrap();
        updates.next_update().await.unwrap();
        login.character('a');
        updates.next_update().await.unwrap();
        let generation = *login.changed.borrow();
        for c in "日本😀password".chars() {
            login.character(c);
        }
        assert_eq!(*login.changed.borrow(), generation);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), updates.next_update())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn suppressed_login_retains_initial_surface_and_resumes_cached_full_refresh() {
        let mut login = login("");
        login.set_output_suppressed(true);
        let mut updates = login.updates().await.unwrap();
        let Some(DisplayUpdate::Bitmap(first)) = updates.next_update().await.unwrap() else {
            panic!("missing startup surface");
        };
        login.character('a');
        assert!(
            tokio::time::timeout(Duration::from_millis(20), updates.next_update())
                .await
                .is_err()
        );
        login.set_output_suppressed(false);
        let Some(DisplayUpdate::Bitmap(resumed)) = updates.next_update().await.unwrap() else {
            panic!("missing resumed surface");
        };
        assert_eq!(
            (resumed.x, resumed.y, resumed.width, resumed.height),
            (first.x, first.y, first.width, first.height)
        );
        login.request_refresh();
        let Some(DisplayUpdate::Bitmap(refreshed)) = updates.next_update().await.unwrap() else {
            panic!("missing explicit refresh");
        };
        assert_eq!(refreshed.data.as_ptr(), resumed.data.as_ptr());
    }

    #[test]
    #[ignore = "comparative login rendering measurement"]
    fn login_render_benchmark() {
        for size in [
            DesktopSize {
                width: 1920,
                height: 1080,
            },
            DesktopSize {
                width: 3840,
                height: 2160,
            },
        ] {
            let login = login("alice");
            let mut state = login.state.lock().unwrap();
            state.size = size;
            let start = Instant::now();
            for _ in 0..60 {
                std::hint::black_box(render(&state).unwrap());
            }
            let uncached_us = start.elapsed().as_micros();
            let mut cache = LoginFramebuffer::new(&state);
            let start = Instant::now();
            for _ in 0..60 {
                std::hint::black_box(cache.snapshot());
            }
            let idle_us = start.elapsed().as_micros();
            let start = Instant::now();
            let mut bytes = 0;
            for i in 0..60 {
                state.username = format!("user{i}");
                cache.repaint(&state);
                bytes += cache.region().data.len();
            }
            let editing_us = start.elapsed().as_micros();
            println!(
                "{{\"component\":\"login\",\"width\":{},\"height\":{},\"updates\":60,\"uncached_us\":{uncached_us},\"cached_idle_us\":{idle_us},\"partial_edit_us\":{editing_us},\"partial_bytes\":{bytes}}}",
                size.width, size.height
            );
        }
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
            let mut request = broker::read_test_request(&mut socket);
            assert_eq!(request.username, b"alice");
            assert_eq!(request.password, b"incorrect");
            // The form forwards its client address to the broker.
            assert_eq!(request.peer, Some("192.0.2.7".parse().unwrap()));
            request.password.fill(0);
            ready.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            socket.write_all(b"NRB2\x01").unwrap();
            listener.set_nonblocking(true).unwrap();
            assert!(listener.accept().is_err());
        });
        let throttle = LoginThrottle::default();
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        // An earlier free failure: this rejection starts the backoff.
        throttle.record_failure(source);
        let mut login = Login::new(
            path,
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            "alice",
        )
        .with_throttle(throttle.clone(), Some(source));
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
        assert_eq!(state.failure, Some(LoginFailure::Rejected));
        assert!(state.password.0.is_empty());
        assert!(state.desktop.is_none());
        assert_eq!(state.attempts, 1);
        drop(state);
        worker.join().unwrap();
        // The rejection counts against the source, not just this form.
        assert!(throttle.blocked(source));
    }

    #[tokio::test]
    async fn login_screen_backoff_rejects_generically_without_contacting_pam() {
        let throttle = LoginThrottle::default();
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        throttle.record_failure(source);
        throttle.record_failure(source);
        // A nonexistent broker would surface as DesktopUnavailable if contacted.
        let form = login("alice").with_throttle(throttle.clone(), Some(source));
        for attempt in 1..=5 {
            form.state.lock().unwrap().retry_at = Instant::now();
            form.character('x');
            form.submit();
            let state = form.state.lock().unwrap();
            assert!(!state.busy);
            assert_eq!(state.failure, Some(LoginFailure::Rejected));
            assert_eq!(state.attempts, attempt);
            assert!(state.password.0.is_empty());
            assert!(state.retry_at > Instant::now());
            assert_eq!(state.stopped, attempt == 5);
        }
        // Rejections during backoff do not extend it.
        assert_eq!(throttle.len(), 1);
        // Without a source, the same form would reach the (missing) broker.
        let unthrottled = login("alice").with_throttle(throttle, None);
        unthrottled.character('x');
        unthrottled.submit();
        // The startup thread may finish before we look, so wait for its
        // result: only a broker contact can report DesktopUnavailable.
        let deadline = Instant::now() + Duration::from_secs(2);
        while unthrottled.state.lock().unwrap().busy {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let state = unthrottled.state.lock().unwrap();
        assert_eq!(state.failure, Some(LoginFailure::DesktopUnavailable));
        assert_eq!(state.attempts, 0);
    }

    #[test]
    fn login_limits_parse_strict_bounds() {
        assert_eq!(
            LoginLimits::parse(None, None).unwrap(),
            LoginLimits::default()
        );
        assert_eq!(
            LoginLimits::parse(Some("10"), Some("30")).unwrap(),
            LoginLimits {
                idle: Duration::from_secs(10),
                total: Duration::from_secs(30),
            }
        );
        assert_eq!(
            LoginLimits::parse(Some("600"), Some("1800")).unwrap().total,
            Duration::from_secs(1800)
        );
        for (idle, total) in [
            (Some("9"), None),
            (Some("601"), Some("1800")),
            (None, Some("29")),
            (None, Some("1801")),
            (Some("0"), None),
            (Some("-1"), None),
            (Some(" 60"), None),
            (Some("60s"), None),
            (Some(""), None),
            (Some("90"), Some("60")),
            (Some("121"), None),
        ] {
            assert!(
                LoginLimits::parse(idle, total).is_err(),
                "{idle:?} {total:?} accepted"
            );
        }
    }

    #[test]
    fn deadline_is_earliest_of_idle_and_total_and_pauses_while_busy() {
        let limits = LoginLimits {
            idle: Duration::from_secs(60),
            total: Duration::from_secs(120),
        };
        let login = login("alice");
        let mut state = login.state.lock().unwrap();
        let opened = state.opened;
        state.last_submit = opened;
        assert_eq!(state.deadline(limits), Some(opened + limits.idle));
        state.last_submit = opened + Duration::from_secs(50);
        assert_eq!(
            state.deadline(limits),
            Some(opened + Duration::from_secs(110))
        );
        state.last_submit = opened + Duration::from_secs(100);
        assert_eq!(state.deadline(limits), Some(opened + limits.total));
        state.busy = true;
        assert_eq!(state.deadline(limits), None);
    }

    fn short_limits(idle_ms: u64, total_ms: u64) -> LoginLimits {
        LoginLimits {
            idle: Duration::from_millis(idle_ms),
            total: Duration::from_millis(total_ms),
        }
    }

    #[tokio::test]
    async fn idle_login_screen_disconnects_at_deadline_even_when_suppressed() {
        for suppress in [false, true] {
            let started = tokio::time::Instant::now();
            let mut login = login("alice").with_limits(short_limits(100, 10_000));
            let mut updates = login.updates().await.unwrap();
            assert!(updates.next_update().await.unwrap().is_some());
            if suppress {
                login.set_output_suppressed(true);
            }
            // Typing is not a submit and does not extend the idle deadline.
            login.character('a');
            let end = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if updates.next_update().await.unwrap().is_none() {
                        break;
                    }
                }
            })
            .await;
            assert!(end.is_ok(), "idle login form was not closed");
            assert!(started.elapsed() >= Duration::from_millis(100));
            let state = login.state.lock().unwrap();
            assert!(state.stopped);
            assert!(state.password.0.is_empty());
        }
    }

    #[tokio::test]
    async fn submits_extend_idle_but_not_total_login_deadline() {
        let started = tokio::time::Instant::now();
        let mut login = login("").with_limits(short_limits(150, 450));
        let mut updates = login.updates().await.unwrap();
        let submitter = login.clone();
        let poke = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(40)).await;
                // An empty form still counts as a submitted attempt.
                submitter.submit();
            }
        });
        let end = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if updates.next_update().await.unwrap().is_none() {
                    break;
                }
            }
        })
        .await;
        poke.abort();
        assert!(end.is_ok(), "total login deadline was not enforced");
        assert!(started.elapsed() >= Duration::from_millis(450));
        assert!(login.state.lock().unwrap().stopped);
    }

    #[tokio::test]
    async fn desktop_startup_failure_is_distinct_and_does_not_consume_auth_attempts() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..6 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = broker::read_test_request(&mut socket);
                assert_eq!(request.peer, None);
                request.password.fill(0);
                // PAM accepted, but the broker's desktop worker failed.
                socket.write_all(b"NRB2\x02").unwrap();
            }
        });
        let login = Login::new(
            path,
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            "alice",
        );
        for _ in 0..6 {
            login.state.lock().unwrap().retry_at = Instant::now();
            login.character('a');
            login.submit();
            let deadline = Instant::now() + Duration::from_secs(2);
            while login.state.lock().unwrap().busy {
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let state = login.state.lock().unwrap();
            assert_eq!(state.failure, Some(LoginFailure::DesktopUnavailable));
            assert_eq!(state.attempts, 0);
            assert!(!state.stopped);
            assert!(state.password.0.is_empty());
            assert!(state.desktop.is_none());
            assert!(state.retry_at > Instant::now());
        }
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn authenticated_denials_explain_themselves_without_consuming_attempts() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            for status in [4u8, 5, 4] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = broker::read_test_request(&mut socket);
                assert_eq!(request.peer, Some("192.0.2.7".parse().unwrap()));
                request.password.fill(0);
                socket.write_all(&[b'N', b'R', b'B', b'2', status]).unwrap();
            }
        });
        let throttle = LoginThrottle::default();
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        throttle.record_failure(source);
        let login = Login::new(
            path,
            "HEADLESS-1".into(),
            None,
            DesktopSize {
                width: 800,
                height: 600,
            },
            "alice",
        )
        .with_throttle(throttle.clone(), Some(source));
        for expected in [
            LoginFailure::DesktopInUse,
            LoginFailure::LocalSession,
            LoginFailure::DesktopInUse,
        ] {
            login.state.lock().unwrap().retry_at = Instant::now();
            login.character('a');
            login.submit();
            let deadline = Instant::now() + Duration::from_secs(2);
            while login.state.lock().unwrap().busy {
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let state = login.state.lock().unwrap();
            assert_eq!(state.failure, Some(expected));
            assert_ne!(state.failure, Some(LoginFailure::Rejected));
            assert_eq!(state.attempts, 0);
            assert!(!state.stopped);
            assert!(state.password.0.is_empty());
            assert!(state.desktop.is_none());
            assert!(state.retry_at > Instant::now());
        }
        worker.join().unwrap();
        // PAM accepted the password, so the source's earlier failure is cleared.
        assert_eq!(throttle.len(), 0);
        assert_eq!(
            LoginFailure::from(broker::Denial::Rejected),
            LoginFailure::Rejected
        );
        // Messages fit the form beside the keyboard hint.
        for failure in [LoginFailure::DesktopInUse, LoginFailure::LocalSession] {
            assert!(failure.message().len() <= 41);
        }
    }
}
