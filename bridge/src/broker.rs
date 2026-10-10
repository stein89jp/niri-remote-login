//! A bounded local broker protocol; no passwords appear in command lines/logs.
use crate::{server_error, wayland::Desktop};
use ironrdp_server::{
    BoundConnection, ConnectionBinder, CredentialDecision, CredentialValidationError,
    CredentialValidator, Credentials, DesktopSize, ServerResult,
};
use std::{
    io::{Read, Write},
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
    let mut iov = libc::iovec {
        iov_base: response.as_mut_ptr().cast(),
        iov_len: response.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    // SAFETY: buffers remain live for recvmsg, and MSG_CMSG_CLOEXEC prevents
    // accidentally inheriting broker-created sockets into child processes.
    let n = unsafe { libc::recvmsg(lease.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    anyhow::ensure!(n > 0, "broker response unavailable");
    let mut fds: Vec<OwnedFd> = Vec::new();
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
    if n < 5 {
        lease.read_exact(&mut response[n as usize..])?;
    }
    anyhow::ensure!(&response[..4] == b"NRB1", "invalid broker response magic");
    if response[4] == 1 {
        return Ok(None);
    }
    anyhow::ensure!(response[4] == 0, "broker session creation failed");
    anyhow::ensure!(fds.len() == 1, "broker must supply exactly one Wayland FD");
    lease.set_read_timeout(None)?;
    lease.set_write_timeout(None)?;
    Ok(Some(Session {
        socket: UnixStream::from(fds.pop().unwrap()),
        lease,
    }))
}

pub struct Broker {
    path: PathBuf,
    pending: Mutex<Option<(String, Session)>>,
    output: String,
    active: Mutex<Option<Desktop>>,
}
impl Broker {
    pub fn new(path: PathBuf, output: String) -> Self {
        Self {
            path,
            pending: Mutex::new(None),
            output,
            active: Mutex::new(None),
        }
    }
    pub fn discard(&self) {
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
        // Only local Linux names are authorized; domain routing is unsupported.
        if c.domain.as_ref().is_some_and(|d| !d.is_empty()) {
            return Ok(CredentialDecision::Reject);
        }
        self.discard();
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
        let desktop = tokio::task::spawn_blocking(move || {
            Desktop::start(session.socket, output, size, Some(session.lease))
        })
        .await
        .map_err(|_| server_error("desktop startup task failed"))?
        .map_err(|_| server_error("Wayland desktop failed"))?;
        *self.active.lock().unwrap() = Some(desktop.clone());
        Ok(BoundConnection {
            display: Box::new(desktop.clone()),
            input: Box::new(desktop),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, thread};
    fn fake(
        status: u8,
        fd: Option<OwnedFd>,
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
            let mut iov = libc::iovec {
                iov_base: response.as_ptr().cast_mut().cast(),
                iov_len: 3,
            };
            let mut ctl = [0usize; 8];
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            if let Some(fd) = fd.as_ref() {
                msg.msg_control = ctl.as_mut_ptr().cast();
                msg.msg_controllen =
                    unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as usize;
                unsafe {
                    let c = libc::CMSG_FIRSTHDR(&msg);
                    (*c).cmsg_level = libc::SOL_SOCKET;
                    (*c).cmsg_type = libc::SCM_RIGHTS;
                    (*c).cmsg_len =
                        libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as usize;
                    *libc::CMSG_DATA(c).cast::<libc::c_int>() = fd.as_raw_fd();
                }
            }
            assert_eq!(unsafe { libc::sendmsg(client.as_raw_fd(), &msg, 0) }, 3);
            client.write_all(&response[3..]).unwrap();
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
        let (mut compositor, bridge) = UnixStream::pair().unwrap();
        let fd: OwnedFd = bridge.into();
        let (_dir, path, handle, _observer) = fake(0, Some(fd));
        let mut session = open(&path, "alice", "example").unwrap().unwrap();
        assert!(
            unsafe { libc::fcntl(session.socket.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC
                != 0
        );
        compositor.write_all(b"w").unwrap();
        let mut b = [0];
        session.socket.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"w");
        drop(session);
        handle.join().unwrap();
    }
    #[test]
    fn rejection_creates_no_session() {
        let (_dir, path, handle, _observer) = fake(1, None);
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
