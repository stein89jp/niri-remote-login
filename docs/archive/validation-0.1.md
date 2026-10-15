# Validation record: 0.1.0 foundations (archived)

This archive keeps the 0.1.0-era foundation and early host records that were
moved out of the [current validation record](../validation.md) on 2026-10-15.
They describe the proof of concept, the first packaged builds (`0.1.0-1` to about
`0.1.0-13`) and their limits at that time; later releases superseded many of
those limits. Commands and paths are kept as originally recorded and may no
longer match the current tree (for example, the NVIDIA device-preparation and
keyboard sections predate the Niri rebase). See the
[release history](../upgrade.md#release-history) for how "release N" maps to
package versions, and [acceptance](../acceptance.md) for the current gates.

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
See [PoC instructions](../../poc/README.md) for explicit TLS setup, launch, and
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

See the [resolution contract](../../bridge/README.md#dynamic-resolution) for bounds
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
[acceptance.md](../acceptance.md), and must precede any `nvidia-smi` invocation that
could create the missing nodes itself.
