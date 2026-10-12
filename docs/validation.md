# Validation record

## Audio playback feedback and Fcitx5, 2026-10-12

Release `0.1.0-15` fixes two audio feedback errors: a receipt confirmation with a
positive timestamp delta previously hid later playback feedback, and eight-bit
block numbers could wrap while an older wave still awaited playback. Increasing
feedback remains valid; clients demonstrating paired confirmations wait for
playback progress before reusing those numbers. Equal replies and later FIFO
progress retire missing earlier confirmations. This follows the
[MS-RDPEA confirmation timestamp rule](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/a4e42263-dc78-4748-9752-173e5bfa3dfb).

The frontend automatically prefers 48 kHz stereo MS ADPCM when offered by the
client, reducing payload traffic from PCM's 1536 kbit/s to about 389 kbit/s.
Independent 1024-byte blocks contain 1012 sample frames and use the exact
21.083333 ms cadence. PCM remains the fallback. Wave2 timestamps record frontend
capture acquisition before pacing, using the
[boot-relative timestamp specified by MS-RDPEA](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/25cebccb-d679-4302-8dd0-df7fb9a4f9b5).
These do not provide upstream sample timestamps or a shared client presentation clock.

The installed release 14 journal identified the Fcitx failure directly:
`zwp_virtual_keyboard_v1#22: error 0: invalid virtual keyboard keymap data`.
The pinned Smithay input-method grab sends an unsealed keymap without a trailing
NUL, which Fcitx forwards unchanged. The maintained patch now accepts that form
alongside sealed bridge keymaps. Forwarded keys from the input-method grab's
owner reach applications without reentering the grab or applying XKB and
bindings twice. The same journal showed a five-second input-method startup
timeout and a Wayland registry timeout. Preparation now serves Wayland events
while waiting for Fcitx's loaded controller; failed helper startup reaps its
launched daemon before retry. The existing profile snapshot remains private.
Committed Unicode retains its device map and shares the grab's FIFO with
physical keys, preserving mixed input order and physical modifiers. Five native
tests cover keymap files, ownership, map/modifier preservation and a delayed
input method with a repeated grab. The latter reproduces deferred Wayland
resource initialization, which must be handled before recording the new grab.
The client protocol test enables the existing locked `wayland-protocols-misc`
0.3.12 client feature: two dependency links change, while all 482 crate
versions, checksums and source revisions remain identical. Both original and
patched lockfile hashes are recorded in `compositor/pins.json`.

`make check BRIDGE_CARGO_HOME="$PWD/poc/.cargo-cache"` passed: 71 Python tests
(two opt-in checks skipped), five PoC Rust tests, four startup checks, all 12
native broker groups, 80 bridge library tests and the frontend lifecycle test.
Fourteen bridge audio tests include more than a minute of simulated encoded RDP
traffic across timestamp/block wrapping, positive receipt then long playback
feedback, duplicate/missing replies, healthy PCM and ADPCM quantum continuity,
backpressure and reconnect cleanup. The native codec fixture independently
decodes dropped ADPCM blocks and checks exact sample counts, stereo signal
quality, guarded input buffers and recovery after invalid lengths. Rust formatting
passed. Clippy reports only five existing login/Wayland warnings.

The default lab FreeRDP 3.32.1 build accepts the 48 kHz stereo PCM fallback but
filters MS ADPCM with its experimental DSP codecs disabled; see its
[codec filter](https://raw.githubusercontent.com/FreeRDP/FreeRDP/3.32.1/libfreerdp/codec/dsp_ffmpeg.c).
The codec negotiation/packet and FFmpeg decode checks do not establish Windows
acceptance of the precise ADPCM format. Actual mstsc lip-sync, codec selection
and recovery after congestion remain the [audio/video acceptance check](acceptance.md).
The previous session's private audio socket had already stopped, so no live
application or client playback latency was measured from that session.

Disposable compositor/RDP/Fcitx checks and build evidence are under
`artifacts/av-ime-fix/`. The input fixture uses a private bus, synthetic account,
copied profile and native Wayland text input. No installed services, host input
profiles or packages were changed; no Git commit was created.
The combined release 15 package built with all required checks, including three
headless and five protocol regressions, and passed package structure/preservation
validation. Its extracted binaries passed NVIDIA EGL/NVENC video decoding, TLS
login rejection/authentication, live resizing, reconnect, held-input release and
compositor-crash checks. Real Fcitx5/Mozc tests committed `日本` and F7-converted
Katakana `ニホン`, preserved consecutive JIS/Shift/Unicode input with Fcitx running,
and preserved committed Unicode while Mozc was active. Startup preparation
completed while Wayland remained responsive. The packaged helper hashes match
the checkout; report: `artifacts/av-ime-fix/package-integration.json`.

## NVIDIA cold-boot and authenticated desktop startup fixes, 2026-10-12

The installed release 13 frontend still failed with `CUDA_ERROR_NO_DEVICE`
after boot even though its NVIDIA helper exited successfully. Host device
timestamps show `/dev/nvidia0` and UVM nodes at 10:46, but `/dev/nvidiactl`
appeared only at 10:48 after manual GPU initialization. NVIDIA's
[utility implementation](https://github.com/NVIDIA/nvidia-modprobe/blob/main/nvidia-modprobe.c)
creates only the explicitly requested device minors: `-c 0` does not create
the control node, and `-u` without `-c` only loads the UVM module.
Release 14 requests `-c 0 -c 255` and then `-u -c 0`, preparing the GPU,
control, UVM, and UVM-tools nodes before the hardened frontend starts.

The same journal shows PAM accepting the account and opening its session at
10:48:41. At 10:48:42, private audio startup failed because
`pactl set-default-sink niri-rdp-sink` returned `Failure: Not supported`.
The sink was already visible while WirePlumber's default metadata was still
starting. Release 14 waits within a bounded deadline for default selection
and confirms the selected sink. Login now distinguishes an account/password
rejection from a desktop startup error; infrastructure failures do not consume
the five rejected-login allowance.

The password field deliberately displays a fixed eight-star mask for any
nonempty password. A new keyboard regression verifies that one scancode press
and release adds one character and that one Unicode press and release adds
one character, while the displayed mask stays identical. The field labels
this behavior explicitly. Physical dead-key composition also preserves
accented password characters and resets between fields and submissions.

Validation evidence is stored under `artifacts/boot-login-fix/`. The Python
suite passed 67 tests with two opt-in tests skipped; all 12 native broker
groups passed. Eight audio tests include a deliberately delayed private
WirePlumber launch, default-output verification, actual playback/capture and
reconnect, startup deadline and daemon failure, and cleanup regressions.
All 73 bridge library tests and the frontend lifecycle test passed, including
dead-key/Unicode input, field-reset, fixed-mask, and infrastructure-failure
regressions. Rust formatting and the packaged systemd units passed their checks.
The combined `0.1.0-14` package built successfully with its required checks,
then passed structure/preservation validation. Its extracted NVIDIA unit and
audio helper match the edited sources. The packaged binaries passed disposable
TLS login rejection/authentication, JIS password input, real desktop input,
dynamic resizing, reconnect, held-input release, and compositor-crash checks
using the namespace broker fixture. The fixture now supplies the same JIS
defaults to the login form as its existing compositor configuration; its
previous default invocation paired a US login map with JIS password scancodes.
Report: `artifacts/boot-login-fix/rdp-integration.json`. This fixture does not
authenticate Linux accounts or exercise an installed systemd boot.

Private socket tests required execution outside the sandbox. A proposed
disposable mount-namespace NVIDIA node test could not run because `sudo -n`
required authentication. No host package, service, driver configuration, or
session was changed. Installed cold-boot and Windows login validation remain
pending; capture the boot journals before running `nvidia-smi`, FFmpeg, or a
local graphical session, following [acceptance.md](acceptance.md).

## Audio delay and adaptive NVENC bitrate, 2026-10-12

Package release `0.1.0-9` separates audio receipt credits from playback feedback,
paces PCM at a 20 ms cadence, and bounds capture FIFOs to three blocks (60 ms).
The capture relay drains its source even when the frontend stalls. Seven Rust
audio tests cover format negotiation, transport credit, playback delay, duplicate
and wrapped confirmations, bursts, partial blocks and healthy sample continuity.
Three Python tests pass, including real private PipeWire tone playback/reconnect
and exact PCM coverage with 4096-byte producer quanta at 21.33 ms intervals.

NVENC bitrate adapts independently for each graphics connection using frame ACK
latency relative to its baseline, queued bytes, and missing acknowledgements.
Tests cover reduction, recovery, bounds, long RTT, ACK suspension, connection
isolation and uninterrupted software decoding across live rate changes. An
opt-in test on the host GPU encoded 180 noisy frames in one NVENC context at
12, 2 and 8 Mbps (60 frames each). Encoded phase sizes were 1,362,510, 325,776 and
942,540 bytes; all 180 frames decoded cleanly. Reproduce with:

```sh
NIRI_RDP_TEST_NVENC=1 python3 -m unittest tests.test_native_video -v
CARGO_HOME="$PWD/poc/.cargo-cache" cargo test --locked --offline --manifest-path bridge/Cargo.toml
python3 -m unittest tests.test_audio -v
```

The release frontend also passed disposable NVENC and bitmap-fallback desktop
integration: decoded changing frames, keyboard/pointer input, live resizing,
reconnect and compositor failure, using the TLS namespace broker fixture.
Reports are `artifacts/audio-video-nvenc-integration.log` and
`artifacts/audio-video-bitmap-fallback-integration.log`. These fixtures do not
measure Windows audio presentation. No installed services were restarted during
development; actual mstsc lip-sync, sound continuity under a constrained link,
and recovery time remain the [audio/video acceptance check](acceptance.md).

## NVIDIA performance, 2026-10-10

The `0.1.0-5` source prefers NVIDIA hardware EGL compositor rendering, advertises
DMA-BUF formats to applications, reuses mapped SHM capture buffers, and avoids
encoding/transmitting unchanged desktops. NVENC capture defaults to a 60 fps
budget; frame rate and bitrate are configurable. Large frames use up to four
FFmpeg color-conversion workers with padded staging; small frames use direct
conversion with bounded scratch padding. Both paths preserve full-range BT.709
and 16-aligned coded dimensions. NVENC uses single-pass bounded VBR with a
one-frame VBV, no B frames/lookahead, and two encoder surfaces.

On this host's NVIDIA GeForce RTX 4070 Ti, driver 615.71.09, the warmed-up native
benchmark measured the following. Each run encodes 180 changing frames after
20 warm-up frames, including CPU color conversion and NVENC submission, at
12 Mbps. The original uses 30 fps; comparisons below use the same nominal rate.

| Logical desktop | Original mean / p95 | Updated mean / p95 |
| --- | --- | --- |
| 1920×1080 | 8.07 / 16.44 ms | 3.69 / 4.79 ms |
| 3840×2160 | 21.37 / 22.48 ms | 11.62 / 12.26 ms |

At a nominal 60 fps, the updated 4K path measured 11.45 ms mean and 12.33 ms p95.
The host was also running other GPU applications; earlier 1080p baseline runs
averaged 5.75–5.83 ms, so the noisy 8.07 ms baseline should not be treated as a
stable speedup ratio. These are synthetic encoding measurements, excluding
capture/readback, input, network, client decoding and display. They do not prove
end-to-end 4K60. The CPU snapshot microbenchmark also improved: native 1080p BGR
copy took 0.34–0.43 ms versus 0.93–1.05 ms for the previous normalization path.
The release probe additionally encoded and decoded 30 actual 3840×2160 H.264
frames without errors; FFprobe confirmed full range and BT.709. Its mean was
12.27 ms/frame and p95 14.90 ms, without the native benchmark's warm-up exclusion.
Recording: `artifacts/nvidia-performance-4k.h264`.

The release bridge passed the isolated real-desktop suite with confirmed NVIDIA
EGL selection (`nvidia=true`) and DMA-BUF advertisement, actual FreeRDP AVC420
decoding, login-screen authentication/rejection, native initial resolution,
live portrait/landscape and odd-height resizing, application input, reconnects,
held-input release, compositor crash, frontend survival and accepted broker
lease EOF. Receipt: `artifacts/nvidia-performance-gpu-integration.json`.
The namespace fixture supplies a real Wayland FD but does not authenticate
through PAM. The test application's buffers use SHM; DMA-BUF advertisement is
verified, while a real GPU application's import/throughput remains an installed
session check.

GPU rendering with an AVC-disabled client also passed bitmap fallback with
`NIRI_RDP_FPS=1`, including input, resizing and disconnect responsiveness.
Receipt: `artifacts/nvidia-performance-fallback-low-fps.json`. A software-rendered
NVENC suite passed during development as well:
`artifacts/nvidia-performance-nvenc-integration.json`.

All 38 bridge library tests and the frontend test passed, including unchanged
frame recovery and tiny/odd-sized H.264 decoding. Python discovery passed 17
tests with the opt-in legacy transport test skipped. Its native regression
exercises 710 guarded-buffer cases across NV12/YUV420P, tiny/odd and large
dimensions, worker boundaries, edge padding, full-range BT.709 samples and frame
reference release. All 10 broker test groups and both maintained compositor
lifecycle tests passed. Release builds, fresh pinned patch application,
makepkg source checksum verification and portable source export passed.
Clippy passed with four existing lint categories allowed; strict Clippy still
reports existing login condition/style and capture-state tuple-complexity lints.
No host package or service was installed or changed.

Reproduce after building the patched compositor and release bridge:

```sh
cc -O3 scripts/encoder-bench.c -o /tmp/niri-rdp-encoder-bench \
  $(pkg-config --cflags --libs libavcodec libavutil libswscale)
/tmp/niri-rdp-encoder-bench 0 1920 1080 60 12000000
/tmp/niri-rdp-encoder-bench 0 3840 2160 60 12000000
lab/run-isolated.sh --nvidia-renderer env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_TLS_CONTRACT=1 NIRI_RDP_LAB_LOGIN_SCREEN=1 \
  NIRI_RDP_LAB_BRIDGE=bridge/target/release/niri-rdp-bridge \
  NIRI_RDP_LAB_REPORT=artifacts/nvidia-performance-gpu-integration.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvidia-renderer env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_GFX=0 NIRI_RDP_LAB_FPS=1 NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_BRIDGE=bridge/target/release/niri-rdp-bridge \
  NIRI_RDP_LAB_REPORT=artifacts/nvidia-performance-fallback-low-fps.json \
  python3 scripts/headless-integration.py
```

Capture/readback and BT.709 conversion still cross CPU memory; this is not a
zero-copy GPU capture/encode pipeline. Installed PAM/logind, service account GPU
permissions, Windows mstsc, physical-monitor and complete-path latency/throughput
remain [actual-machine acceptance](acceptance.md) gates. Rebuild both compositor
and frontend, and enable the packaged NVIDIA frontend drop-in as described in
[installation](install.md#optional-nvidia-encoding).

Additional implementation evidence is tracked in
[compositor checkpoint](../compositor/CHECKPOINT.md),
[bridge checkpoint](../bridge/CHECKPOINT.md),
[broker checkpoint](../broker/CHECKPOINT.md), and the repository checkpoints.
The foundation record below is preserved as historical evidence. Current
actual-machine gates are listed in [acceptance](acceptance.md).

## Packaged integration, 2026-10-10

Both x86_64 Arch packages were built with `makepkg`, inspected, and extracted
inside the workspace. No pacman installation or service start was performed.
The extracted binaries passed real headless Niri-to-FreeRDP tests in a disposable
Arch namespace: animated Wayland application frames, two keyboard/pointer
roundtrips, bad-password rejection, release after a held-input disconnect, and
RDP closure within five seconds of compositor SIGKILL while the frontend survived.

The same application tests also passed in the frontend's production TLS-only
mode with a namespace-only broker fixture. That proves ClientInfo credential
validation, real Wayland FD binding, and lease EOF for all five accepted
connections. The fixture is not PAM/logind. FreeRDP retries bad credentials;
the test requires positive broker rejections and zero accepted bad logins.

The bridge has 10 passing unit tests and strict clippy validation. The broker has
9 passing native test groups with no skips, using mock PAM and no real login.
The patched compositor's bounded headless-output regression passes. Its optional
client/compositor lifecycle fixtures stalled and remain unresolved; the full
upstream compositor suite is not claimed. The 13 original prototype/PoC/test
source files retain their foundation SHA-256 hashes.

Package checks reject GNOME/GDM changes, stock Niri replacement, automatic
service enablement, install hooks and unsafe archive entries. Staged executable
paths were substituted only in temporary copies for `systemd-analyze verify`;
both units passed, without being started. See
[integration checkpoint](../checkpoints/001-integration.md) for commands, pins,
limitations and package receipts. Actual PAM/logind, Windows, fresh boot and
physical-monitor tests are still pending.

Executed on Arch Linux on 2026-10-10. All changes are workspace files or temporary
test artifacts. No packages, system units, PAM policies, GNOME settings, accounts,
firewall rules, or monitor/power settings were changed.

## Completed

- **7 Python session-isolation tests pass.** They use real child processes and
  a private D-Bus: no inherited display/socket/credential environment, private
  directories for two workers, visible startup failure, worker and descendant
  termination, cleanup, and refusal to run the harness as root.
- **5 Rust adapter unit tests pass.** Fixed loopback endpoint/peer policy,
  framebuffer stride/size/pixels and input-driven changes, extreme pointer
  coordinates, extended-key separation, and input release after disconnect.
- **4 startup failure tests pass.** Missing/empty credentials, missing or invalid
  TLS identity, forbidden CLI overrides and TLS key logging all fail closed.
- **1 real FreeRDP transport integration test passes.** A temporary synthetic IronRDP
  server at `127.0.0.1:33990` used TLS/CredSSP and random throwaway credentials.
  The displayless FreeRDP C client rejects an incorrect password specifically
  through NLA, verifies HYBRID/TLS negotiation, decodes the 640×480 bitmap,
  sends keyboard/pointer input and checks its pixel feedback, verifies key
  release, then repeats the exchange on a fresh connection. This does not test
  resuming a persistent desktop session. The server and
  TLS/credential artifacts are cleaned up. A preliminary `xfreerdp3` auth-only
  test also passed before this stronger test replaced it.

These are 17 automated tests across the three suites. The integration client
uses FreeRDP's software GDI without opening any X11 or Wayland display. Its
certificate-verification bypass is limited to the managed test's ephemeral
self-signed loopback certificate.

The sandbox initially denied Unix/network socket creation. Successful socket
tests were run with explicit execution permission outside the sandbox. Private
D-Bus uses a Unix socket; the RDP listener is loopback-only and temporary.

## Foundation limits (before the real bridge implementation)

No real Niri process, capture client, virtual keyboard/pointer bridge, PAM login
broker, or logind remote session was started. Niri is not installed on this host;
its behavior was investigated using pinned source. No Windows `mstsc.exe`
interoperability, physical monitor power-off, GPU/software rendering, resize,
remote account authentication, or same-UID desktop coexistence has been tested.
The proof of concept establishes transport/adapter and process-isolation
foundations, not completion of the target desktop.

The RDP prototype uses the **published** `ironrdp-server =0.13.0` and its lockfile.
The research snapshot is a later development revision with materially different
APIs despite the same manifest version. Its new credential/session binding APIs
must not be assumed to exist in the published crate used by the prototype.

## Reproduction

From the repository root:

```sh
python3 -m unittest discover -s tests -v
CARGO_HOME="$PWD/poc/.cargo-cache" cargo test --locked --offline --manifest-path poc/Cargo.toml
python3 poc/tests/startup.py
POC_RDP_BINARY="$PWD/poc/target/debug/niri-rdp-transport-poc" python3 -m unittest discover -s tests -v
```

The local Cargo cache is ignored. On a fresh checkout, dependency acquisition
must precede `--offline`; keep the Cargo home and target within the workspace.
See [PoC instructions](../poc/README.md) for explicit TLS setup, launch, and
integration commands. Default test discovery does not implicitly start an RDP
service.


## Dynamic resolution validation (2026-10-10)

The current bridge and maintained headless compositor patch support native
client-selected initial resolution and single-display live resize. The displayless
FreeRDP client negotiates 1001×701, then sends Display Control layouts for
702×1001 → 1002×701 → 702×1001 on the same connection. For every size, the harness
checks decoded frame geometry against Niri IPC's native and logical output size,
then requires application keyboard and center-pointer click/release feedback.
This verifies application reflow at native resolution as well as RDP geometry.

Three complete isolated TLS broker fixture suites passed: bitmap, NVENC AVC420,
and NVENC with a bitmap-only client. Receipts are
`artifacts/dynamic-resolution-bitmap.json`,
`artifacts/dynamic-resolution-nvenc.json`, and
`artifacts/dynamic-resolution-nvenc-fallback.json`. They also retain wrong-password
rejection, reconnect, held-input disconnect release, compositor-crash termination,
frontend survival and broker lease EOF checks. The last fallback run uses the
final disconnect responsiveness checks. The fixture supplies a real compositor
FD but does not authenticate through PAM.

All 27 bridge unit tests passed, including resize-before-pixels ordering,
cancellation, unsupported layouts, odd-size video decoding, encoder recreation,
and graphics surface reset. Both compositor headless regressions passed; the
maintained patch applies to the pristine pinned archive, and both debug binaries
build. Python regression discovery passed seven tests with the opt-in synthetic
transport test skipped. Socket tests and isolated integration required execution
outside the restrictive sandbox. No host service was installed or enabled.

Reproduce after building the patched compositor, bridge and strict FreeRDP probe:

```sh
cargo test --manifest-path bridge/Cargo.toml --locked --offline
lab/run-isolated.sh env NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/dynamic-resolution-bitmap.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/dynamic-resolution-nvenc.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_GFX=0 NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_REPORT=artifacts/dynamic-resolution-nvenc-fallback.json \
  python3 scripts/headless-integration.py
```

See the [resolution contract](../bridge/README.md#dynamic-resolution) for bounds
and rebuilding both packages. Windows mstsc, installed-service operation and
real PAM/logind boot acceptance remain separate-machine gates.

## Simplified Arch host installation (2026-10-10)

`packaging/arch/niri-rdp/PKGBUILD` builds the compositor, bridge, broker, and
setup helpers as one `niri-remote-login` package. Source checksum verification,
portable `makepkg --allsource` export and verification outside the checkout,
pinned compositor patch application, package assembly, and staged package
structure checks passed. The assembly check reused existing compiled binaries;
a complete fresh combined release build and actual pacman installation were not
performed. The legacy split-package source exporter also excludes makepkg's
generated trees and cached inputs.

Python discovery passed 13 tests, with the opt-in synthetic transport test
skipped. The broker's 10 mock-PAM test groups passed. The bridge Rust suite
passed 35 library tests plus one frontend test, including direct listener configuration, restricted credential-less
login binding, keyboard/focus handling, password masking, rejected login, and
stream cancellation. Staged systemd units passed `systemd-analyze verify`; only
executable paths were redirected to the extracted package for this check. TLS
tests use temporary files and exercise real OpenSSL generation, identity
preservation, private-key permissions, and incomplete/symlink identity rejection.

Fresh bridge and FreeRDP probe builds passed three isolated real-desktop suites:

- `artifacts/easy-install-direct-tls-integration.json`: direct ClientInfo
  credentials through the broker protocol to a real Niri desktop.
- `artifacts/easy-install-login-integration.json`: omitted ClientInfo credentials
  reach the login form, wrong form credentials are rejected, and physical US
  scancodes submit valid credentials before real desktop interaction.
- `artifacts/easy-install-login-nvenc-integration.json`: the same login sequence
  switches to actual NVIDIA NVENC AVC420 with client-decoded desktop frames.

All suites exercise real application frames/input, client-selected initial
resolution, native/live portrait and odd-height resize, reconnect, held-input
release, compositor failure, frontend survival, and closure of each accepted
broker lease. The namespace broker supplies a real compositor socket but uses
fixture credentials: these results establish RDP/login/desktop transport,
not installed Linux PAM or logind sessions.

Reproduce the login-screen suites after building the patched compositor, bridge,
and strict FreeRDP probe:

```sh
lab/run-isolated.sh env NIRI_RDP_LAB_TLS_CONTRACT=1 \
  NIRI_RDP_LAB_LOGIN_SCREEN=1 \
  NIRI_RDP_LAB_REPORT=artifacts/easy-install-login-integration.json \
  python3 scripts/headless-integration.py
lab/run-isolated.sh --nvenc env NIRI_RDP_LAB_ENCODER=nvenc \
  NIRI_RDP_LAB_TLS_CONTRACT=1 NIRI_RDP_LAB_LOGIN_SCREEN=1 \
  NIRI_RDP_LAB_REPORT=artifacts/easy-install-login-nvenc-integration.json \
  python3 scripts/headless-integration.py
```

Microsoft's [RDP negotiation request specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/902b090b-9cb3-4efc-92bf-ee13373371e3)
allows a client offering CredSSP to also offer TLS. The pinned acceptor selects
the shared TLS protocol and supplies ClientInfo credentials even when they are
empty. This supports the ordinary-client/login-screen design; it is not evidence
that a specific Windows build interoperates. Actual Windows mstsc, installed
PAM/logind boot login, and physical-monitor acceptance remain pending.

## Remote login graphics fixes (2026-10-11)

The rebuilt bridge passed eight login-related tests and sixteen video tests,
including static-form repaint, bitmap fallback without AVC, and delivery of the
first graphics surface while the client suppresses output during its handshake.
An isolated bitmap/TLS login-screen run passed wrong-password rejection,
authentication into a real headless Niri desktop, decoded frames, live resizing,
application input, disconnect releases, compositor failure, and broker lease
closure. Its report is `artifacts/remote-fixes-login-integration.json`.

The run used fixture credentials through the namespace broker, existing lab
compositor/client binaries, and FreeRDP libraries extracted into the local test
sysroot. It did not validate installed PAM/logind, an actual Windows mstsc
client, or hardware NVENC; no NVIDIA devices were available. Login and desktop
now share one graphics wrapper, so authentication at the negotiated desktop
size reuses the login surface and adds no surface initialization to the NVENC
integration fixture's existing count.

## Remote keyboard and desktop fixes (2026-10-11)

`make check` passed: 22 Python tests (one optional transport test skipped),
five transport unit tests, four startup checks, ten native broker groups, and
42 bridge tests. The worker's 14 host setup tests include host configuration
reuse, remote override precedence, private bar startup and duplicate detection.
Niri validated the user's existing configuration through the remote wrapper.

The rebuilt compositor and expanded FreeRDP fixture passed combined TLS login
and keyboard integration, recorded in
`artifacts/login-keyboard-regression-integration.json`. The Wayland application
decodes its actual advertised XKB keymap and requires consecutive `abc123ABc`,
Return, and a Meta+D compositor binding. The run also checks resize, capture,
disconnect release and compositor crash cleanup. The maintained patch applies
cleanly to the exact pinned source archive and its checksum matches `pins.json`.
Actual Windows mstsc, installed PAM/logind, the running DMS bar and hardware
NVENC remain machine checks; these results do not claim those checks passed.

Both headless compositor library regressions passed on the rebuilt source
(`cargo test --locked --offline --no-default-features --lib backend::headless`).

## Japanese layout and phone Unicode input (2026-10-11)

Validation passed: 47 bridge library tests plus one frontend test, 22 Python
tests (one optional transport test skipped), and the real-compositor RDP
integration using both debug and release frontends. The release binary's host
shared-library dependencies resolve successfully.

The remote bridge now uses `evdev` / `jp106` / `jp`, matching the host's
`/etc/X11/xorg.conf.d/00-keyboard.conf`. Login punctuation uses the same JIS
positions. Japanese Ro, Yen, Hiragana/Katakana, Henkan and Muhenkan scan codes
map to their Linux evdev keys.

The FreeRDP fixture sends physical `abc123ABc`, JIS punctuation, Ro/Yen and
conversion keys, then phone-style Unicode presses/releases containing Japanese
and emoji with interleaved UTF-16 surrogate releases. The real Wayland fixture
decodes `@"^&:[]` plus two backslashes and `Phone@"日本😀Aa@` exactly. This
also exercises repeated Unicode presses, Backspace, Unicode Enter, Shift held
across text injection, resumed physical typing, reconnect and disconnect
release. Results: `artifacts/jp106-phone-keyboard-integration.json`.

Unicode uses balanced taps through a temporary one-level keymap; the physical
JP106 keymap and modifiers are restored after every scalar. Mobile IME
composition happens in the client; remote IME composition is outside this
change. Actual phone app and Windows hardware/client checks still require the
user's devices. The automated test verifies both RDP event paths through the
compositor into an application, rather than merely checking server receipt.

Build the frontend using the repository's cached dependencies:

```sh
CARGO_HOME="$PWD/poc/.cargo-cache" cargo build --release --locked --offline --manifest-path bridge/Cargo.toml
```

System deployment requires root. An installed frontend continues using the
old binary until it is updated and restarted. The package rebuild workflow in
`docs/upgrade.md` includes the new `bridge/src/keyboard.rs` automatically.

## Host keyboard and input-method inheritance (2026-10-12)

Installed-host investigation found `jp` / `jp106` still saved in localed's
configuration. The user's usual Niri `xkb` section was empty. The packaged
headless build disables desktop D-Bus integration, including the locale1
keyboard watcher, so it had fallen back to US. A remote session also rewrote
the shared Fcitx profile. The later login attempt had no authenticated session
or PAM failure counter; this does not prove which typed character failed.

The environment helper now exports saved host XKB defaults before both the
remote compositor and frontend start. The login form uses XKB rather than a
fixed ASCII table. Configured Fcitx/IBus starts before applications, with a
private Fcitx profile snapshot and private IBus address/discovery file.

Regression checks cover localed parsing and user precedence; inherited JP106
punctuation, Ro and Yen in a real headless seat at startup and reload; explicit
Swedish override; login JP106, US, Swedish, AltGr, keypad and Unicode handling;
private bus activation; input-method selection, disabled overrides, duplicate
startup suppression and isolation of host profile writes. The maintained patch
applies to the pinned source archive, and its hash matches `pins.json`.

Release 12's package checks passed: 67 bridge tests, three headless compositor
tests, 12 native broker groups and 52 Python tests (one optional transport test
skipped). The isolated RDP login integration also authenticated a disposable
password containing JIS punctuation through physical scancodes and Shift events.
An isolated Fcitx startup loaded Mozc and the native Wayland input method, reused
one daemon on repeated startup and preserved the source profile through teardown.

Windows 10 mstsc and a physical Japanese keyboard require a fresh login on the
updated installed host for final confirmation. The automated checks do not
claim that device test has passed.

## NVIDIA device preparation after reboot (2026-10-12)

The installed headless host repeatedly failed the NVENC preflight with
`CUDA_ERROR_NO_DEVICE` after reboot, despite a loaded NVIDIA driver and the
correct GPU-access drop-in. Running `nvidia-smi` created `/dev/nvidia0` and
`/dev/nvidiactl` after the failed starts. A one-frame FFmpeg NVENC test then
passed with `setpriv --no-new-privs`, consistent with missing device nodes being
the startup blocker. This observation does not prove the repaired boot path.

Release 13 packages `niri-rdp-nvidia-devices.service` and makes the frontend
wait for it through `Wants` and `After`. The root helper prepares base nodes and
UVM nodes in separate commands, skips machines without a loaded NVIDIA GPU,
and finishes without remaining active so it can run on later frontend starts.
The frontend retains its unprivileged account and hardening. Existing NVIDIA
drop-ins do not need a new dependency or a separately enabled helper.

Validation passed: 62 Python regressions (two optional tests skipped), including
startup-order and device-preparation regressions; the combined release 13
`makepkg` build and checks; package structure/preservation validation; and
`systemd-analyze verify` on the packaged helper, frontend and broker units.
The package checks include 67 bridge tests, the headless compositor tests,
native broker checks and 54 Python tests (one optional hardware test skipped).
The release source bundle also includes the helper. Logs and extracted package
evidence are under `artifacts/nvidia-boot-fix/`.

Sandbox socket restrictions initially blocked private D-Bus/audio and Rust IPC
tests; rerunning the checks outside the sandbox passed. No package installation,
service restart or reboot was performed during this packaging change.
Installed NVIDIA cold-boot validation remains pending in
[acceptance.md](acceptance.md), and must precede any `nvidia-smi` invocation that
could create the missing nodes itself.
