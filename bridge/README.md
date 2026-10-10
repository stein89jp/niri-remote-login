# Real Wayland/RDP bridge

IronRDP git revision `bf019ca502d1e52616853a5ded68c885cec1e322` owns RDP,
TLS, optional CredSSP, bitmap codecs, graphics framing and negotiation. This exact revision includes
`CredentialValidator` and `ConnectionBinder`, absent from crates.io 0.13.0.
Rust >=1.94 is required. `Cargo.lock` pins every transitive crate and Git source.
Native dependencies include `libxkbcommon` with `xkeyboard-config` US/evdev data
and FFmpeg's codec, utility and scaling libraries. Install the distribution's
`ffmpeg` package before building; the native shim uses `pkg-config` to find it.

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
and normalizing frames into the negotiated RDP desktop. The native headless
output is resized to that same pixel size before capture. The worker retains only the
latest frame and captures at most 30 fps. Wayland dispatch/capture waits have
bounded deadlines. Output removal or capture failure closes the connection.

## Desktop codecs

`NIRI_RDP_ENCODER=bitmap` is the default transport. Set
`NIRI_RDP_ENCODER=nvenc` to encode the captured BGR0 desktop frames with
FFmpeg's `h264_nvenc` and send H.264 AVC420 over the negotiated RDP graphics
channel. Clients without the required graphics/codec capabilities receive bitmap
updates. The remote desktop still renders with llvmpipe and captures through SHM;
NVENC accelerates the frontend's video encoding independently of compositor
rendering. FFmpeg's software scaler converts the captured pixels to full-range
BT.709 NV12 before NVENC encoding, as required by AVC420. This path does not use
NVDEC; the RDP client decodes the video.

Explicit NVENC mode validates the hardware encoder at startup and fails with an
error if initialization is unavailable. It requires an NVENC-capable NVIDIA GPU,
driver userspace libraries compatible with FFmpeg's NVENC API version, and access
to the GPU device nodes. The packaged default systemd unit uses `PrivateDevices`
and therefore needs the optional `nvidia-frontend.conf` drop-in described in
[installation](../docs/install.md). The remote-login account does not need GPU
groups. The bridge uses prebuilt FFmpeg libraries and does not require the CUDA
toolkit or Video Codec SDK to build or run.

The release frontend passed actual NVENC AVC420 encoding and FreeRDP decoding
of an animated Wayland app in private namespaces, including input, reconnects,
held-input release and compositor failure. A client without graphics support
also passed the same checks through bitmap fallback while NVENC remained
configured. Reports are `artifacts/nvenc-tls-integration.json` and
`artifacts/nvenc-bitmap-fallback-integration.json`. Windows, installed-service
permissions and complete-path performance remain pending; see
[acceptance](../docs/acceptance.md).

`encoder-probe OUTPUT.h264` tests the native NVENC encoder without Wayland or
an RDP listener. It writes 30 changing 1920×1088 H.264 frames and refuses to
overwrite an existing output file. From source:

```sh
cargo run --manifest-path bridge/Cargo.toml --locked --bin encoder-probe -- \
  /tmp/niri-rdp-nvenc.h264
ffmpeg -v error -i /tmp/niri-rdp-nvenc.h264 -f null -
ffprobe -v error -select_streams v:0 -count_frames \
  -show_entries stream=codec_name,width,height,nb_read_frames \
  -of default=noprint_wrappers=1 /tmp/niri-rdp-nvenc.h264
```

Require successful decode and 30 H.264 frames at 1920×1088. The eight padded rows
align the coded height to 16 pixels; RDP region masks crop them to the logical
1920×1080 desktop. The package installs
the probe as `/usr/lib/niri-rdp/encoder-probe`; the installation guide shows how
to run it with the frontend service account's GPU access.

The real desktop integration client also needs FreeRDP's client addon library
to load the graphics channel and its AVC420 decoder. Build it in the prepared
repository lab:

```sh
mkdir -p artifacts
cc -Wall -Wextra -Werror -Wno-deprecated-declarations -O2 \
  scripts/rdp-desktop-client.c \
  $(pkg-config --cflags --libs freerdp3 freerdp-client3 winpr3) \
  -o artifacts/rdp-desktop-client
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_BRIDGE=bridge/target/release/niri-rdp-bridge \
  python3 scripts/headless-integration.py
```

The explicit `--nvenc` lab mode exposes NVIDIA device nodes and read-only sysfs
metadata required by CUDA device discovery. Physical input and DRM nodes,
host homes, display sockets and runtime directories remain hidden. The test
requires server AVC420 activation and actual FreeRDP AVC420 decoding, then
checks application input, reconnects and compositor failure.
Set `NIRI_RDP_LAB_GFX=0` in that command to check bitmap fallback for a client
without graphics support while the host encoder remains configured as NVENC.

Input uses virtual keyboard and virtual pointer v2 bound to the selected output
and seat. The compiled US PC105/evdev XKB map, modifiers and Caps/Num/Scroll
synchronization are sent to Niri; held keys/buttons are released at disconnect, and a bounded compositor sync
acknowledgment completes input teardown before the socket closes.
RDP set-1 E0 codes map explicitly to Linux evdev codes. Pointer absolute and
relative motion is bounded before mapping into the capture output. Click
coordinates travel with the button event. Output scale 1 and normal transform
are required; unsupported layouts fail closed. Unicode/IME events are ignored,
not treated as physical keys; arbitrary layouts, Pause/E1, IME, clipboard, audio
and lock-screen behavior remain work.

## Dynamic resolution

The frontend honors the client's initial desktop dimensions, bounded to 4096
pixels per axis. During the session, clients with the RDP Display Control channel
can request a new single-display layout at the origin. Live dimensions must be
200..4096 pixels per axis, with even width as required by Display Control; odd
heights and portrait layouts are supported. Unsupported layouts are ignored.
Clients without Display Control keep their initial size. Physical monitor size
and client DPI scaling do not change the remote output scale, which remains 1.

The capture worker applies each mode through wlr output management on its
authenticated Wayland connection and verifies Niri's current mode. The maintained
headless compositor patch updates the existing output, so applications and
workspaces survive and reflow. Frames and pointer coordinates use the same size;
IronRDP reactivates the desktop before sending pixels at the new resolution.
NVENC recreates its encoder and graphics surface and begins with an IDR frame;
codec padding is cropped to the logical desktop.

Rebuild **both** `niri-rdp-headless` and `niri-remote-login`. The older delivery
archives and upstream headless backend do not include native mode changes.
Choose the desired resolution in your RDP client and enable its dynamic
resolution option to follow window-size changes. Windows mstsc interoperability
remains an actual-machine acceptance gate.

The namespace integration probe requests 1001×701 initially, then resizes to
702×1001, 1002×701, and 702×1001 without reconnecting. At each size it checks
FreeRDP-decoded geometry, native/logical Niri output dimensions, application
keyboard input and center-pointer click/release. Use the existing integration
commands above with the current debug or release frontend and rebuilt compositor.

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
