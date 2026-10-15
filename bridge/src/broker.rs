//! A bounded local broker protocol; no passwords appear in command lines/logs.
use crate::{
    login::{Login, LoginLimits},
    server_error,
    video::Video,
    wayland::Desktop,
};
use ironrdp_server::{
    BoundConnection, ConnectionBinder, CredentialDecision, CredentialValidationError,
    CredentialValidator, Credentials, DesktopSize, ServerResult,
};
use std::{
    collections::HashMap,
    io::Write,
    net::{IpAddr, Ipv6Addr},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(60);
const FAILURE_EXPIRY: Duration = Duration::from_secs(15 * 60);
const MAX_TRACKED_SOURCES: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct SourceFailures {
    failures: u32,
    last: Instant,
    blocked_until: Instant,
}
/// Per-source exponential backoff after rejected PAM logins, shared by the
/// login screen and ClientInfo credentials. It lives in the frontend process,
/// so reconnecting does not reset it; the broker's own PAM policy still runs.
#[derive(Clone, Default)]
pub struct LoginThrottle(Arc<Mutex<HashMap<IpAddr, SourceFailures>>>);
/// One IPv6 host normally controls its whole /64, so it is one source.
fn source_key(source: IpAddr) -> IpAddr {
    match source {
        IpAddr::V4(_) => source,
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(address.to_bits() & !((1u128 << 64) - 1))),
        },
    }
}
/// The first failure is free, so one mistyped password followed by an
/// immediate retry still works; later failures double from one second.
fn backoff(failures: u32) -> Duration {
    if failures <= 1 {
        return Duration::ZERO;
    }
    BACKOFF_BASE
        .saturating_mul(1u32 << (failures - 2).min(16))
        .min(BACKOFF_CAP)
}
impl LoginThrottle {
    pub fn blocked(&self, source: IpAddr) -> bool {
        self.blocked_at(source, Instant::now())
    }
    pub fn record_failure(&self, source: IpAddr) {
        self.record_failure_at(source, Instant::now());
    }
    pub fn record_success(&self, source: IpAddr) {
        self.0.lock().unwrap().remove(&source_key(source));
    }
    fn blocked_at(&self, source: IpAddr, now: Instant) -> bool {
        let mut entries = self.0.lock().unwrap();
        let key = source_key(source);
        match entries.get(&key) {
            Some(entry) if now.saturating_duration_since(entry.last) >= FAILURE_EXPIRY => {
                entries.remove(&key);
                false
            }
            Some(entry) => now < entry.blocked_until,
            None => false,
        }
    }
    /// Returns the backoff now applied to this source.
    fn record_failure_at(&self, source: IpAddr, now: Instant) -> Duration {
        let mut entries = self.0.lock().unwrap();
        let key = source_key(source);
        entries.retain(|_, entry| now.saturating_duration_since(entry.last) < FAILURE_EXPIRY);
        if !entries.contains_key(&key)
            && entries.len() >= MAX_TRACKED_SOURCES
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last)
                .map(|(key, _)| *key)
        {
            entries.remove(&oldest);
        }
        let entry = entries.entry(key).or_insert(SourceFailures {
            failures: 0,
            last: now,
            blocked_until: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last = now;
        let delay = backoff(entry.failures);
        entry.blocked_until = now + delay;
        delay
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

pub struct Session {
    pub socket: UnixStream,
    pub lease: UnixStream,
    pub audio: Option<UnixStream>,
    /// The session's `audio-in-0` microphone helper. The broker sends it only
    /// together with the playback descriptor, so position identifies it.
    pub microphone: Option<UnixStream>,
}
/// Broker wire contract v2 (broker/broker.c). The request carries the RDP
/// client's address for PAM_RHOST and the audit log; the frontend and broker
/// must be upgraded together.
const MAGIC: &[u8; 4] = b"NRB2";
/// Why the broker opened no desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// Any authentication, account or policy failure (status 1).
    Rejected,
    /// PAM accepted the password, but this account's desktop is attached
    /// to another connection (status 4).
    AlreadyAttached,
    /// PAM accepted the password, but the account has a local graphical
    /// session, perhaps one that is still closing (status 5).
    LocalSession,
}
impl Denial {
    fn from_status(status: u8) -> Option<Self> {
        match status {
            1 => Some(Self::Rejected),
            4 => Some(Self::AlreadyAttached),
            5 => Some(Self::LocalSession),
            _ => None,
        }
    }
    /// The broker sends every other denial only after PAM succeeded.
    pub fn authenticated(self) -> bool {
        self != Self::Rejected
    }
}
/// Magic, user and password lengths (u16 big endian), the client address
/// family (0 unknown, 4 or 6) and 0, 4 or 16 address bytes, then the user
/// name and password.
fn encode_request(username: &str, password: &str, peer: Option<IpAddr>) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(9 + 16 + username.len() + password.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(username.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&(password.len() as u16).to_be_bytes());
    // An IPv4 client on a dual-stack socket is logged as IPv4.
    match peer.map(|peer| peer.to_canonical()) {
        None => bytes.push(0),
        Some(IpAddr::V4(address)) => {
            bytes.push(4);
            bytes.extend_from_slice(&address.octets());
        }
        Some(IpAddr::V6(address)) => {
            bytes.push(6);
            bytes.extend_from_slice(&address.octets());
        }
    }
    bytes.extend_from_slice(username.as_bytes());
    bytes.extend_from_slice(password.as_bytes());
    bytes
}
pub fn open(
    path: &Path,
    username: &str,
    password: &str,
    peer: Option<IpAddr>,
) -> anyhow::Result<Result<Session, Denial>> {
    anyhow::ensure!(
        (1..=64).contains(&username.len())
            && (1..=512).contains(&password.len())
            && !username.contains('\0')
            && !password.contains('\0'),
        "invalid broker credential lengths"
    );
    let mut lease = UnixStream::connect(path)?;
    lease.set_read_timeout(Some(Duration::from_secs(30)))?;
    lease.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut bytes = encode_request(username, password, peer);
    let sent = lease.write_all(&bytes);
    bytes.fill(0);
    sent?;
    let mut response = [0u8; 5];
    // Alignment is required by cmsghdr. Reserve room for several unexpected
    // descriptors so all received descriptors can be closed on rejection.
    let mut control = [0usize; 16];
    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut received = 0;
    while received < response.len() {
        let mut iov = libc::iovec {
            iov_base: response[received..].as_mut_ptr().cast(),
            iov_len: response.len() - received,
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&control);
        // Every fragment must use recvmsg: read_exact would silently discard
        // SCM_RIGHTS attached to a later fragment of the stream response.
        // SAFETY: buffers remain live, and received FDs become owned immediately.
        let n = unsafe { libc::recvmsg(lease.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        anyhow::ensure!(n > 0, "broker response unavailable");
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize)
                        / std::mem::size_of::<libc::c_int>();
                    let ptr = libc::CMSG_DATA(c).cast::<libc::c_int>();
                    for i in 0..count {
                        fds.push(OwnedFd::from_raw_fd(*ptr.add(i)));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        anyhow::ensure!(
            msg.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) == 0,
            "truncated broker response"
        );
        received += n as usize;
    }
    anyhow::ensure!(&response[..4] == MAGIC, "invalid broker response magic");
    if let Some(denial) = Denial::from_status(response[4]) {
        return Ok(Err(denial));
    }
    anyhow::ensure!(response[4] == 0, "broker session creation failed");
    // Positional: [Wayland], [Wayland, playback] or [Wayland, playback,
    // microphone]. Rejected replies close every received descriptor.
    anyhow::ensure!(
        (1..=3).contains(&fds.len()),
        "broker must supply Wayland and optional playback and microphone FDs"
    );
    lease.set_read_timeout(None)?;
    lease.set_write_timeout(None)?;
    let mut fds = fds.into_iter().map(UnixStream::from);
    Ok(Ok(Session {
        socket: fds.next().unwrap(),
        audio: fds.next(),
        microphone: fds.next(),
        lease,
    }))
}
/// A request as the broker parses it, for fake brokers in tests.
#[cfg(test)]
pub(crate) struct TestRequest {
    pub username: Vec<u8>,
    pub password: Vec<u8>,
    pub peer: Option<IpAddr>,
}
#[cfg(test)]
pub(crate) fn read_test_request(stream: &mut impl std::io::Read) -> TestRequest {
    let mut head = [0; 9];
    stream.read_exact(&mut head).unwrap();
    assert_eq!(&head[..4], MAGIC);
    let mut address = vec![
        0;
        match head[8] {
            0 => 0,
            4 => 4,
            6 => 16,
            family => panic!("unknown address family {family}"),
        }
    ];
    stream.read_exact(&mut address).unwrap();
    let peer = match address.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(address).unwrap())),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(address).unwrap())),
        _ => None,
    };
    let mut username = vec![0; u16::from_be_bytes([head[4], head[5]]).into()];
    let mut password = vec![0; u16::from_be_bytes([head[6], head[7]]).into()];
    stream.read_exact(&mut username).unwrap();
    stream.read_exact(&mut password).unwrap();
    TestRequest {
        username,
        password,
        peer,
    }
}

pub struct Broker {
    path: PathBuf,
    pending: Mutex<Option<(String, Session)>>,
    output: String,
    video: Option<Video>,
    active: Mutex<Option<Desktop>>,
    login: Mutex<Option<Login>>,
    audio: crate::audio::Audio,
    /// `None` with `NIRI_RDP_MICROPHONE=off`: the descriptor is then dropped.
    microphone: Option<crate::microphone::Microphone>,
    clipboard: crate::clipboard::Clipboard,
    limits: LoginLimits,
    throttle: LoginThrottle,
    // ConnectionPolicy::Reject serves one connection at a time, so the
    // accepted peer identifies the connection being validated and bound.
    peer: Mutex<Option<IpAddr>>,
    // Why authenticated ClientInfo credentials opened no desktop; the login
    // screen bound next explains it.
    notice: Mutex<Option<Denial>>,
}
fn requires_login_screen(credentials: &Credentials) -> bool {
    credentials.password.is_empty()
        || credentials
            .domain
            .as_ref()
            .is_some_and(|domain| !domain.is_empty())
}
impl Broker {
    pub fn new(path: PathBuf, output: String, video: Option<Video>) -> Self {
        Self {
            path,
            pending: Mutex::new(None),
            output,
            video,
            active: Mutex::new(None),
            login: Mutex::new(None),
            audio: Default::default(),
            microphone: None,
            clipboard: Default::default(),
            limits: LoginLimits::default(),
            throttle: LoginThrottle::default(),
            peer: Mutex::new(None),
            notice: Mutex::new(None),
        }
    }
    pub fn with_login_limits(mut self, limits: LoginLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Record the peer of the connection now being served, or clear it.
    pub fn begin_connection(&self, peer: Option<IpAddr>) {
        *self.peer.lock().unwrap() = peer;
    }
    pub fn current_peer(&self) -> Option<IpAddr> {
        *self.peer.lock().unwrap()
    }
    pub fn with_audio(mut self, audio: crate::audio::Audio) -> Self {
        self.audio = audio;
        self
    }
    pub fn with_clipboard(mut self, clipboard: crate::clipboard::Clipboard) -> Self {
        self.clipboard = clipboard;
        self
    }
    pub fn with_microphone(mut self, microphone: Option<crate::microphone::Microphone>) -> Self {
        self.microphone = microphone;
        self
    }
    pub fn discard(&self) {
        if let Some(login) = self.login.lock().unwrap().take() {
            login.stop();
        }
        self.audio.clear();
        if let Some(microphone) = &self.microphone {
            microphone.clear();
        }
        self.clipboard.clear();
        self.pending.lock().unwrap().take();
        self.notice.lock().unwrap().take();
        if let Some(desktop) = self.active.lock().unwrap().take() {
            desktop.stop();
        }
    }
}
#[async_trait::async_trait]
impl CredentialValidator for Broker {
    async fn validate(
        &self,
        c: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        self.discard();
        // A TLS client may omit credentials or send its remembered Windows
        // domain credentials. Neither identifies an authorized Linux account.
        // Bind only the restricted form, without passing those secrets to PAM.
        if requires_login_screen(c) {
            return Ok(CredentialDecision::Accept);
        }
        let source = self.current_peer();
        // A source in backoff gets the ordinary rejection without PAM.
        if source.is_some_and(|source| self.throttle.blocked(source)) {
            return Ok(CredentialDecision::Reject);
        }
        let path = self.path.clone();
        let username = c.username.clone();
        let password = c.password.clone();
        let result = tokio::task::spawn_blocking(move || open(&path, &username, &password, source))
            .await
            .map_err(CredentialValidationError::new)?;
        match result {
            Ok(Ok(session)) => {
                if let Some(source) = source {
                    self.throttle.record_success(source);
                }
                *self.pending.lock().unwrap() = Some((c.username.clone(), session));
                Ok(CredentialDecision::Accept)
            }
            Ok(Err(Denial::Rejected)) => {
                if let Some(source) = source {
                    self.throttle.record_failure(source);
                }
                Ok(CredentialDecision::Reject)
            }
            // PAM accepted the password, but no desktop can open now. A bare
            // rejection would look like a wrong password, so bind the login
            // screen with the reason instead.
            Ok(Err(denial)) => {
                if let Some(source) = source {
                    self.throttle.record_success(source);
                }
                *self.notice.lock().unwrap() = Some(denial);
                Ok(CredentialDecision::Accept)
            }
            Err(_) => Err(CredentialValidationError::new(std::io::Error::other(
                "broker unavailable or session startup failed",
            ))),
        }
    }
}
#[async_trait::async_trait]
impl ConnectionBinder for Broker {
    async fn bind_connection(
        &self,
        c: &Credentials,
        size: DesktopSize,
    ) -> ServerResult<BoundConnection> {
        let notice = self.notice.lock().unwrap().take();
        if requires_login_screen(c) || notice.is_some() {
            let login = Login::new(
                self.path.clone(),
                self.output.clone(),
                self.video.clone(),
                size,
                if c.domain.as_ref().is_none_or(|d| d.is_empty()) {
                    &c.username
                } else {
                    ""
                },
            );
            let login = login
                .with_audio(self.audio.clone())
                .with_microphone(self.microphone.clone())
                .with_clipboard(self.clipboard.clone())
                .with_limits(self.limits)
                .with_throttle(self.throttle.clone(), self.current_peer())
                .with_notice(notice);
            let bound = login.bound();
            *self.login.lock().unwrap() = Some(login);
            return Ok(bound);
        }
        let (name, session) = self
            .pending
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| server_error("no authenticated broker session"))?;
        if name != c.username {
            return Err(server_error("authenticated session identity mismatch"));
        }
        let output = self.output.clone();
        let audio_socket = session.audio;
        let microphone_socket = session.microphone;
        let desktop = tokio::task::spawn_blocking(move || {
            Desktop::start(session.socket, output, size, Some(session.lease))
        })
        .await
        .map_err(|_| server_error("desktop startup task failed"))?
        .map_err(|_| server_error("Wayland desktop failed"))?;
        self.audio.attach(audio_socket);
        if let Some(microphone) = &self.microphone {
            microphone.attach(microphone_socket);
        }
        desktop.attach_clipboard(self.clipboard.clone());
        *self.active.lock().unwrap() = Some(desktop.clone());
        let display = match &self.video {
            Some(video) => video.display(desktop.clone()),
            None => Box::new(desktop.clone()),
        };
        Ok(BoundConnection {
            display,
            input: Box::new(desktop),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::net::UnixListener, thread};
    #[tokio::test]
    async fn remembered_windows_domain_credentials_bind_only_empty_login_form() {
        let broker = Broker::new(
            PathBuf::from("/nonexistent-niri-rdp-test.sock"),
            "HEADLESS-1".into(),
            None,
        );
        let credentials = Credentials {
            username: "windows-user".into(),
            password: "remembered-windows-secret".into(),
            domain: Some("WINDOWS-PC".into()),
        };
        assert!(matches!(
            broker.validate(&credentials).await.unwrap(),
            CredentialDecision::Accept
        ));
        let size = DesktopSize {
            width: 800,
            height: 600,
        };
        let mut bound = broker.bind_connection(&credentials, size).await.unwrap();
        assert!(broker.pending.lock().unwrap().is_none());
        assert!(broker.active.lock().unwrap().is_none());
        let mut updates = bound.display.updates().await.unwrap();
        let frame = updates.next_update().await.unwrap();
        let mut empty = Login::new(PathBuf::new(), "HEADLESS-1".into(), None, size, "").bound();
        let mut empty_updates = empty.display.updates().await.unwrap();
        match (frame, empty_updates.next_update().await.unwrap()) {
            (
                Some(ironrdp_server::DisplayUpdate::Bitmap(actual)),
                Some(ironrdp_server::DisplayUpdate::Bitmap(expected)),
            ) => assert_eq!(actual.data, expected.data),
            _ => panic!("restricted login form did not render"),
        }
        broker.discard();
    }
    #[tokio::test]
    async fn omitted_password_binds_only_login_screen_without_contacting_broker() {
        let broker = Broker::new(
            PathBuf::from("/nonexistent-niri-rdp-test.sock"),
            "HEADLESS-1".into(),
            None,
        );
        let credentials = Credentials {
            username: "alice".into(),
            password: String::new(),
            domain: None,
        };
        assert!(matches!(
            broker.validate(&credentials).await.unwrap(),
            CredentialDecision::Accept
        ));
        let mut bound = broker
            .bind_connection(
                &credentials,
                DesktopSize {
                    width: 800,
                    height: 600,
                },
            )
            .await
            .unwrap();
        assert!(broker.pending.lock().unwrap().is_none());
        assert!(broker.active.lock().unwrap().is_none());
        assert!(broker.login.lock().unwrap().is_some());
        let mut updates = bound.display.updates().await.unwrap();
        assert!(updates.next_update().await.unwrap().is_some());
        broker.discard();
        assert!(updates.next_update().await.unwrap().is_none());
    }
    fn fake(
        status: u8,
        fds: Vec<OwnedFd>,
        late_fd: bool,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        thread::JoinHandle<Option<IpAddr>>,
        UnixStream,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (observer, peer) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut request = read_test_request(&mut client);
            assert_eq!(request.username, b"alice");
            assert_eq!(request.password, b"example");
            request.password.fill(0);
            let response = [b'N', b'R', b'B', b'2', status];
            if late_fd {
                client.write_all(&response[..2]).unwrap();
                // Force the frontend to consume the prefix before SCM_RIGHTS
                // arrives, exercising ancillary data on a later fragment.
                thread::sleep(Duration::from_millis(50));
            }
            let mut iov = libc::iovec {
                iov_base: response[if late_fd { 2 } else { 0 }..]
                    .as_ptr()
                    .cast_mut()
                    .cast(),
                iov_len: 3,
            };
            let mut ctl = [0usize; 8];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if !fds.is_empty() {
                let size = (std::mem::size_of::<libc::c_int>() * fds.len()) as u32;
                msg.msg_control = ctl.as_mut_ptr().cast();
                msg.msg_controllen = unsafe { libc::CMSG_SPACE(size) } as usize;
                unsafe {
                    let c = libc::CMSG_FIRSTHDR(&msg);
                    (*c).cmsg_level = libc::SOL_SOCKET;
                    (*c).cmsg_type = libc::SCM_RIGHTS;
                    (*c).cmsg_len = libc::CMSG_LEN(size) as usize;
                    for (index, fd) in fds.iter().enumerate() {
                        *libc::CMSG_DATA(c).cast::<libc::c_int>().add(index) = fd.as_raw_fd();
                    }
                }
            }
            assert_eq!(unsafe { libc::sendmsg(client.as_raw_fd(), &msg, 0) }, 3);
            if !late_fd {
                client.write_all(&response[3..]).unwrap();
            }
            drop(peer);
            let mut byte = [0; 1];
            assert_eq!(
                client.read(&mut byte).unwrap(),
                0,
                "frontend must close broker lease on cleanup"
            );
            request.peer
        });
        (dir, path, handle, observer)
    }
    #[test]
    fn fragmented_reply_retains_cloexec_fd_and_broker_lease() {
        for late_fd in [false, true] {
            let (mut compositor, bridge) = UnixStream::pair().unwrap();
            let fd: OwnedFd = bridge.into();
            let (_dir, path, handle, _observer) = fake(0, vec![fd], late_fd);
            let mut session = open(&path, "alice", "example", None).unwrap().unwrap();
            assert!(
                unsafe { libc::fcntl(session.socket.as_raw_fd(), libc::F_GETFD) }
                    & libc::FD_CLOEXEC
                    != 0
            );
            compositor.write_all(b"w").unwrap();
            let mut b = [0];
            session.socket.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"w");
            drop(session);
            handle.join().unwrap();
        }
    }
    #[test]
    fn audio_handoff_preserves_fd_order_and_cloexec_on_fragmented_response() {
        for late_fd in [false, true] {
            let (mut compositor, wayland) = UnixStream::pair().unwrap();
            let (mut monitor, audio) = UnixStream::pair().unwrap();
            let (_dir, path, handle, _) = fake(0, vec![wayland.into(), audio.into()], late_fd);
            let mut session = open(&path, "alice", "example", None).unwrap().unwrap();
            let mut audio = session.audio.take().unwrap();
            assert_ne!(
                unsafe { libc::fcntl(audio.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            compositor.write_all(b"W").unwrap();
            monitor.write_all(b"A").unwrap();
            let mut byte = [0];
            session.socket.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"W");
            audio.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"A");
            drop(audio);
            drop(session);
            handle.join().unwrap();
        }
    }
    #[test]
    fn handoff_accepts_one_two_or_three_descriptors_in_order_with_cloexec() {
        for late_fd in [false, true] {
            let (mut compositor, wayland) = UnixStream::pair().unwrap();
            let (mut monitor, audio) = UnixStream::pair().unwrap();
            let (mut helper, microphone) = UnixStream::pair().unwrap();
            let (_dir, path, handle, _) = fake(
                0,
                vec![wayland.into(), audio.into(), microphone.into()],
                late_fd,
            );
            let mut session = open(&path, "alice", "example", None).unwrap().unwrap();
            let mut audio = session.audio.take().unwrap();
            let mut microphone = session.microphone.take().unwrap();
            for stream in [&session.socket, &audio, &microphone] {
                assert_ne!(
                    unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
            }
            compositor.write_all(b"W").unwrap();
            monitor.write_all(b"A").unwrap();
            helper.write_all(b"M").unwrap();
            let mut byte = [0];
            session.socket.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"W");
            audio.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"A");
            microphone.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"M");
            drop((audio, microphone, session));
            handle.join().unwrap();
        }
        // One or two descriptors carry no microphone.
        let (_compositor, wayland) = UnixStream::pair().unwrap();
        let (_dir, path, handle, _) = fake(0, vec![wayland.into()], false);
        let session = open(&path, "alice", "example", None).unwrap().unwrap();
        assert!(session.audio.is_none() && session.microphone.is_none());
        drop(session);
        handle.join().unwrap();
        let (_compositor, wayland) = UnixStream::pair().unwrap();
        let (_monitor, audio) = UnixStream::pair().unwrap();
        let (_dir, path, handle, _) = fake(0, vec![wayland.into(), audio.into()], false);
        let session = open(&path, "alice", "example", None).unwrap().unwrap();
        assert!(session.audio.is_some() && session.microphone.is_none());
        drop(session);
        handle.join().unwrap();
    }
    #[test]
    fn four_descriptors_are_rejected_and_closed() {
        let (mut kept, sent) = UnixStream::pair().unwrap();
        let mut descriptors: Vec<OwnedFd> = vec![sent.into()];
        for _ in 0..3 {
            descriptors.push(UnixStream::pair().unwrap().0.into());
        }
        let (_dir, path, handle, _) = fake(0, descriptors, false);
        assert!(open(&path, "alice", "example", None).is_err());
        handle.join().unwrap();
        // The frontend kept no copy of the rejected descriptors.
        kept.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut byte = [0];
        assert_eq!(kept.read(&mut byte).unwrap(), 0);
    }
    #[test]
    fn rejection_creates_no_session() {
        let (_dir, path, handle, _observer) = fake(1, vec![], false);
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        assert_eq!(
            open(&path, "alice", "example", Some(source)).unwrap().err(),
            Some(Denial::Rejected)
        );
        // The broker received the client address for PAM_RHOST and auditing.
        assert_eq!(handle.join().unwrap(), Some(source));
    }
    #[test]
    fn post_authentication_denials_are_distinct_and_failures_stay_errors() {
        for (status, denial) in [(4, Denial::AlreadyAttached), (5, Denial::LocalSession)] {
            let (_dir, path, handle, _observer) = fake(status, vec![], false);
            assert_eq!(
                open(&path, "alice", "example", None).unwrap().err(),
                Some(denial)
            );
            assert!(denial.authenticated());
            handle.join().unwrap();
        }
        assert!(!Denial::Rejected.authenticated());
        // Startup failure, a malformed request and unknown codes are errors,
        // never a password verdict.
        for status in [2, 3, 6, 255] {
            let (_dir, path, handle, _observer) = fake(status, vec![], false);
            assert!(open(&path, "alice", "example", None).is_err());
            handle.join().unwrap();
        }
    }
    #[test]
    fn request_encodes_canonical_client_address() {
        let request =
            |peer: Option<&str>| encode_request("al", "pw", peer.map(|peer| peer.parse().unwrap()));
        assert_eq!(request(None), b"NRB2\0\x02\0\x02\0alpw");
        assert_eq!(
            request(Some("192.0.2.7")),
            b"NRB2\0\x02\0\x02\x04\xc0\0\x02\x07alpw"
        );
        // An IPv4 client accepted on a dual-stack socket is sent as IPv4.
        assert_eq!(
            request(Some("::ffff:192.0.2.7")),
            request(Some("192.0.2.7"))
        );
        let v6 = request(Some("2001:db8::1"));
        assert_eq!(&v6[..9], b"NRB2\0\x02\0\x02\x06");
        assert_eq!(
            &v6[9..25],
            "2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets()
        );
        assert_eq!(&v6[25..], b"alpw");
        let mut stream = std::io::Cursor::new(v6);
        let parsed = read_test_request(&mut stream);
        assert_eq!(parsed.peer, Some("2001:db8::1".parse().unwrap()));
        assert_eq!(
            (parsed.username, parsed.password),
            (b"al".to_vec(), b"pw".to_vec())
        );
    }
    #[tokio::test]
    async fn authenticated_denial_binds_login_screen_with_reason() {
        let (_dir, path, handle, _observer) = fake(4, vec![], false);
        let broker = Broker::new(path, "HEADLESS-1".into(), None);
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        broker.begin_connection(Some(source));
        broker.throttle.record_failure(source);
        let credentials = Credentials {
            username: "alice".into(),
            password: "example".into(),
            domain: None,
        };
        assert!(matches!(
            broker.validate(&credentials).await.unwrap(),
            CredentialDecision::Accept
        ));
        assert_eq!(handle.join().unwrap(), Some(source));
        // The password was right: the source's failures are cleared.
        assert_eq!(broker.throttle.len(), 0);
        let size = DesktopSize {
            width: 800,
            height: 600,
        };
        let _bound = broker.bind_connection(&credentials, size).await.unwrap();
        assert!(broker.notice.lock().unwrap().is_none());
        assert!(broker.active.lock().unwrap().is_none());
        let login = broker.login.lock().unwrap().clone().unwrap();
        assert_eq!(
            login.failure_message(),
            Some("Desktop in use by another connection.")
        );
        broker.discard();
        assert!(broker.login.lock().unwrap().is_none());
    }
    #[test]
    fn source_backoff_doubles_to_cap_and_success_clears_it() {
        let throttle = LoginThrottle::default();
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        let start = Instant::now();
        assert!(!throttle.blocked_at(source, start));
        let mut delays = Vec::new();
        for failure in 1..=9u32 {
            let now = start + Duration::from_secs(u64::from(failure) * 100);
            let delay = throttle.record_failure_at(source, now);
            delays.push(delay);
            if failure == 1 {
                // A single mistake does not delay an immediate retry.
                assert_eq!(delay, Duration::ZERO);
                assert!(!throttle.blocked_at(source, now));
                continue;
            }
            assert!(throttle.blocked_at(source, now));
            assert!(throttle.blocked_at(source, now + delay - Duration::from_millis(1)));
            assert!(!throttle.blocked_at(source, now + delay));
        }
        assert_eq!(
            delays,
            [0, 1, 2, 4, 8, 16, 32, 60, 60].map(Duration::from_secs)
        );
        assert_eq!(*delays.last().unwrap(), BACKOFF_CAP);
        assert_eq!(backoff(u32::MAX), BACKOFF_CAP);
        // Another source is independent, and a successful login clears it.
        assert!(!throttle.blocked_at(
            "192.0.2.8".parse().unwrap(),
            start + Duration::from_secs(800)
        ));
        throttle.record_success(source);
        assert_eq!(throttle.len(), 0);
        assert_eq!(throttle.record_failure_at(source, start), Duration::ZERO);
    }
    #[test]
    fn source_failures_expire_after_quiet_period() {
        let throttle = LoginThrottle::default();
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        let start = Instant::now();
        for _ in 0..8 {
            throttle.record_failure_at(source, start);
        }
        assert!(throttle.blocked_at(source, start + Duration::from_secs(59)));
        let later = start + FAILURE_EXPIRY;
        assert!(!throttle.blocked_at(source, later));
        assert_eq!(throttle.len(), 0);
        // The count restarts rather than continuing at the old cap.
        assert_eq!(throttle.record_failure_at(source, later), Duration::ZERO);
        assert_eq!(throttle.record_failure_at(source, later), BACKOFF_BASE);
        // Expired entries of other sources are pruned when a failure is recorded.
        let other: IpAddr = "192.0.2.9".parse().unwrap();
        throttle.record_failure_at(other, later + FAILURE_EXPIRY);
        assert_eq!(throttle.len(), 1);
    }
    #[test]
    fn source_map_is_bounded_and_evicts_oldest_failure() {
        let throttle = LoginThrottle::default();
        let start = Instant::now();
        let address = |index: usize| IpAddr::from([10, 0, (index >> 8) as u8, index as u8]);
        for index in 0..MAX_TRACKED_SOURCES {
            throttle.record_failure_at(address(index), start + Duration::from_millis(index as u64));
        }
        assert_eq!(throttle.len(), MAX_TRACKED_SOURCES);
        let now = start + Duration::from_secs(10);
        // Refreshing the oldest source makes the second oldest the eviction victim.
        throttle.record_failure_at(address(0), now);
        assert_eq!(throttle.len(), MAX_TRACKED_SOURCES);
        throttle.record_failure_at("172.16.0.1".parse().unwrap(), now);
        assert_eq!(throttle.len(), MAX_TRACKED_SOURCES);
        assert!(throttle.blocked_at(address(0), now));
        let entries = throttle.0.lock().unwrap();
        assert!(!entries.contains_key(&address(1)));
        assert!(entries.contains_key(&address(2)));
        assert!(entries.contains_key(&"172.16.0.1".parse().unwrap()));
    }
    #[test]
    fn ipv6_prefix_and_mapped_ipv4_share_one_source() {
        let throttle = LoginThrottle::default();
        let now = Instant::now();
        throttle.record_failure_at("2001:db8:1:2::1".parse().unwrap(), now);
        throttle.record_failure_at("2001:db8:1:2::2".parse().unwrap(), now);
        assert!(throttle.blocked_at("2001:db8:1:2:ffff::9".parse().unwrap(), now));
        assert!(!throttle.blocked_at("2001:db8:1:3::1".parse().unwrap(), now));
        throttle.record_failure_at("::ffff:192.0.2.1".parse().unwrap(), now);
        throttle.record_failure_at("192.0.2.1".parse().unwrap(), now);
        assert!(throttle.blocked_at("::ffff:192.0.2.1".parse().unwrap(), now));
    }
    #[tokio::test]
    async fn direct_credentials_are_counted_and_backoff_rejects_without_pam() {
        let (_dir, path, handle, _observer) = fake(1, vec![], false);
        let broker = Broker::new(path, "HEADLESS-1".into(), None);
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        broker.begin_connection(Some(source));
        // An earlier free failure from this source; the next one blocks it.
        broker.throttle.record_failure(source);
        assert!(!broker.throttle.blocked(source));
        let credentials = Credentials {
            username: "alice".into(),
            password: "example".into(),
            domain: None,
        };
        assert!(matches!(
            broker.validate(&credentials).await.unwrap(),
            CredentialDecision::Reject
        ));
        handle.join().unwrap();
        assert!(broker.throttle.blocked(source));
        // The fake broker has gone: contacting it would be an error, so a
        // fast ordinary rejection proves PAM was not consulted.
        assert!(matches!(
            broker.validate(&credentials).await.unwrap(),
            CredentialDecision::Reject
        ));
        assert_eq!(broker.throttle.len(), 1);
        // A different source is unaffected and reaches the (absent) broker.
        broker.begin_connection(Some("192.0.2.8".parse().unwrap()));
        assert!(broker.validate(&credentials).await.is_err());
        // The login-screen form never consults PAM in validation.
        broker.begin_connection(Some(source));
        let empty = Credentials {
            username: "alice".into(),
            password: String::new(),
            domain: None,
        };
        assert!(matches!(
            broker.validate(&empty).await.unwrap(),
            CredentialDecision::Accept
        ));
    }
    #[tokio::test]
    async fn login_screen_inherits_source_throttle_and_limits() {
        let limits = LoginLimits {
            idle: Duration::from_secs(11),
            total: Duration::from_secs(31),
        };
        let broker = Broker::new(
            PathBuf::from("/nonexistent-niri-rdp-test.sock"),
            "HEADLESS-1".into(),
            None,
        )
        .with_login_limits(limits);
        let source: IpAddr = "192.0.2.7".parse().unwrap();
        broker.begin_connection(Some(source));
        let credentials = Credentials {
            username: "alice".into(),
            password: String::new(),
            domain: None,
        };
        let size = DesktopSize {
            width: 800,
            height: 600,
        };
        let _bound = broker.bind_connection(&credentials, size).await.unwrap();
        let login = broker.login.lock().unwrap().clone().unwrap();
        assert_eq!(login.limits(), limits);
        assert_eq!(login.source(), Some(source));
        broker.discard();
    }
    #[test]
    fn malformed_credentials_do_not_connect() {
        for (u, p) in [("", "pass"), ("alice", ""), ("a\0b", "pass")] {
            assert!(open(Path::new("/nonexistent"), u, p, None).is_err());
        }
    }
}
