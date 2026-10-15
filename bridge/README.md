# Real Wayland/RDP bridge

IronRDP git revision `7b976a6965c96c76ff949313e174ea55dada8c44` owns RDP,
TLS, optional CredSSP, bitmap codecs, graphics framing and negotiation. This exact revision includes
`CredentialValidator` and `ConnectionBinder`, absent from crates.io 0.13.0.
Rust >=1.94 is required. `Cargo.lock` pins every transitive crate and Git source.
Native dependencies include `libxkbcommon` with `xkeyboard-config` Japanese/evdev data
and FFmpeg's codec, utility and scaling libraries. Install the distribution's
`ffmpeg` package before building; the native shim uses `pkg-config` to find it.

Build without installing on the working host:

```sh
cargo build --manifest-path bridge/Cargo.toml --release --locked
cargo test --manifest-path bridge/Cargo.toml --locked
```

Broker mode defaults to `0.0.0.0:33991` for direct Windows connections.
`NIRI_RDP_LISTEN` accepts a numeric IPv4/IPv6 address and nonzero port. Disposable
mode defaults to `127.0.0.1:33991` and rejects non-loopback configuration and peers.
No UDP transport, preemption or reconnect cookie is enabled.
Each TCP connection authenticates through PAM. In broker mode, reconnecting with
the same authorized Linux account resumes its retained desktop and applications.
Disconnect closes the frontend Wayland connection and broker lease after held
input is released; lease EOF detaches the frontend and starts a 45-second grace
window. Reconnect before expiry to resume; otherwise the broker fully logs out
and cleans up the desktop, and the next login creates a fresh one after cleanup.
The timer starts when the broker observes detachment, including frontend exit.
Logout, compositor exit or broker shutdown ends the desktop immediately.
The single-connection reject policy remains. Linux TCP keepalive and an
outstanding-write timeout now detect silent loss independently of desktop activity;
private blackhole tests measured about 20–23 seconds. Retention starts at observed
detachment. Installed Windows behavior remains pending validation.
The disposable mode and namespace test broker do not prove persistent PAM-backed
resume. A compositor crash terminates the capture stream with the library
`Ok(None)` contract, ending the connection and releasing its broker lease; the frontend keeps accepting.

The default `NIRI_RDP_MODE=broker` connects only to `/run/niri-rdp/broker.sock`.
`NIRI_RDP_OUTPUT` defaults to `HEADLESS-1`. TLS reads `tls-cert` and `tls-key`
inside systemd's `CREDENTIALS_DIRECTORY`, falling back to explicit
`NIRI_RDP_CERT` and `NIRI_RDP_KEY` paths for foreground tests. Broker mode uses
TLS-only RDP because PAM passwords cannot supply the static NTLM credential
needed by this library's default NLA backend. Credentials supplied in the
library's TLS-protected ClientInfo flow reach the broker. Remembered Windows
domain credentials open an empty Linux login form and are never forwarded to
PAM. A client omitting its password also receives a
restricted login screen. No compositor or Linux login session starts until
the screen submits credentials and the broker accepts them. Incorrect credentials
clear the password; attempts have a two-second delay and the connection closes
after five failures. The screen supports JP106 physical keys and Unicode character
events, Tab, Backspace, Enter, and mouse focus/sign-in. Password pixels contain
a fixed mask, never the secret or its length. Disconnect cancels access to a
pending desktop binding and releases its lease; a desktop already created by the
broker remains available for authenticated resume. The display/input adapters switch to the
real desktop after successful login, including optional NVENC and live resizing.
Desktop keyboard input uses the authenticated compositor's existing XKB map,
including layouts, model, variants and options. The bridge observes later map
changes instead of imposing a Japanese map, and remote modifier synchronization
preserves the compositor's active layout group.
The login form uses the same negotiated graphics wrapper as the desktop and
repaints once per second while waiting for input, so late client surface setup
or graphics negotiation does not leave a static black screen. The initial
graphics surface is delivered even during startup output suppression; later
suppression still pauses desktop output.

Ordinary mstsc can negotiate TLS when the server offers no NLA. An explicit
`enablecredsspsupport:i:0` profile remains a fallback; policies requiring NLA are
incompatible. Actual mstsc verification remains pending.

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
latest frame and reuses a mapped SHM buffer until its layout changes. Native BGR
frames need one owned snapshot copy; padded, inverted or RGB frames are normalized.
`NIRI_RDP_FPS` bounds capture to 1..60 fps (default 60 with NVENC, 30 with bitmap).
Capture time counts toward the frame budget. An eventfd wakes capture negotiation,
readback and idle waits to service input. Adjacent motion and resize requests
coalesce within bounded queues while ordered input remains a barrier. Damage-aware
capture waits on quiet output; health roundtrips distinguish idle from compositor
failure. Bitmap clients receive sparse rectangles reconstructed from complete
latest snapshots; video always receives a complete desktop. Output removal or
capture failure closes the connection.

## Desktop codecs

`NIRI_RDP_ENCODER=bitmap` is the default transport. Set
`NIRI_RDP_ENCODER=nvenc` to encode the captured BGR0 desktop frames with
FFmpeg's `h264_nvenc` and send H.264 AVC420 over the negotiated RDP graphics
channel. Clients without the required graphics/codec capabilities receive bitmap
updates. The patched headless compositor prefers hardware EGL devices, including
NVIDIA, and advertises DMA-BUF formats for GPU applications. Its `auto` policy falls back to surfaceless rendering if hardware is unavailable;
the session worker's `software` policy forces llvmpipe. SHM remains the default
capture path; GPU capture is an explicit option.
NVENC accelerates the frontend's video encoding independently of compositor
rendering. FFmpeg's software scaler converts the captured pixels to full-range
BT.709 NV12 before NVENC encoding, as required by AVC420. This default path does not use NVDEC; the RDP client decodes the video. An opt-in
DMA-BUF/EGL/CUDA path keeps supported capture and BT.709 conversion on the GPU,
with SHM fallback. See [performance controls](../docs/performance.md).

Color conversion uses up to four FFmpeg workers for larger desktops. A padded
BGR staging frame enables parallel conversion; small or single-core frames use
direct conversion with only the final rows copied to SIMD-safe scratch memory.
NVENC uses its fastest preset, ultra-low-latency tuning, no B frames/lookahead,
single-pass bounded VBR and a one-frame VBV buffer.

Unchanged desktop pixels skip conversion, GPU upload and transmission. Capture
heartbeats continue so decoder recovery, minimize/resume and channel negotiation
can request a new IDR even on a static desktop. Encoding keeps one pending task
and graphics backpressure bounds unacknowledged frames. Set
`NIRI_RDP_BITRATE_MBPS=1..100` sets the H.264 bitrate ceiling (default 12 Mbps).
Each connection adapts bitrate toward a 1 Mbps floor when graphics ACK latency
rises above its baseline, acknowledgements stall, or the client reports a
decoding backlog. It recovers slowly after healthy delivery and updates the
live NVENC rate control without recreating the encoder. Tracking stays bounded
and resets for reconnects; the graphics acknowledgement-suspension sentinel is
handled as a protocol mode. Bitmap fallback retains its existing transport.
See [installation](../docs/install.md#optional-nvidia-encoding) for GPU access and
renderer policy configuration. The default remains SHM capture with CPU color conversion. Select GPU capture
explicitly only with the documented device access and a supported import path;
server/client decoding timings do not establish physical presentation latency.

Explicit NVENC mode validates the hardware encoder at startup and fails with an
error if initialization is unavailable. It requires an NVENC-capable NVIDIA GPU,
driver userspace libraries compatible with FFmpeg's NVENC API version, and access
to the GPU device nodes. The packaged default systemd unit uses `PrivateDevices`
and therefore needs the optional `nvidia-frontend.conf` drop-in described in
[installation](../docs/install.md). Hardware compositor rendering also requires
the remote-login account to have access to its GPU device nodes. The bridge uses
prebuilt FFmpeg libraries and does not require the CUDA
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

The probe reports mean, median, p95 and maximum encoding latency plus output
bytes. Optional `NIRI_RDP_PROBE_WIDTH` and `NIRI_RDP_PROBE_HEIGHT` accept 1..4096,
and `NIRI_RDP_PROBE_FRAMES` accepts 1..10000; defaults preserve the command above.
Use `NIRI_RDP_FPS=60` to test a 60 fps encoding budget. The warmed-up native
benchmark in `scripts/encoder-bench.c` excludes capture/transport and measures
180 changing frames after 20 warm-up frames:

```sh
cc -O3 scripts/encoder-bench.c -o /tmp/niri-rdp-encoder-bench \
  $(pkg-config --cflags --libs libavcodec libavutil libswscale)
/tmp/niri-rdp-encoder-bench 0 1920 1080 60 12000000
/tmp/niri-rdp-encoder-bench 0 3840 2160 60 12000000
```

Actual NVIDIA compositor rendering plus client decoding can be validated with
`lab/run-isolated.sh --nvidia-renderer` in place of `--nvenc` in the integration
command below. It exposes NVIDIA and DRM render nodes, verifies NVIDIA EGL
selection and DMA-BUF advertisement, and hides physical input and DRM modesetting
nodes. `--nvenc` retains software compositor rendering for comparison.

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
and seat. The bridge inherits the compositor's actual XKB map, including the
host's JP106 layout or explicit user layout, and sends Caps/Num/Scroll lock
synchronization to Niri. The restricted login form also reads the host's XKB
defaults before authentication, with Shift, Caps Lock, AltGr and keypad support.
Held keys/buttons are released at disconnect, and a bounded compositor sync
acknowledgment completes input teardown before the socket closes.
RDP set-1 E0 codes map explicitly to Linux evdev codes. Pointer absolute and
relative motion is bounded before mapping into the capture output. Click
coordinates travel with the button event. Normal transform and the explicitly selected output scale are required; unsupported layouts fail closed. Unicode events commit exact text through a temporary one-level XKB map, including
Japanese and UTF-16 surrogate pairs (emoji). Each character is a balanced tap; the
physical keymap and held modifiers are restored afterward. Phone-side IME
composition can submit committed text this way. Pause/E1 and lock-screen behavior remain work. Bidirectional Unicode text
clipboard uses CLIPRDR and private-session Wayland data control, with bounded
payloads/deadlines and reconnect cleanup; host-local clipboard access is excluded.

## Session audio

Broker sessions redirect application playback to the RDP client over the
TLS-protected `rdpsnd` channel. Each desktop starts its own PipeWire,
PipeWire-Pulse and WirePlumber instances, with a virtual default output named
**Remote Desktop Audio**. Hardware discovery is disabled on that private graph.
Applications use its private runtime and Pulse socket; the frontend receives
only a broker-authorized versioned PCM capture descriptor, without access to the user's runtime.
This follows GNOME Remote Login's session-playback model: capture an audio sink
and send it through RDP ([GNOME implementation](https://github.com/GNOME/gnome-remote-desktop/blob/main/src/grd-rdp-dvc-audio-playback.c)).

Capture is stereo 48 kHz, signed 16-bit little-endian PCM. The frontend prefers
this uncompressed format, using 20 ms blocks and about 1.54 Mbit/s before framing.
Windows 10 mstsc playback was intelligible with PCM, but both compressed offers
have produced static in actual listening checks. Clients that do not accept PCM
can use GNOME Remote Login's AAC-LC format: `AAC_MS`, stereo 44.1 kHz at 96 kbit/s,
1024 sample frames per raw packet, with no ADTS header or WAVE extra data.
FFmpeg resamples capture with bounded buffers; encoder lookahead remains below
two AAC frames. MS ADPCM is the last fallback. No
additional audio configuration is required. A client must enable
remote playback and accept one of these formats. Microphone redirection is a
separate path; see [Microphone](#microphone). Audio starts after
successful Linux authentication, including submission through the login form.
Disconnect closes capture; authenticated reconnect opens a fresh monitor of the
retained desktop. At most four waves await receipt confirmation. Optional later
confirmations provide playback delay measurements; they are never required to
release a second credit. Fresh completion feedback can describe a wave queued
for more than a second if the same playback run is still active and its block
number has not been reused. Feedback from an earlier run, ambiguous reused
numbers and implausible timestamp delays cannot restart recovery. Timestamp
delays must fit the elapsed time plus a 300 ms device scheduling allowance.
Valid feedback above 300 ms briefly discards capture while the queued tail
drains. Each measurement can extend recovery by at most one second; distinct
fresh completions can extend the drain, but optional replies never become
required credits. Recovery expires and resumes consecutive fresh packets,
allowing AAC to progress beyond priming.
Missing replies, mixed single/double confirmations and old idle waves cannot
permanently mute sound or create an ever-growing delay. Later receipt progress
also retires missing earlier confirmations. Actual audio congestion reduces
the live H.264 target even when graphics acknowledgements are healthy or
suspended; recovery waits for three seconds without audio pressure.
The bridge paces blocks by their exact sample duration and retains a three-block
capture FIFO to absorb ordinary PipeWire batching. Excess backlog drops the
oldest complete blocks while preserving partial-sample alignment. Capture also
expires after 60 ms of frontend buffering. Continuous digital silence is
suppressed after a short codec tail, and AAC state resets when a transport or
playback hold discards capture so old lookahead is discarded. Waiting for credit
without dropping capture preserves codec state. The session monitor also drains
capture continuously through socket stalls with bounded buffers.
Like GNOME, RDPSND timestamps are zero: this desktop uses RDPGFX, without the
RDPEVOR shared presentation clock that the Wave2 audio timestamp accompanies.
Internal monotonic times still bound queue age and measure confirmation delays.
The private NRA1 framing carries sequence and CLOCK_MONOTONIC source/relay times.
Source age begins at the helper's first monitor read, excluding earlier application
and PipeWire buffering. The frontend expires capture using that source age.
Audio waits on readiness, feedback and exact pacing deadlines instead of fixed
two-millisecond polling. Private graph/helper retries are finite; ancillary audio
failure permits a usable desktop and reconnect obtains a fresh capture descriptor. Actual Windows
lip-sync and recovery are checked separately; client output latency is not a
server-controlled shared presentation clock.
Disposable compositor mode has no audio backend.

`python3 -m unittest tests.test_audio -v` runs real private PipeWire playback,
captures a tone and repeats after reconnect, and verifies AAC and ADPCM
encode/decode with guarded source buffers. Rust tests negotiate the audio
formats, verify codec preference and exact signed stereo samples in both
WaveInfo/Wave and Wave2 PDUs, verify the client's format index, and check
backpressure, long playback feedback, timestamp/block wrapping, healthy sample
continuity and disconnect cleanup. Installed-service and Windows mstsc sound
playback remain actual-machine acceptance checks.

## Microphone

`src/microphone.rs` implements mstsc **Record from this computer** (MS-RDPEAI,
the `AUDIO_INPUT` dynamic channel) through the vendored server's
`RdpeaiServerFactory`, using the `ironrdp-rdpeai` crate at the same pinned
IronRDP revision. `NIRI_RDP_MICROPHONE=auto` (default) registers the factory;
`off` registers nothing, so no `AUDIO_INPUT` channel is offered, and drops the
broker's microphone descriptor. Any other value fails startup.

The path shares nothing with playback except the TLS connection and the server
event loop: its own dynamic channel (playback uses the `rdpsnd` static channel),
its own `rdp-microphone` thread per helper connection, its own bounded queue,
its own socket (`audio-in-0`, the optional third descriptor of the
[broker reply](../broker/README.md#wire-contract-v2)), its own NRM1 framing,
helper (`session-microphone`) and restart budget, and its own `mic_*` summary
counters. `audio.rs` and the accepted RDPSND PCM baseline are unchanged.

The server offers 16-bit PCM at 48 kHz mono, 48 kHz stereo, 44.1 kHz mono and
44.1 kHz stereo, in that order, each as WAVE_FORMAT_EXTENSIBLE and plain PCM,
and requests 20 ms packets. 48 kHz mono needs about 0.8 Mbit/s of upload before
framing. Recording is server-initiated, and the bridge sends `Open` only when
formats are negotiated, the session helper is attached and it reports a remote
recorder (`IN_USE`). When the helper reports `IDLE` (after its own 5-second
debounce), the bridge asks the server to recycle the channel: MS-RDPEAI has no
stop PDU, so closing the channel is what releases the Windows microphone. A
fresh channel is created at once, so the next recorder needs no reconnect.
A refused open tries the next format; when all fail, the client is asked again
only on a recorder start at least 30 seconds later. An unanswered Open is
abandoned after 10 s, as a failed open.

`on_audio_data` validates the length against the format and pushes whole
frames to a drop-oldest queue of at most 100 ms without blocking the server
loop. The writer thread sends NRM1 records (`!4sBBHIIIQ`: magic, kind,
channels, flags, rate, sequence, length, CLOCK_MONOTONIC receive time) over a
non-blocking socket with an 8 KiB send buffer and never sends audio older than
100 ms. The helper answers with 8-byte `!4sBBH` records (`HELLO`, `IN_USE`,
`IDLE`). A protocol error or helper exit is logged once
(`RDP microphone ...` in the journal); the microphone is then unavailable until
the next connection, while desktop and playback continue.

The pinned upstream server cannot stop capture, and its DRDYNVC treats data for
a removed channel as fatal. The vendored `ironrdp-server` therefore adds
`ServerEvent::RdpeaiRecycle` (`src/rdpeai_recycle.rs`, recorded in
`vendor/ironrdp-server/UPSTREAM`): it closes the open `AUDIO_INPUT` channel,
which runs the backend's `on_close`, creates a fresh one from the factory, and
drops capture data the client sent before it processed the Close (up to 8
closed channel ids). RDPSND, EGFX and CLIPRDR handling is untouched. It is a
candidate for upstreaming.

`cargo test` covers format choice, Open only after negotiation and `IN_USE`,
length validation, the queue bound and drop-oldest under a stalled writer, the
Open timeout and retry window, and clean shutdown.
`tests/microphone_wire.rs` runs negotiation, Open, Data and the recycle
(including late data for a recycled channel) through the crate's client side
offline. `python3 -m unittest tests.test_microphone -v` covers the session
helper, including `ECONNRESET` when the frontend closes. The namespace check with real Niri, PipeWire and
FreeRDP is `lab/run-isolated.sh env NIRI_RDP_LAB_BRIDGE=<bridge> python3
scripts/microphone-integration.py`; see the
[validation record](../docs/validation.md#microphone-redirection-ms-rdpeai-2026-10-15).
Windows mstsc recording remains an actual-machine acceptance check.

## Dynamic resolution

The frontend honors the client's initial desktop dimensions, bounded to 4096
pixels per axis. During the session, clients with the RDP Display Control channel
can request a new single-display layout at the origin. Live dimensions must be
200..4096 pixels per axis, with even width as required by Display Control; odd
heights and portrait layouts are supported. Unsupported layouts are ignored.
Clients without Display Control keep their initial size. Display Control desktop scale metadata selects logical output scaling when
`NIRI_RDP_SCALE=client` (default); an explicit 1.0–5.0 override takes precedence.
Physical RDP pixels remain distinct from logical application coordinates. The
initial scale is 1 until client scale metadata is available.

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
`NRB1 + status`: 0 plus two ordered CLOEXEC SCM_RIGHTS FDs (Wayland, PCM); the PCM
socket starts capture after receiving one ASCII `A` readiness byte. Status 1 is auth/policy
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

## Upgrade controls and measurements

See [performance](../docs/performance.md) for profile, codec, GPU capture, link
budget, DPI, diagnostics and reproducible private workload controls. AVC444 and
new graphics cadence/window policies remain opt-in pending comparative and
Windows acceptance. Login rendering caches its background and complete refresh;
bitmap field edits repaint a region, and password edits under the fixed nonempty
mask do not trigger pixel notifications. XKB maps are cached on their owning
input thread.
