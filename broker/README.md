# PAM/logind broker

This is an experimental privileged broker implementation, separate from the
unprivileged RDP frontend. Native tests use fake PAM and never create a real
login session. A real seatless session, boot without local login, and fatal
cleanup still require the installed disposable Arch test machine. Nothing in
this directory has been installed or configured on the working host.

Build with `make -C broker`; run `make -C broker check`. Dependencies are GCC or
Clang, Linux-PAM headers/library, and systemd headers/libsystemd. Stage with an
**absolute** DESTDIR, for example:

```sh
make -C broker DESTDIR="$PWD/artifacts/broker-stage" install
```

The coordinator's Arch package installs these artifacts; do not run the install
target with an empty DESTDIR on the working host. Package service enablement,
TLS credentials, account setup, upgrade and uninstall instructions are in the
main installation documentation.

## Ownership and policy

The root system service listens only on `/run/niri-rdp/broker.sock`, not TCP.
It requires the peer's kernel-reported UID to equal the dedicated `niri-rdp`
frontend account. The socket is root:niri-rdp mode 0660. Its parent is root-owned
mode 0711 so desktop UIDs can traverse to their own private runtime; session
records are 0700 and every desktop runtime is 0700 owned by its account.

Requests contain only a Linux username and password. The frontend cannot choose
an executable, arguments, session environment, account UID, socket path or
output size. Accounts must have UID >= 1000, a login shell and membership in
`niri-rdp-users`. The frontend account and root are rejected. The final identity
returned by PAM is resolved and authorized; the originally requested identity
is not trusted after PAM. A nonblocking per-UID lock prevents concurrent broker
desktops. A retained broker desktop can be resumed after fresh PAM authentication,
account checks and group authorization; its own logind session is exempted from
the existing-session check. Other X11/Wayland sessions, including closing
sessions, reject login. The initial desktop should use a dedicated test account. A simultaneous
local login can race this check; the broker cannot lock GDM or an unrelated
login manager, and same-UID desktops are not an isolation boundary.

Each desktop has a separate privileged holder process. Its per-UID resume socket
is root-only and authenticates the root broker peer; the frontend cannot access
it directly. The holder retains the original PAM session while detached.
A resume request authenticates anew without opening another desktop session. The dedicated
`niri-rdp` PAM policy includes Arch's remote-login auth/account policy and
requires `pam_systemd`. Password-only PAM is supported; a second username or
secret prompt across the handle's lifetime is rejected, including OTP/MFA and
password-change conversations. Credentials are not logged or placed in argv or
persistent state; the broker erases its request buffer and disables core dumps.
PAM owns its internal credential allocations. Credential reading has a 10s
socket timeout; the holder catches a 30s alarm, and a parent 35s watchdog kills a
stalled startup even if a PAM module ignores interrupted I/O.

Before launch the broker checks logind's recorded UID, leader, service,
remote flag, Wayland type, absent seat and absent VT. It requires PAM's genuine
`/run/user/UID` to exist with correct ownership and permissions. It launches
only `/usr/bin/dbus-run-session -- /usr/lib/niri-rdp/session-worker` after
setting account groups and UID/GID. The environment is constructed afresh;
only PAM's validated runtime path and session ID are carried over. The worker
uses a private runtime directory for Niri and its applications, a private bus,
`NIRI_BACKEND=headless`, automatic headless EGL hardware rendering with a
surfaceless fallback, and the maintained side-by-side Niri binary. The root-owned
`/etc/niri-rdp/renderer.conf` selects `auto` (default), `hardware` (require GPU),
or `software` (force llvmpipe). GPU access uses the account's existing device
permissions; the broker does not acquire DRM master or grant extra groups. It does not
use `niri --session`, import environment into the user manager, or start
`graphical-session.target`. The per-UID runtime and user manager still exist as
required by logind; this is not a separate Unix security boundary.

## Wire contract v1

Use an AF_UNIX SOCK_STREAM socket. Send exactly:

| Field | Size | Value |
|---|---:|---|
| Magic | 4 bytes | `NRB1` |
| Username length | 2 bytes | Unsigned big endian, 1..64 |
| Password length | 2 bytes | Unsigned big endian, 1..512 |
| Username | Length bytes | Linux ASCII account name, no NUL |
| Password | Length bytes | UTF-8 password bytes, no NUL |

The response is 5 bytes: `NRB1` and one status byte. Status 0 carries **one**
SCM_RIGHTS Wayland connection FD in the first `recvmsg`; 1 means generic
authentication/account/policy rejection; 2 means backend/session startup failed;
3 means malformed/unauthorized request or connection capacity exceeded. No
human-readable credentials/errors are returned. The frontend must use
`recvmsg`, preserve received FDs while finishing a fragmented 5-byte header,
validate ancillary metadata, and reject missing/excess descriptors. The broker
connects to the new compositor as its target UID before passing the FD; the
frontend never connects to the user's local Wayland socket or filesystem.

Keep the control stream open while attached to the desktop. EOF detaches the
frontend and retains the desktop for a 45-second grace window measured from
broker-observed detachment. If no authorized reconnect completes before expiry,
the holder fully logs out and cleans up the desktop. A subsequent login starts
a fresh desktop after cleanup. An ordinary v1 login within the grace window with
the same account performs fresh authentication and authorization, then receives
a new Wayland connection to that desktop. No client resume token or reconnect
cookie bypasses authentication. Unexpected additional bytes detach the frontend
and start the same 45-second grace window.
Compositor exit, explicit compositor logout, or broker shutdown tears down the
desktop. Close all frontend-held descriptors when detaching.

## Cleanup and failure behavior

Normal teardown kills the compositor process group (also after its leader has
exited), closes the PAM session, deletes credentials and ends PAM. The private
directory is traversed with no symlink following by an unprivileged cleanup
process, then its empty root is removed from the root-owned parent.

The parent maintains root-only records of each logind ID and terminates only
sessions whose PAM service is `niri-rdp`. A holder crash is also matched by
logind's recorded leader PID, covering death between PAM registration and record
creation. ExecStopPost scans records and service-owned logind sessions after a
broker crash. It never terminates GNOME/GDM/GRD or unrelated sessions.
Transient logind failures retain records for 5s retries; an unrecorded crashed
holder's record is reconstructed from logind before termination. Directories
remain until the session's asynchronous termination is observed on a later retry.
Real logind TerminateSession and descendant cgroup cleanup need target-machine
validation; native process-group tests are not proof of cgroup cleanup. Programs
that deliberately start persistent user-manager units need a separate lifetime
audit. The test default launches only a terminal in the dedicated session.

## Manual broker checks on the disposable Arch target

After installing the project package and configuring the dedicated test
account/service, invoke this repository's client as the frontend service UID:

```sh
sudo -u niri-rdp python broker/tools/client.py niri-rdp-test --expect reject
sudo -u niri-rdp python broker/tools/client.py niri-rdp-test --expect success --seconds 30
```

The first invocation should use an incorrect password and return status 1;
repeat with a denied account and expired/locked test account. Use disposable
credentials: this Python smoke client cannot reliably erase immutable password
copies and is not the production frontend.

While the success client holds its control socket, use `loginctl` on the target
to inspect the new session's User, Leader, Service, Type, Remote, Seat and VTNr.
Expect the account UID, `niri-rdp`, `wayland`, Remote=yes, no seat and VTNr=0.
Inspect its process cgroup and verify the compositor/app share that session
scope; no compositor opens a physical DRM card or VT. After the control socket
closes, verify the same session, compositor/app/bus processes and desktop remain.
Reconnect and check the logind session ID, compositor PID and open application
state are unchanged. Reject a wrong password and revoked group membership on
resume without creating another session. Then log out of the compositor and
verify the session and compositor/app/bus processes disappear and
`/run/niri-rdp/desktops` and `/run/niri-rdp/sessions` contain no session resources.
An unrelated SSH session or existing user manager can legitimately keep
`/run/user/UID` alive; it must not be deleted manually by the broker.

Repeat with missing/failing compositor executable in the disposable VM, a
compositor SIGKILL, holder SIGKILL, broker SIGKILL and a boot with no local login.
Watch for no stale session scope, child process or private runtime. Then use the
packaged frontend and ordinary Windows mstsc directly at `HOST:33991`. The broker
prepares the TLS identity on first start before the frontend loads credentials.
Clients omitting a password see the frontend's restricted login screen; PAM still
authorizes the account before any desktop is started. These
checks and physical-monitor-off testing are pending, tracked in CHECKPOINT.md.
