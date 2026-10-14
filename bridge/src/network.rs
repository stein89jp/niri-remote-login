//! TCP liveness independent of desktop activity and optional graphics ACKs.
use std::{io, os::fd::AsRawFd};

pub const KEEP_IDLE_SECONDS: i32 = 10;
pub const KEEP_INTERVAL_SECONDS: i32 = 3;
pub const KEEP_PROBES: i32 = 3;
pub const USER_TIMEOUT_MILLISECONDS: i32 = 20_000;

/// Linux inherits these listener options on each accepted connection. Keepalive
/// probes require TCP responses even when RDP is idle; user-timeout bounds an
/// outstanding write or zero-window stall. No application traffic is required.
pub fn configure_tcp_liveness(socket: &tokio::net::TcpSocket) -> io::Result<()> {
    configure_fd(socket.as_raw_fd())
}

fn configure_fd(fd: std::os::fd::RawFd) -> io::Result<()> {
    for (level, option, value) in [
        (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1),
        (libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, KEEP_IDLE_SECONDS),
        (
            libc::IPPROTO_TCP,
            libc::TCP_KEEPINTVL,
            KEEP_INTERVAL_SECONDS,
        ),
        (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, KEEP_PROBES),
        (
            libc::IPPROTO_TCP,
            libc::TCP_USER_TIMEOUT,
            USER_TIMEOUT_MILLISECONDS,
        ),
    ] {
        // SAFETY: value is a live integer of the size passed to setsockopt.
        if unsafe {
            libc::setsockopt(
                fd,
                level,
                option,
                (&value as *const i32).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn option(fd: std::os::fd::RawFd, level: i32, name: i32) -> i32 {
        let mut value = 0i32;
        let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
        assert_eq!(
            unsafe {
                libc::getsockopt(fd, level, name, (&mut value as *mut i32).cast(), &mut size)
            },
            0
        );
        value
    }

    #[test]
    fn accepted_stream_inherits_finite_liveness_without_rdp_traffic() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        configure_fd(listener.as_raw_fd()).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        for stream in [listener.as_raw_fd(), accepted.as_raw_fd()] {
            assert_eq!(option(stream, libc::SOL_SOCKET, libc::SO_KEEPALIVE), 1);
            for (name, expected) in [
                (libc::TCP_KEEPIDLE, KEEP_IDLE_SECONDS),
                (libc::TCP_KEEPINTVL, KEEP_INTERVAL_SECONDS),
                (libc::TCP_KEEPCNT, KEEP_PROBES),
                (libc::TCP_USER_TIMEOUT, USER_TIMEOUT_MILLISECONDS),
            ] {
                assert_eq!(option(stream, libc::IPPROTO_TCP, name), expected);
            }
        }
    }
}
