//! A bounded local broker protocol; no passwords appear in command lines/logs.
use crate::{login::Login, server_error, video::Video, wayland::Desktop};
use ironrdp_server::{
    BoundConnection, ConnectionBinder, CredentialDecision, CredentialValidationError,
    CredentialValidator, Credentials, DesktopSize, ServerResult,
};
use std::{
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

pub struct Session {
    pub socket: UnixStream,
    pub lease: UnixStream,
    pub audio: Option<UnixStream>,
}
pub fn open(path: &Path, username: &str, password: &str) -> anyhow::Result<Option<Session>> {
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
    let mut bytes = Vec::with_capacity(8 + username.len() + password.len());
    bytes.extend_from_slice(b"NRB1");
    bytes.extend_from_slice(&(username.len() as u16).to_be_bytes());
    bytes.extend_from_slice(&(password.len() as u16).to_be_bytes());
    bytes.extend_from_slice(username.as_bytes());
    bytes.extend_from_slice(password.as_bytes());
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
    anyhow::ensure!(&response[..4] == b"NRB1", "invalid broker response magic");
    if response[4] == 1 {
        return Ok(None);
    }
    anyhow::ensure!(response[4] == 0, "broker session creation failed");
    anyhow::ensure!(
        (1..=2).contains(&fds.len()),
        "broker must supply Wayland and optional PCM FDs"
    );
    lease.set_read_timeout(None)?;
    lease.set_write_timeout(None)?;
    let audio = if fds.len() == 2 {
        Some(UnixStream::from(fds.pop().unwrap()))
    } else {
        None
    };
    Ok(Some(Session {
        audio,
        socket: UnixStream::from(fds.pop().unwrap()),
        lease,
    }))
}

pub struct Broker {
    path: PathBuf,
    pending: Mutex<Option<(String, Session)>>,
    output: String,
    video: Option<Video>,
    active: Mutex<Option<Desktop>>,
    login: Mutex<Option<Login>>,
    audio: crate::audio::Audio,
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
        }
    }
    pub fn with_audio(mut self, audio: crate::audio::Audio) -> Self {
        self.audio = audio;
        self
    }
    pub fn discard(&self) {
        if let Some(login) = self.login.lock().unwrap().take() {
            login.stop();
        }
        self.audio.clear();
        self.pending.lock().unwrap().take();
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
        let path = self.path.clone();
        let username = c.username.clone();
        let password = c.password.clone();
        let result = tokio::task::spawn_blocking(move || open(&path, &username, &password))
            .await
            .map_err(CredentialValidationError::new)?;
        match result {
            Ok(Some(session)) => {
                *self.pending.lock().unwrap() = Some((c.username.clone(), session));
                Ok(CredentialDecision::Accept)
            }
            Ok(None) => Ok(CredentialDecision::Reject),
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
        if requires_login_screen(c) {
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
            let login = login.with_audio(self.audio.clone());
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
        let desktop = tokio::task::spawn_blocking(move || {
            Desktop::start(session.socket, output, size, Some(session.lease))
        })
        .await
        .map_err(|_| server_error("desktop startup task failed"))?
        .map_err(|_| server_error("Wayland desktop failed"))?;
        self.audio.attach(audio_socket);
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
        thread::JoinHandle<()>,
        UnixStream,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (observer, peer) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut head = [0; 8];
            client.read_exact(&mut head).unwrap();
            assert_eq!(&head[..4], b"NRB1");
            let user = u16::from_be_bytes([head[4], head[5]]) as usize;
            let password = u16::from_be_bytes([head[6], head[7]]) as usize;
            let mut credentials = vec![0; user + password];
            client.read_exact(&mut credentials).unwrap();
            assert_eq!(credentials, b"aliceexample");
            credentials.fill(0);
            let response = [b'N', b'R', b'B', b'1', status];
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
        });
        (dir, path, handle, observer)
    }
    #[test]
    fn fragmented_reply_retains_cloexec_fd_and_broker_lease() {
        for late_fd in [false, true] {
            let (mut compositor, bridge) = UnixStream::pair().unwrap();
            let fd: OwnedFd = bridge.into();
            let (_dir, path, handle, _observer) = fake(0, vec![fd], late_fd);
            let mut session = open(&path, "alice", "example").unwrap().unwrap();
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
            let mut session = open(&path, "alice", "example").unwrap().unwrap();
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
    fn excess_capabilities_are_rejected() {
        let mut descriptors = Vec::new();
        for _ in 0..3 {
            descriptors.push(UnixStream::pair().unwrap().0.into());
        }
        let (_dir, path, handle, _) = fake(0, descriptors, false);
        assert!(open(&path, "alice", "example").is_err());
        handle.join().unwrap();
    }
    #[test]
    fn rejection_creates_no_session() {
        let (_dir, path, handle, _observer) = fake(1, vec![], false);
        assert!(open(&path, "alice", "example").unwrap().is_none());
        handle.join().unwrap();
    }
    #[test]
    fn malformed_credentials_do_not_connect() {
        for (u, p) in [("", "pass"), ("alice", ""), ("a\0b", "pass")] {
            assert!(open(Path::new("/nonexistent"), u, p).is_err());
        }
    }
}
