# Real Wayland/RDP bridge

IronRDP git revision `bf019ca502d1e52616853a5ded68c885cec1e322` owns RDP,
TLS, optional CredSSP, codecs and negotiation. This exact revision includes
`CredentialValidator` and `ConnectionBinder`, absent from crates.io 0.13.0.
Rust >=1.94 is required. `Cargo.lock` pins every transitive crate and Git source.
The native dependency is `libxkbcommon` with `xkeyboard-config` US/evdev data.

Build without installing on the working host:

```sh
cargo build --manifest-path bridge/Cargo.toml --release --locked
cargo test --manifest-path bridge/Cargo.toml --locked
```

The frontend has one fixed listener, `127.0.0.1:33991`. It does not change GNOME
Remote Login, the existing PoC listener or physical inputs. No UDP transport,
preemption or reconnect cookie is enabled. An SSH tunnel is needed for Windows.
Each TCP connection receives a new authenticated desktop; reconnect performs PAM
again and creates a new session. Disconnect closes the broker lease and ends
the session. A compositor crash terminates the capture stream with the library
`Ok(None)` contract, ending the connection and releasing its broker lease; the frontend keeps accepting.

The default `NIRI_RDP_MODE=broker` connects only to `/run/niri-rdp/broker.sock`.
`NIRI_RDP_OUTPUT` defaults to `HEADLESS-1`. TLS reads `tls-cert` and `tls-key`
inside systemd's `CREDENTIALS_DIRECTORY`, falling back to explicit
`NIRI_RDP_CERT` and `NIRI_RDP_KEY` paths for foreground tests. Broker mode uses
TLS-only RDP because PAM passwords cannot supply the static NTLM credential
needed by this library's default NLA backend. Credentials supplied in the
library's TLS-protected ClientInfo flow reach the broker; domain-qualified
names are rejected. Windows must permit a TLS-only connection (`enablecredsspsupport:i:0`
in a dedicated .rdp profile). Actual mstsc verification remains pending.

For a disposable current-UID compositor only, `NIRI_RDP_MODE=disposable` uses
library NLA with explicit disposable `NIRI_RDP_USERNAME` and
`NIRI_RDP_PASSWORD`; those are never interpreted as Linux credentials. Set
`NIRI_RDP_WAYLAND_SOCKET` to the **absolute** private test compositor socket.
No inherited `WAYLAND_DISPLAY`, `DISPLAY` or local desktop socket is used.
The bridge executable accepts no arguments or password flags.

`capture-probe /absolute/private/wayland-socket [HEADLESS-1]` captures real
frames at 640x480 and requires changing application pixels. It injects keyboard,
pointer, click and disconnect-release requests. Confirm acknowledgements in the
animated test app's own log; request submission alone is not input proof.

Capture uses `zwlr_screencopy_manager_v1` into bounded anonymous SHM files,
accepting XRGB/ARGB/XBGR/ABGR8888, converting row padding and vertical inversion,
and scaling into the negotiated RDP desktop. The worker retains only the
latest frame and captures at most 30 fps. Wayland dispatch/capture waits have
bounded deadlines. Output removal or capture failure closes the connection.

Input uses virtual keyboard and virtual pointer v2 bound to the selected output
and seat. The compiled US PC105/evdev XKB map, modifiers and Caps/Num/Scroll
synchronization are sent to Niri; held keys/buttons are released at disconnect, and a bounded compositor sync
acknowledgment completes input teardown before the socket closes.
RDP set-1 E0 codes map explicitly to Linux evdev codes. Pointer absolute and
relative motion is bounded before mapping into the capture output. Click
coordinates travel with the button event. Output scale 1 and normal transform
are required; unsupported layouts fail closed. Unicode/IME events are ignored,
not treated as physical keys; arbitrary layouts, Pause/E1, IME, dynamic resize,
clipboard, audio and lock-screen behavior remain work. The network frontend reports a fixed 1920x1080 desktop; the capture probe
exercises 640x480 scaling. Client-selected initial size and resizing during an
active connection need a tested size/input contract before enabling
display-control or honoring client dimensions.

Broker transport is bounded `NRB1 + BE16(username length) + BE16(password length)
+ UTF-8 credentials`, no NUL; username 1..64, password 1..512 bytes. Reply is
`NRB1 + status`: 0 plus exactly one CLOEXEC SCM_RIGHTS Wayland FD; 1 auth/policy
rejected; 2 startup/backend failure; 3 malformed/unauthorized request. The broker
lease remains open until the authenticated backend drops. No filesystem access
to the target user's runtime is needed by the frontend. Broker receive supports
fragmented replies and rejects truncated/excess FD transport.

The disposable Arch namespace validation passed real Niri application capture,
FreeRDP/NLA decoded frames, keyboard/pointer/button acknowledgments, wrong
credentials, held-input disconnect/reconnect release and compositor SIGKILL
(client closes within 5 seconds; frontend remains alive). See CHECKPOINT.md for
exact commands. PAM/logind boot, Windows mstsc and physical-monitor tests remain
pending actual separate machines.

Production broker-mode TLS-only transport was also validated using a namespace
NRB1 test broker handing a real Niri FD: actual ClientInfo credential delivery,
auth rejection, desktop binding and all accepted lease EOF checks passed. This
fixture is transport validation only and does not replace real PAM/logind boot
testing. The FreeRDP client enables automatic logon to send TLS ClientInfo
credentials; it retries rejected TLS attempts, whose client error is currently
a generic transport failure. See coordinator validation for the packaged rerun.
